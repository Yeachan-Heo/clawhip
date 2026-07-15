use std::collections::HashSet;
use std::net::IpAddr;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Map, Value};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};

use crate::Result;
use crate::config::{SubscriptionConfig, SubscriptionRoutingConfig};
use crate::events::{IncomingEvent, RoutingMetadata};

fn subscription_error(reason: &'static str) -> crate::DynError {
    Box::new(std::io::Error::other(reason))
}

const SENSITIVE_KEYS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "authorization",
    "cookie",
    "credential",
    "credentials",
    "api_key",
    "apikey",
    "access_token",
    "refresh_token",
    "private_key",
    "client_secret",
    "endpoint",
    "url",
    "uri",
    "webhook",
];
const RESERVED_KEYS: &[&str] = &[
    "channel",
    "mention",
    "format",
    "template",
    "type",
    "kind",
    "event",
    "event_id",
    "correlation_id",
    "first_seen_at",
    "raw_event",
    "contract_event",
    "event_timestamp",
    "timestamp",
    "observed_at",
    "created_at",
    "tool",
    "project",
    "repo",
    "repo_name",
    "repo_path",
    "worktree",
    "worktree_path",
    "directory",
    "session",
    "session_name",
    "session_id",
    "branch",
    "provider",
    "source",
    "subscription_name",
    "subscription_transport",
    "subscription_received_at",
    "ingress_source",
    "_clawhip",
    "clawhip",
    "routing",
    "metadata",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionState {
    Disabled,
    Stopped,
    Starting,
    Connecting,
    Connected,
    BackingOff,
    Exhausted,
    Stopping,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SubscriptionSnapshot {
    pub schema: &'static str,
    pub name: String,
    pub enabled: bool,
    pub state: SubscriptionState,
    pub desired_running: bool,
    pub started_at: Option<String>,
    pub last_connected_at: Option<String>,
    pub last_event_enqueued_at: Option<String>,
    pub last_state_transition_at: String,
    pub connection_attempts_total: u64,
    pub connection_failures_total: u64,
    pub reconnects_total: u64,
    pub frames_received_total: u64,
    pub frames_rejected_total: u64,
    pub frames_matched_total: u64,
    pub adapters_started_total: u64,
    pub adapters_rejected_total: u64,
    pub events_enqueued_total: u64,
    pub last_reason_code: &'static str,
}

impl SubscriptionSnapshot {
    pub fn new(config: &SubscriptionConfig) -> Self {
        Self {
            schema: "clawhip.subscription.v1",
            name: config.name.clone(),
            enabled: config.enabled,
            state: if config.enabled {
                SubscriptionState::Stopped
            } else {
                SubscriptionState::Disabled
            },
            desired_running: config.enabled,
            started_at: None,
            last_connected_at: None,
            last_event_enqueued_at: None,
            last_state_transition_at: subscription_timestamp(),
            connection_attempts_total: 0,
            connection_failures_total: 0,
            reconnects_total: 0,
            frames_received_total: 0,
            frames_rejected_total: 0,
            frames_matched_total: 0,
            adapters_started_total: 0,
            adapters_rejected_total: 0,
            events_enqueued_total: 0,
            last_reason_code: if config.enabled {
                "start_requested"
            } else {
                "configured_disabled"
            },
        }
    }

    pub fn transition(&mut self, state: SubscriptionState, reason: &'static str) {
        self.state = state;
        self.last_reason_code = reason;
        self.last_state_transition_at = subscription_timestamp();
    }

    pub fn mark_started(&mut self) {
        self.started_at = Some(subscription_timestamp());
    }

    fn connection_attempt(&mut self) {
        self.connection_attempts_total = self.connection_attempts_total.saturating_add(1);
    }

    fn connection_failed(&mut self) {
        self.connection_failures_total = self.connection_failures_total.saturating_add(1);
    }
}

fn subscription_timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub fn validate_endpoint(endpoint: &str) -> Result<()> {
    if endpoint.len() > 4096 {
        return Err(subscription_error("endpoint_unavailable"));
    }
    let url = Url::parse(endpoint).map_err(|_| subscription_error("endpoint_unavailable"))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(subscription_error("endpoint_unavailable"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| subscription_error("endpoint_unavailable"))?;
    match url.scheme() {
        "wss" => Ok(()),
        "ws" => {
            let ip: IpAddr = host
                .parse()
                .map_err(|_| subscription_error("endpoint_unavailable"))?;
            if ip.is_loopback() {
                Ok(())
            } else {
                Err(subscription_error("endpoint_unavailable"))
            }
        }
        _ => Err(subscription_error("endpoint_unavailable")),
    }
}

pub fn parse_pointer(pointer: &str) -> Result<Vec<String>> {
    if pointer.is_empty() || !pointer.starts_with('/') || pointer.len() > 256 {
        return Err(subscription_error("invalid_subscription_pointer"));
    }
    let parts: Vec<String> = pointer[1..]
        .split('/')
        .map(|part| {
            let mut decoded = String::new();
            let mut chars = part.chars();
            while let Some(ch) = chars.next() {
                if ch == '~' {
                    match chars.next() {
                        Some('0') => decoded.push('~'),
                        Some('1') => decoded.push('/'),
                        _ => return Err(subscription_error("invalid_subscription_pointer")),
                    }
                } else {
                    decoded.push(ch);
                }
            }
            Ok(decoded)
        })
        .collect::<Result<_>>()?;
    if parts.len() > 8 {
        return Err(subscription_error("invalid_subscription_pointer"));
    }
    Ok(parts)
}

fn value_at<'a>(value: &'a Value, pointer: &str) -> Result<&'a Value> {
    let mut current = value;
    for part in parse_pointer(pointer)? {
        current = current
            .get(&part)
            .ok_or_else(|| subscription_error("projection_rejected"))?;
    }
    Ok(current)
}

fn forbidden_key(key: &str) -> bool {
    let components: Vec<String> = key
        .split(|character: char| !character.is_ascii_alphanumeric())
        .flat_map(|component| {
            let mut components = Vec::new();
            let mut current = String::new();
            for character in component.chars() {
                if character.is_ascii_uppercase() && !current.is_empty() {
                    components.push(std::mem::take(&mut current));
                }
                current.extend(character.to_lowercase());
            }
            if !current.is_empty() {
                components.push(current);
            }
            components
        })
        .collect();
    let canonical = components.join("_");
    SENSITIVE_KEYS.contains(&canonical.as_str())
        || RESERVED_KEYS.contains(&canonical.as_str())
        || components.iter().any(|component| {
            SENSITIVE_KEYS.contains(&component.as_str())
                || RESERVED_KEYS.contains(&component.as_str())
        })
}

fn validate_filter_pointer(pointer: &str) -> Result<()> {
    parse_pointer(pointer).map(|_| ())
}

pub fn project_frame(config: &SubscriptionConfig, frame: &str) -> Result<Option<Vec<u8>>> {
    if frame.len() > config.max_frame_bytes {
        return Err(subscription_error("frame_oversized"));
    }
    let value: Value =
        serde_json::from_str(frame).map_err(|_| subscription_error("frame_malformed"))?;
    if !value.is_object() || json_depth(&value) > config.max_json_depth {
        return Err(subscription_error("frame_depth_exceeded"));
    }
    let filter = &config.filter;
    if value_at(&value, &filter.discriminator_pointer)?
        != &Value::String(filter.discriminator_equals.clone())
    {
        return Ok(None);
    }
    for predicate in &filter.predicates {
        if value_at(&value, &predicate.pointer)? != &Value::String(predicate.equals.clone()) {
            return Ok(None);
        }
    }
    let mut projection = Map::new();
    for (name, pointer) in &config.projection {
        let selected = value_at(&value, pointer)?;
        if selected.is_null()
            || !selected.is_string() && !selected.is_number() && !selected.is_boolean()
        {
            return Err(subscription_error("projection_rejected"));
        }
        projection.insert(name.clone(), selected.clone());
    }
    let bytes = serde_json::to_vec(&Value::Object(projection))?;
    if bytes.len() > config.adapter.max_stdin_bytes {
        return Err(subscription_error("projection_rejected"));
    }
    Ok(Some(bytes))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestrictedAdapterEvent {
    #[serde(rename = "type")]
    kind: String,
    payload: Value,
}

pub fn adapter_event(stdout: &[u8], config: &SubscriptionConfig) -> Result<IncomingEvent> {
    if stdout.is_empty() || stdout.len() > config.adapter.max_stdout_bytes {
        return Err(subscription_error("adapter_invalid_output"));
    }
    let mut de = serde_json::Deserializer::from_slice(stdout);
    let output = RestrictedAdapterEvent::deserialize(&mut de)
        .map_err(|_| subscription_error("adapter_invalid_output"))?;
    de.end()
        .map_err(|_| subscription_error("adapter_invalid_output"))?;
    if !valid_event_kind(&output.kind)
        || !output.payload.is_object()
        || json_depth(&output.payload) > 8
        || !safe_payload(&output.payload)
    {
        return Err(subscription_error("adapter_reserved_field"));
    }
    Ok(IncomingEvent {
        kind: output.kind,
        channel: None,
        mention: None,
        format: None,
        template: None,
        payload: output.payload,
    })
}

fn valid_event_kind(kind: &str) -> bool {
    if kind.is_empty() || kind.len() > 96 || kind.contains("://") || kind.contains('@') {
        return false;
    }
    let mut last_separator = true;
    for byte in kind.bytes() {
        let is_alnum = byte.is_ascii_lowercase() || byte.is_ascii_digit();
        if !is_alnum && byte != b'.' && byte != b'-' {
            return false;
        }
        if byte == b'.' || byte == b'-' {
            if last_separator {
                return false;
            }
            last_separator = true;
        } else {
            last_separator = false;
        }
    }
    !last_separator && kind.as_bytes()[0].is_ascii_lowercase()
}
fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(values) => 1 + values.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(values) => 1 + values.values().map(json_depth).max().unwrap_or(0),
        _ => 0,
    }
}
fn safe_payload(value: &Value) -> bool {
    match value {
        Value::Object(values) => {
            values.len() <= 32
                && values
                    .iter()
                    .all(|(k, v)| !forbidden_key(k) && safe_payload(v))
        }
        Value::Array(values) => values.iter().all(safe_payload),
        _ => true,
    }
}

async fn run_adapter(
    config: &SubscriptionConfig,
    input: &[u8],
    cancel: &mut watch::Receiver<bool>,
) -> Result<IncomingEvent> {
    let deadline = Instant::now() + Duration::from_millis(config.adapter.timeout_ms);
    let mut command = Command::new(&config.adapter.program);
    command
        .args(&config.adapter.args)
        .env_clear()
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| subscription_error("adapter_spawn_failed"))?;
    if Instant::now() >= deadline {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(subscription_error("adapter_timeout"));
    }
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| subscription_error("adapter_stdin_failed"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| subscription_error("adapter_invalid_output"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| subscription_error("adapter_invalid_output"))?;
    let stdout_cap = config.adapter.max_stdout_bytes + 1;
    let stderr_cap = config.adapter.max_stderr_bytes + 1;
    let mut stdout_task = tokio::spawn(async move {
        let mut output = Vec::new();
        stdout
            .take(stdout_cap as u64)
            .read_to_end(&mut output)
            .await
            .map(|_| output)
    });
    let mut stderr_task = tokio::spawn(async move {
        let mut output = Vec::new();
        stderr
            .take(stderr_cap as u64)
            .read_to_end(&mut output)
            .await
            .map(|_| output)
    });
    let result = tokio::select! {
        result = async {
            stdin.write_all(input).await.map_err(|_| subscription_error("adapter_stdin_failed"))?;
            stdin.shutdown().await.map_err(|_| subscription_error("adapter_stdin_failed"))?;
            let status = child.wait().await.map_err(|_| subscription_error("adapter_invalid_output"))?;
            let output = (&mut stdout_task)
                .await
                .map_err(|_| subscription_error("adapter_invalid_output"))?
                .map_err(|_| subscription_error("adapter_invalid_output"))?;
            let stderr = (&mut stderr_task)
                .await
                .map_err(|_| subscription_error("adapter_invalid_output"))?
                .map_err(|_| subscription_error("adapter_invalid_output"))?;
            Ok((status, output, stderr))
        } => result,
        _ = tokio::time::sleep_until(deadline) => Err(subscription_error("adapter_timeout")),
        _ = cancel.changed() => Err(subscription_error("cancelled")),
    };
    let (status, output, stderr) = match result {
        Ok(result) => result,
        Err(error) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(error);
        }
    };
    if stderr.len() > config.adapter.max_stderr_bytes {
        return Err(subscription_error("adapter_stderr_oversized"));
    }
    if output.len() > config.adapter.max_stdout_bytes {
        return Err(subscription_error("adapter_stdout_oversized"));
    }
    if !status.success() {
        return Err(subscription_error("adapter_nonzero_exit"));
    }
    adapter_event(&output, config)
}

pub struct SubscriptionWorker {
    pub config: Arc<SubscriptionConfig>,
    pub snapshot: Arc<Mutex<SubscriptionSnapshot>>,
    pub cancel: watch::Receiver<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconnectDecision {
    Backoff,
    Exhausted,
}

fn reconnect_delay_ms(config: &SubscriptionConfig, attempts: u64) -> u64 {
    let multiplier = 1u64
        .checked_shl(attempts.saturating_sub(1).min(63) as u32)
        .unwrap_or(u64::MAX);
    config
        .reconnect
        .initial_delay_ms
        .saturating_mul(multiplier)
        .min(config.reconnect.max_delay_ms)
}

fn reconnect_decision(attempts: u64, max_attempts: u64) -> ReconnectDecision {
    if attempts >= max_attempts {
        ReconnectDecision::Exhausted
    } else {
        ReconnectDecision::Backoff
    }
}

fn safe_reject_reason(error: &(dyn std::error::Error + Send + Sync)) -> &'static str {
    match error.to_string().as_str() {
        "frame_oversized" => "frame_oversized",
        "frame_malformed" => "frame_malformed",
        "frame_depth_exceeded" => "frame_depth_exceeded",
        "projection_rejected" => "projection_rejected",
        "adapter_stderr_oversized" => "adapter_stderr_oversized",
        "adapter_stdout_oversized" => "adapter_stdout_oversized",
        "adapter_nonzero_exit" => "adapter_nonzero_exit",
        "adapter_invalid_output" => "adapter_invalid_output",
        "adapter_reserved_field" => "adapter_reserved_field",
        _ => "adapter_rejected",
    }
}

fn socket_error_reason(error: &tokio_tungstenite::tungstenite::Error) -> &'static str {
    match error {
        tokio_tungstenite::tungstenite::Error::Capacity(_) => "frame_oversized",
        _ => "protocol_error",
    }
}

impl SubscriptionWorker {
    async fn stop(&self) {
        self.snapshot
            .lock()
            .await
            .transition(SubscriptionState::Stopped, "cancelled");
    }

    async fn reject_frame(&self, reason: &'static str) {
        let mut snapshot = self.snapshot.lock().await;
        snapshot.frames_rejected_total = snapshot.frames_rejected_total.saturating_add(1);
        snapshot.transition(SubscriptionState::Connected, reason);
    }

    async fn reject_adapter(&self, reason: &'static str) {
        let mut snapshot = self.snapshot.lock().await;
        snapshot.adapters_rejected_total = snapshot.adapters_rejected_total.saturating_add(1);
        snapshot.transition(SubscriptionState::Connected, reason);
    }

    async fn reconnect_or_exhaust(&self, attempts: u64, reason: &'static str) -> bool {
        let mut snapshot = self.snapshot.lock().await;
        snapshot.connection_failed();
        match reconnect_decision(attempts, self.config.reconnect.max_attempts) {
            ReconnectDecision::Exhausted => {
                snapshot.transition(SubscriptionState::Exhausted, "retry_exhausted");
                true
            }
            ReconnectDecision::Backoff => {
                snapshot.transition(SubscriptionState::BackingOff, reason);
                false
            }
        }
    }

    async fn wait_to_reconnect(&mut self, attempts: u64) -> bool {
        let delay = reconnect_delay_ms(&self.config, attempts);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {
                self.snapshot.lock().await.transition(SubscriptionState::Connecting, "reconnect_requested");
                false
            }
            _ = self.cancel.changed() => true,
        }
    }

    pub async fn run(mut self, tx: mpsc::Sender<IncomingEvent>) -> Result<()> {
        if *self.cancel.borrow() {
            self.stop().await;
            return Ok(());
        }
        let endpoint = match std::env::var(&self.config.endpoint_env) {
            Ok(endpoint) if validate_endpoint(&endpoint).is_ok() => endpoint,
            _ => {
                self.snapshot
                    .lock()
                    .await
                    .transition(SubscriptionState::Exhausted, "endpoint_unavailable");
                return Err(subscription_error("endpoint_unavailable"));
            }
        };
        let mut attempts = 0u64;
        let mut has_attempted = false;
        loop {
            if *self.cancel.borrow() {
                self.stop().await;
                return Ok(());
            }
            attempts = attempts.saturating_add(1);
            {
                let mut snapshot = self.snapshot.lock().await;
                snapshot.connection_attempt();
                if has_attempted {
                    snapshot.reconnects_total = snapshot.reconnects_total.saturating_add(1);
                }
                has_attempted = true;
                snapshot.transition(SubscriptionState::Connecting, "connecting");
            }
            let ws_config = WebSocketConfig::default()
                .max_message_size(Some(self.config.max_frame_bytes))
                .max_frame_size(Some(self.config.max_frame_bytes));
            let connect = tokio_tungstenite::connect_async_with_config(
                endpoint.as_str(),
                Some(ws_config),
                false,
            );
            let stream = match tokio::select! { result = connect => result.map(|stream| stream.0), _ = self.cancel.changed() => { self.stop().await; return Ok(()); } }
            {
                Ok(stream) => stream,
                Err(_) => {
                    if self.reconnect_or_exhaust(attempts, "connect_failed").await {
                        return Err(subscription_error("retry_exhausted"));
                    }
                    if self.wait_to_reconnect(attempts).await {
                        self.stop().await;
                        return Ok(());
                    }
                    continue;
                }
            };
            {
                let mut snapshot = self.snapshot.lock().await;
                snapshot.last_connected_at = Some(subscription_timestamp());
                snapshot.transition(SubscriptionState::Connected, "connected");
            }
            let (_, mut reader) = stream.split();
            let mut reconnect_reason = "peer_closed";
            loop {
                let message = tokio::select! { message = reader.next() => message, _ = self.cancel.changed() => { self.stop().await; return Ok(()); } };
                let Some(message) = message else { break };
                let message = match message {
                    Ok(message) => message,
                    Err(error) => {
                        reconnect_reason = socket_error_reason(&error);
                        break;
                    }
                };
                match message {
                    Message::Text(text) => {
                        {
                            let mut snapshot = self.snapshot.lock().await;
                            snapshot.frames_received_total =
                                snapshot.frames_received_total.saturating_add(1);
                        }
                        let input = match project_frame(&self.config, &text) {
                            Ok(Some(input)) => {
                                let mut snapshot = self.snapshot.lock().await;
                                snapshot.frames_matched_total =
                                    snapshot.frames_matched_total.saturating_add(1);
                                input
                            }
                            Ok(None) => {
                                self.reject_frame("frame_not_matched").await;
                                continue;
                            }
                            Err(error) => {
                                self.reject_frame(safe_reject_reason(error.as_ref())).await;
                                continue;
                            }
                        };
                        {
                            let mut snapshot = self.snapshot.lock().await;
                            snapshot.adapters_started_total =
                                snapshot.adapters_started_total.saturating_add(1);
                        }
                        let event = match run_adapter(&self.config, &input, &mut self.cancel).await
                        {
                            Ok(event) => event,
                            Err(_) if *self.cancel.borrow() => {
                                self.stop().await;
                                return Ok(());
                            }
                            Err(error) => {
                                self.reject_adapter(safe_reject_reason(error.as_ref()))
                                    .await;
                                continue;
                            }
                        };
                        let event = IncomingEvent::subscription(
                            event.kind,
                            event.payload,
                            &subscription_routing(&self.config.routing),
                            &self.config.name,
                        );
                        match tokio::select! { sent = tx.send(event) => sent, _ = self.cancel.changed() => { self.stop().await; return Ok(()); } }
                        {
                            Ok(()) => {
                                let mut snapshot = self.snapshot.lock().await;
                                snapshot.events_enqueued_total =
                                    snapshot.events_enqueued_total.saturating_add(1);
                                snapshot.last_event_enqueued_at = Some(subscription_timestamp());
                                attempts = 0;
                            }
                            Err(_) => {
                                self.snapshot
                                    .lock()
                                    .await
                                    .transition(SubscriptionState::Exhausted, "queue_closed");
                                return Err(subscription_error("queue_closed"));
                            }
                        }
                    }
                    Message::Binary(_) => {
                        {
                            let mut snapshot = self.snapshot.lock().await;
                            snapshot.frames_received_total =
                                snapshot.frames_received_total.saturating_add(1);
                        }
                        self.reject_frame("binary_frame_rejected").await;
                    }
                    Message::Close(_) => {
                        reconnect_reason = "peer_closed";
                        break;
                    }
                    _ => {}
                }
            }
            if self.reconnect_or_exhaust(attempts, reconnect_reason).await {
                return Err(subscription_error("retry_exhausted"));
            }
            if self.wait_to_reconnect(attempts).await {
                self.stop().await;
                return Ok(());
            }
        }
    }
}

pub fn validate_filter_policy(config: &SubscriptionConfig) -> Result<()> {
    validate_filter_pointer(&config.filter.discriminator_pointer)?;
    for predicate in &config.filter.predicates {
        validate_filter_pointer(&predicate.pointer)?;
    }
    Ok(())
}

pub fn validate_projection_policy(config: &SubscriptionConfig) -> Result<()> {
    fn valid_projection_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name.as_bytes()[0].is_ascii_lowercase()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    }

    let mut names = HashSet::new();
    for (name, pointer) in &config.projection {
        if !valid_projection_name(name)
            || forbidden_key(name)
            || !names.insert(name.to_ascii_lowercase())
        {
            return Err(subscription_error("invalid_subscription_projection"));
        }
        if parse_pointer(pointer)?
            .iter()
            .any(|part| forbidden_key(part))
        {
            return Err(subscription_error("invalid_subscription_pointer"));
        }
    }
    Ok(())
}

pub fn subscription_routing(routing: &SubscriptionRoutingConfig) -> RoutingMetadata {
    RoutingMetadata {
        tool: routing.tool.clone(),
        project: routing.project.clone(),
        repo_name: routing.repo_name.clone(),
        repo_path: routing.repo_path.clone(),
        worktree_path: routing.worktree_path.clone(),
        session_id: routing.session_id.clone(),
        branch: routing.branch.clone(),
    }
}

pub fn is_regular_executable(path: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute()
        && std::fs::metadata(path)
            .map(|metadata| metadata.is_file() && has_execute_permission(&metadata))
            .unwrap_or(false)
}

#[cfg(unix)]
fn has_execute_permission(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn has_execute_permission(_: &std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        SubscriptionAdapterConfig, SubscriptionFilterConfig, SubscriptionPredicateConfig,
        SubscriptionReconnectConfig,
    };
    use std::collections::BTreeMap;

    fn config() -> SubscriptionConfig {
        SubscriptionConfig {
            name: "gjc-workflow-gate".into(),
            enabled: true,
            kind: "websocket".into(),
            endpoint_env: "GJC_WS_URL".into(),
            max_frame_bytes: 65_536,
            max_json_depth: 16,
            filter: SubscriptionFilterConfig {
                discriminator_pointer: "/type".into(),
                discriminator_equals: "workflow_gate".into(),
                predicates: vec![SubscriptionPredicateConfig {
                    pointer: "/gate/state".into(),
                    equals: "ready".into(),
                }],
            },
            projection: BTreeMap::from([
                (String::from("workflow_id"), String::from("/workflow/id")),
                (String::from("gate_state"), String::from("/gate/state")),
            ]),
            adapter: SubscriptionAdapterConfig {
                program: "/bin/true".into(),
                args: vec!["--literal;$(not-a-shell)".into()],
                timeout_ms: 5_000,
                max_stdin_bytes: 16_384,
                max_stdout_bytes: 16_384,
                max_stderr_bytes: 4_096,
            },
            reconnect: SubscriptionReconnectConfig::default(),
            routing: SubscriptionRoutingConfig::default(),
        }
    }

    #[test]
    fn projects_matching_gjc_workflow_gate_but_not_questions() {
        let config = config();
        let frame = r#"{"type":"workflow_gate","workflow":{"id":"wf-1"},"gate":{"state":"ready"},"secret":"never"}"#;
        assert_eq!(
            project_frame(&config, frame).unwrap().unwrap(),
            br#"{"gate_state":"ready","workflow_id":"wf-1"}"#
        );
        assert!(
            project_frame(&config, r#"{"type":"question","gate":{"state":"ready"}}"#)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_reserved_projection_and_adapter_payload() {
        let mut invalid_config = config();
        invalid_config
            .projection
            .insert("token".into(), "/workflow/id".into());
        assert!(validate_projection_policy(&invalid_config).is_err());
        let config = config();
        assert!(
            adapter_event(
                br#"{"type":"workflow.gate","payload":{"correlation_id":"secret"}}"#,
                &config
            )
            .is_err()
        );
        assert!(
            adapter_event(
                br#"{"type":"workflow.gate","payload":{"nested":{"token":"secret"}}}"#,
                &config
            )
            .is_err()
        );
    }

    #[test]
    fn filter_pointers_allow_discriminators_and_predicates_while_projections_remain_redacted() {
        let config = config();
        assert!(validate_filter_policy(&config).is_ok());
        assert!(validate_projection_policy(&config).is_ok());

        let mut sensitive_projection = config.clone();
        sensitive_projection
            .projection
            .insert("safe_name".into(), "/credentials/token".into());
        assert!(validate_projection_policy(&sensitive_projection).is_err());
    }

    #[test]
    fn validates_redacted_transport_and_strict_output() {
        assert!(validate_endpoint("wss://example.invalid/path?token=secret").is_ok());
        assert!(validate_endpoint("ws://localhost:9000").is_err());
        assert!(validate_endpoint("wss://user:secret@example.invalid").is_err());
        let config = config();
        assert!(
            adapter_event(
                br#"{"type":"workflow.gate","payload":{}} {"type":"workflow.gate","payload":{}}"#,
                &config
            )
            .is_err()
        );
        assert!(adapter_event(br#"{"type":"Workflow.Gate","payload":{}}"#, &config).is_err());
    }
    #[test]
    fn snapshot_transitions_and_counters_are_deterministic() {
        let config = config();
        let mut snapshot = SubscriptionSnapshot::new(&config);
        let initial_transition = snapshot.last_state_transition_at.clone();
        snapshot.connection_attempt();
        snapshot.connection_failed();
        snapshot.transition(SubscriptionState::BackingOff, "connect_failed");
        assert_eq!(snapshot.connection_attempts_total, 1);
        assert_eq!(snapshot.connection_failures_total, 1);
        assert_eq!(snapshot.state, SubscriptionState::BackingOff);
        assert_eq!(snapshot.last_reason_code, "connect_failed");
        assert!(OffsetDateTime::parse(&initial_transition, &Rfc3339).is_ok());
        assert!(OffsetDateTime::parse(&snapshot.last_state_transition_at, &Rfc3339).is_ok());
    }

    #[test]
    fn reconnect_helpers_preserve_attempts_until_an_event_is_enqueued() {
        let config = config();

        assert_eq!(
            [1, 2, 3, 4, 5, 6]
                .into_iter()
                .map(|attempt| reconnect_delay_ms(&config, attempt))
                .collect::<Vec<_>>(),
            vec![250, 500, 1_000, 2_000, 4_000, 5_000]
        );
        assert_eq!(
            reconnect_decision(4, config.reconnect.max_attempts),
            ReconnectDecision::Backoff
        );
        assert_eq!(
            reconnect_decision(5, config.reconnect.max_attempts),
            ReconnectDecision::Exhausted
        );
    }

    #[test]
    fn rejects_decorated_sensitive_keys_at_every_payload_depth() {
        let config = config();
        for key in [
            "github_token",
            "auth_token",
            "accessToken",
            "credentials",
            "webhook_url",
        ] {
            let output = format!(
                r#"{{"type":"workflow.gate","payload":{{"nested":{{"{key}":"secret"}}}}}}"#
            );
            assert!(adapter_event(output.as_bytes(), &config).is_err(), "{key}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn executable_check_requires_an_executable_regular_file() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("adapter");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        assert!(!is_regular_executable(path.to_str().unwrap()));
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions).unwrap();
        assert!(is_regular_executable(path.to_str().unwrap()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn adapter_deadline_kills_and_reaps_a_non_reading_child() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("non-reading-adapter");
        let pid_file = directory.path().join("adapter.pid");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > {}\nexec sleep 30\n",
                pid_file.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();
        let mut config = config();
        config.adapter.program = script.to_string_lossy().into_owned();
        config.adapter.timeout_ms = 100;
        let (_sender, mut cancel) = watch::channel(false);
        let error = run_adapter(&config, br#"{"workflow_id":"wf-1"}"#, &mut cancel)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "adapter_timeout");
        let pid = std::fs::read_to_string(pid_file).unwrap();
        let process_exists = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .unwrap()
            .success();
        assert!(!process_exists, "timed-out adapter must be reaped");
    }
}
