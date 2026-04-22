use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Deserialize;
use serde_json::json;

use crate::Result;
use crate::binding_verify::ChannelLookup;
use crate::config::AppConfig;
use crate::core::circuit_breaker::CircuitBreaker;
use crate::core::dlq::{Dlq, DlqEntry};
use crate::core::rate_limit::RateLimiter;
use crate::sink::{SinkMessage, SinkTarget};
use crate::telemetry;

const MAX_ATTEMPTS: u32 = 3;
const JITTER_MS: u64 = 50;
const CIRCUIT_FAILURE_THRESHOLD: u32 = 3;
const CIRCUIT_COOLDOWN_SECS: u64 = 5;
const RATE_LIMIT_CAPACITY: u32 = 5;
const RATE_LIMIT_REFILL_PER_SEC: f64 = 5.0;

#[derive(Clone)]
pub struct DiscordClient {
    bot_client: Option<reqwest::Client>,
    webhook_client: reqwest::Client,
    api_base: String,
    state: Arc<Mutex<DiscordState>>,
}

#[derive(Debug)]
struct DiscordState {
    limiter: RateLimiter,
    circuits: HashMap<String, CircuitBreaker>,
    dlq: Dlq,
}

#[derive(Debug)]
struct DiscordSendError {
    message: String,
    retry_after: Option<Duration>,
    status: Option<u16>,
}

#[derive(Debug, Deserialize)]
struct DiscordRateLimitBody {
    retry_after: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct DiscordChannelBody {
    #[serde(default)]
    name: Option<String>,
}

impl DiscordClient {
    pub fn from_config(config: Arc<AppConfig>) -> Result<Self> {
        let bot_client = if let Some(token) = config.effective_token() {
            let mut headers = HeaderMap::new();
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bot {token}"))?,
            );
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

            Some(
                reqwest::Client::builder()
                    .default_headers(headers)
                    .build()?,
            )
        } else {
            None
        };
        let api_base = std::env::var("CLAWHIP_DISCORD_API_BASE")
            .unwrap_or_else(|_| "https://discord.com/api/v10".to_string());
        let webhook_client = reqwest::Client::new();

        Ok(Self {
            bot_client,
            webhook_client,
            api_base,
            state: Arc::new(Mutex::new(DiscordState {
                limiter: RateLimiter::new(RATE_LIMIT_CAPACITY, RATE_LIMIT_REFILL_PER_SEC),
                circuits: HashMap::new(),
                dlq: Dlq::default(),
            })),
        })
    }

    pub async fn send(&self, target: &SinkTarget, message: &SinkMessage) -> Result<()> {
        let key = target_rate_limit_key(target);
        let safe_target = telemetry::safe_target_id(target);
        let telemetry_ctx = telemetry::TelemetryContext::from_message(message);
        let (allowed, transition) = self.allow_request(&key);
        if let Some(transition) = transition {
            self.emit_circuit_transition(&telemetry_ctx.correlation_id, &safe_target, &transition);
        }
        if !allowed {
            telemetry::emit(discord_record(DiscordTelemetryInput {
                event_name: telemetry::event_name::DISCORD_SEND_FAILURE,
                reason_code: telemetry::reason::CIRCUIT_OPEN,
                correlation_id: &telemetry_ctx.correlation_id,
                safe_target: &safe_target,
                message,
                attempt: Some(MAX_ATTEMPTS),
                error: Some("circuit-open".to_string()),
                status: None,
            }));
            let error = format!("Discord circuit open for {safe_target}");
            self.record_dlq(target, message, MAX_ATTEMPTS, error.clone());
            return Err(error.into());
        }

        for attempt in 1..=MAX_ATTEMPTS {
            let delay = self.rate_limit_delay(&key);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            telemetry::emit(discord_record(DiscordTelemetryInput {
                event_name: telemetry::event_name::DISCORD_SEND_ATTEMPT,
                reason_code: telemetry::reason::DISCORD_PRE_SEND,
                correlation_id: &telemetry_ctx.correlation_id,
                safe_target: &safe_target,
                message,
                attempt: Some(attempt),
                error: None,
                status: None,
            }));

            let result = match target {
                SinkTarget::DiscordChannel(channel_id) => {
                    self.send_message(channel_id, &message.content).await
                }
                SinkTarget::DiscordWebhook(webhook_url) => {
                    self.send_webhook(webhook_url, &message.content).await
                }
                SinkTarget::SlackWebhook(_) => {
                    return Err("cannot send Slack webhook via Discord client".into());
                }
            };

            match result {
                Ok(()) => {
                    if let Some(transition) = self.record_success(&key) {
                        self.emit_circuit_transition(
                            &telemetry_ctx.correlation_id,
                            &safe_target,
                            &transition,
                        );
                    }
                    telemetry::emit(discord_record(DiscordTelemetryInput {
                        event_name: telemetry::event_name::DISCORD_SEND_SUCCESS,
                        reason_code: telemetry::reason::DISCORD_SUCCESS,
                        correlation_id: &telemetry_ctx.correlation_id,
                        safe_target: &safe_target,
                        message,
                        attempt: Some(attempt),
                        error: None,
                        status: None,
                    }));
                    return Ok(());
                }
                Err(error) => {
                    if let Some(transition) = self.record_failure(&key) {
                        self.emit_circuit_transition(
                            &telemetry_ctx.correlation_id,
                            &safe_target,
                            &transition,
                        );
                    }
                    if let Some(retry_after) = error.retry_after
                        && attempt < MAX_ATTEMPTS
                    {
                        telemetry::emit(discord_record(DiscordTelemetryInput {
                            event_name: telemetry::event_name::DISCORD_SEND_FAILURE,
                            reason_code: telemetry::reason::DISCORD_RETRY,
                            correlation_id: &telemetry_ctx.correlation_id,
                            safe_target: &safe_target,
                            message,
                            attempt: Some(attempt),
                            error: Some(error.message.clone()),
                            status: error.status,
                        }));
                        tokio::time::sleep(retry_after + jitter_for_attempt(attempt)).await;
                        continue;
                    }

                    telemetry::emit(discord_record(DiscordTelemetryInput {
                        event_name: telemetry::event_name::DISCORD_SEND_FAILURE,
                        reason_code: telemetry::reason::DISCORD_EXHAUSTED,
                        correlation_id: &telemetry_ctx.correlation_id,
                        safe_target: &safe_target,
                        message,
                        attempt: Some(attempt),
                        error: Some(error.message.clone()),
                        status: error.status,
                    }));
                    self.record_dlq(target, message, attempt, error.message.clone());
                    return Err(error.message.into());
                }
            }
        }

        let error = format!("Discord delivery exhausted retries for {safe_target}");
        self.record_dlq(target, message, MAX_ATTEMPTS, error.clone());
        Err(error.into())
    }

    /// Look up a Discord channel by ID using the bot API.
    ///
    /// Returns a typed `ChannelLookup` that surfaces the live channel name on
    /// success or a specific failure mode (not-found, forbidden, unauthorized,
    /// no-token, transport error). The DLQ and circuit breaker are deliberately
    /// NOT touched — binding verification is a read-only operator probe, not a
    /// dispatch event, and should never mark the delivery circuit as degraded.
    pub async fn lookup_channel(&self, channel_id: &str) -> ChannelLookup {
        let Some(client) = self.bot_client.as_ref() else {
            return ChannelLookup::NoToken;
        };

        let url = format!(
            "{}/channels/{}",
            self.api_base.trim_end_matches('/'),
            channel_id
        );

        let response = match client.get(url).send().await {
            Ok(response) => response,
            Err(error) => {
                return ChannelLookup::Transport(format!(
                    "Discord channel lookup request failed: {error}"
                ));
            }
        };

        let status = response.status();
        if status.is_success() {
            let body = match response.json::<DiscordChannelBody>().await {
                Ok(body) => body,
                Err(error) => {
                    return ChannelLookup::Transport(format!(
                        "Discord channel lookup body parse failed: {error}"
                    ));
                }
            };
            return ChannelLookup::Found {
                id: channel_id.to_string(),
                name: body.name,
            };
        }

        match status {
            StatusCode::NOT_FOUND => ChannelLookup::NotFound,
            StatusCode::FORBIDDEN => ChannelLookup::Forbidden,
            StatusCode::UNAUTHORIZED => ChannelLookup::Unauthorized,
            other => {
                let body = response.text().await.unwrap_or_default();
                ChannelLookup::Transport(format!(
                    "Discord channel lookup failed with {other}: {body}"
                ))
            }
        }
    }

    async fn send_message(
        &self,
        channel_id: &str,
        content: &str,
    ) -> std::result::Result<(), DiscordSendError> {
        let url = format!(
            "{}/channels/{}/messages",
            self.api_base.trim_end_matches('/'),
            channel_id
        );
        let client = self.bot_client.as_ref().ok_or_else(|| DiscordSendError {
            message: "missing Discord bot token for channel delivery; configure [providers.discord].token (or legacy [discord].token) or use a route webhook".to_string(),
            retry_after: None,
            status: None,
        })?;

        self.execute_request(
            client.post(url).json(&json!({ "content": content })),
            "Discord API request",
        )
        .await
    }

    async fn send_webhook(
        &self,
        webhook_url: &str,
        content: &str,
    ) -> std::result::Result<(), DiscordSendError> {
        self.execute_request(
            self.webhook_client
                .post(webhook_url_with_wait(webhook_url))
                .json(&json!({ "content": content })),
            "Discord webhook request",
        )
        .await
    }

    async fn execute_request(
        &self,
        request: reqwest::RequestBuilder,
        label: &str,
    ) -> std::result::Result<(), DiscordSendError> {
        let response = request.send().await.map_err(|_error| DiscordSendError {
            message: format!("{label} failed: transport error"),
            retry_after: None,
            status: None,
        })?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(DiscordSendError {
            message: format!("{label} failed with {status}: {body}"),
            retry_after: parse_retry_after(status, &body),
            status: Some(status.as_u16()),
        })
    }

    fn allow_request(
        &self,
        key: &str,
    ) -> (
        bool,
        Option<crate::core::circuit_breaker::CircuitTransition>,
    ) {
        let mut state = self.state.lock().expect("discord state lock");
        state
            .circuits
            .entry(key.to_string())
            .or_insert_with(|| {
                CircuitBreaker::new(
                    CIRCUIT_FAILURE_THRESHOLD,
                    Duration::from_secs(CIRCUIT_COOLDOWN_SECS),
                )
            })
            .allow_request()
    }

    fn rate_limit_delay(&self, key: &str) -> Duration {
        let mut state = self.state.lock().expect("discord state lock");
        state.limiter.delay_for(key)
    }

    fn record_success(&self, key: &str) -> Option<crate::core::circuit_breaker::CircuitTransition> {
        let mut state = self.state.lock().expect("discord state lock");
        state
            .circuits
            .entry(key.to_string())
            .or_insert_with(|| {
                CircuitBreaker::new(
                    CIRCUIT_FAILURE_THRESHOLD,
                    Duration::from_secs(CIRCUIT_COOLDOWN_SECS),
                )
            })
            .record_success()
    }

    fn record_failure(&self, key: &str) -> Option<crate::core::circuit_breaker::CircuitTransition> {
        let mut state = self.state.lock().expect("discord state lock");
        state
            .circuits
            .entry(key.to_string())
            .or_insert_with(|| {
                CircuitBreaker::new(
                    CIRCUIT_FAILURE_THRESHOLD,
                    Duration::from_secs(CIRCUIT_COOLDOWN_SECS),
                )
            })
            .record_failure()
    }

    fn emit_circuit_transition(
        &self,
        correlation_id: &str,
        safe_target: &str,
        transition: &crate::core::circuit_breaker::CircuitTransition,
    ) {
        let mut record = telemetry::record(
            telemetry::event_name::CIRCUIT_TRANSITION,
            telemetry::reason::CIRCUIT_TRANSITION,
            correlation_id.to_string(),
        );
        record.insert("target".to_string(), json!(safe_target));
        record.insert("from".to_string(), json!(transition.from));
        record.insert("to".to_string(), json!(transition.to));
        telemetry::emit(record);
    }

    fn record_dlq(&self, target: &SinkTarget, message: &SinkMessage, attempts: u32, error: String) {
        let safe_target = telemetry::safe_target_id(target);
        let correlation_id =
            telemetry::correlation_id_for_message(&message.event_kind, &message.payload);
        let entry = DlqEntry {
            original_topic: message.event_kind.clone(),
            retry_count: attempts,
            last_error: error,
            target: safe_target.clone(),
            event_kind: message.event_kind.clone(),
            format: message.format.as_str().to_string(),
            content: message.content.clone(),
            payload: message.payload.clone(),
            correlation_id: Some(correlation_id.clone()),
            content_bytes: Some(message.content.len()),
            payload_bytes: telemetry::payload_bytes(&message.payload),
        };

        telemetry::emit(discord_record(DiscordTelemetryInput {
            event_name: telemetry::event_name::DLQ_BURY,
            reason_code: telemetry::reason::DLQ_WRITE,
            correlation_id: &correlation_id,
            safe_target: &safe_target,
            message,
            attempt: Some(attempts),
            error: Some(entry.last_error.clone()),
            status: None,
        }));

        eprintln!(
            "clawhip dlq bury: {}",
            serde_json::to_string(&entry)
                .unwrap_or_else(|_| "{\"error\":\"dlq serialize failed\"}".to_string())
        );

        let mut state = self.state.lock().expect("discord state lock");
        state.dlq.push(entry);
    }

    #[cfg(test)]
    pub(crate) fn for_tests_with_api_base(bot_token: &str, api_base: String) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bot {bot_token}"))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let bot_client = Some(
            reqwest::Client::builder()
                .default_headers(headers)
                .build()?,
        );
        let webhook_client = reqwest::Client::new();

        Ok(Self {
            bot_client,
            webhook_client,
            api_base,
            state: Arc::new(Mutex::new(DiscordState {
                limiter: RateLimiter::new(RATE_LIMIT_CAPACITY, RATE_LIMIT_REFILL_PER_SEC),
                circuits: HashMap::new(),
                dlq: Dlq::default(),
            })),
        })
    }

    #[cfg(test)]
    fn dlq_entries(&self) -> Vec<DlqEntry> {
        self.state
            .lock()
            .expect("discord state lock")
            .dlq
            .entries()
            .to_vec()
    }
}

fn parse_retry_after(status: StatusCode, body: &str) -> Option<Duration> {
    if status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }

    serde_json::from_str::<DiscordRateLimitBody>(body)
        .ok()
        .and_then(|parsed| parsed.retry_after)
        .map(Duration::from_secs_f64)
}

fn target_rate_limit_key(target: &SinkTarget) -> String {
    match target {
        SinkTarget::DiscordChannel(channel_id) => format!("discord:channel:{channel_id}"),
        SinkTarget::DiscordWebhook(webhook_url) => format!("discord:webhook:{webhook_url}"),
        SinkTarget::SlackWebhook(webhook_url) => format!("slack:webhook:{webhook_url}"),
    }
}

struct DiscordTelemetryInput<'a> {
    event_name: &'a str,
    reason_code: &'a str,
    correlation_id: &'a str,
    safe_target: &'a str,
    message: &'a SinkMessage,
    attempt: Option<u32>,
    error: Option<String>,
    status: Option<u16>,
}

fn discord_record(input: DiscordTelemetryInput<'_>) -> serde_json::Map<String, serde_json::Value> {
    let mut record = telemetry::record(
        input.event_name,
        input.reason_code,
        input.correlation_id.to_string(),
    );
    record.insert("target".to_string(), json!(input.safe_target));
    record.insert("event_kind".to_string(), json!(input.message.event_kind));
    record.insert("format".to_string(), json!(input.message.format.as_str()));
    record.insert(
        "content_bytes".to_string(),
        json!(input.message.content.len()),
    );
    record.insert(
        "payload_bytes".to_string(),
        json!(telemetry::payload_bytes(&input.message.payload)),
    );
    if let Some(attempt) = input.attempt {
        record.insert("attempt".to_string(), json!(attempt));
    }
    if let Some(error) = input.error {
        record.insert("error".to_string(), json!(error));
    }
    if let Some(status) = input.status {
        record.insert("status".to_string(), json!(status));
    }
    if let Some(extra) = &input.message.telemetry {
        record.insert("route_result".to_string(), json!(extra.route_result));
        record.insert("route_index".to_string(), json!(extra.route_index));
        record.insert("batch_count".to_string(), json!(extra.batch_count));
    }
    record
}

fn jitter_for_attempt(attempt: u32) -> Duration {
    Duration::from_millis(JITTER_MS * u64::from(attempt))
}

fn webhook_url_with_wait(webhook_url: &str) -> String {
    if webhook_url.contains("wait=") {
        webhook_url.to_string()
    } else if webhook_url.contains('?') {
        format!("{webhook_url}&wait=true")
    } else {
        format!("{webhook_url}?wait=true")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::MessageFormat;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn webhook_urls_gain_wait_true_by_default() {
        assert_eq!(
            webhook_url_with_wait("https://discord.com/api/webhooks/1/abc"),
            "https://discord.com/api/webhooks/1/abc?wait=true"
        );
        assert_eq!(
            webhook_url_with_wait("https://discord.com/api/webhooks/1/abc?thread_id=7"),
            "https://discord.com/api/webhooks/1/abc?thread_id=7&wait=true"
        );
        assert_eq!(
            webhook_url_with_wait("https://discord.com/api/webhooks/1/abc?wait=false"),
            "https://discord.com/api/webhooks/1/abc?wait=false"
        );
    }

    #[test]
    fn parses_retry_after_for_429() {
        assert_eq!(
            parse_retry_after(StatusCode::TOO_MANY_REQUESTS, r#"{"retry_after":0.25}"#),
            Some(Duration::from_millis(250))
        );
        assert_eq!(parse_retry_after(StatusCode::BAD_REQUEST, "{}"), None);
    }

    #[tokio::test]
    async fn retries_429_then_succeeds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for idx in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0_u8; 4096];
                let _ = stream.read(&mut buf).await.unwrap();
                if idx == 0 {
                    let body = r#"{"retry_after":0.01}"#;
                    let response = format!(
                        "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                } else {
                    stream
                        .write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n")
                        .await
                        .unwrap();
                }
            }
        });

        let client = DiscordClient::from_config(Arc::new(AppConfig::default())).unwrap();
        let message = SinkMessage {
            event_kind: "tmux.keyword".into(),
            format: MessageFormat::Compact,
            content: "hello".into(),
            payload: json!({"session":"ops"}),
            telemetry: None,
        };
        client
            .send(
                &SinkTarget::DiscordWebhook(format!("http://{addr}/webhook")),
                &message,
            )
            .await
            .unwrap();
        server.await.unwrap();
        assert!(client.dlq_entries().is_empty());
    }

    /// Serve a single HTTP response on a bound TCP listener.
    async fn serve_once(
        listener: tokio::net::TcpListener,
        status_line: &'static str,
        body: &'static str,
    ) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0_u8; 4096];
        let _ = stream.read(&mut buf).await.unwrap();
        let response = format!(
            "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len(),
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.ok();
    }

    #[tokio::test]
    async fn lookup_channel_returns_found_with_name() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_once(
            listener,
            "HTTP/1.1 200 OK",
            r#"{"id":"1480171113253175356","name":"clawhip-dev","type":0}"#,
        ));

        let client =
            DiscordClient::for_tests_with_api_base("test-token", format!("http://{addr}")).unwrap();
        let lookup = client.lookup_channel("1480171113253175356").await;
        server.await.unwrap();

        match lookup {
            ChannelLookup::Found { id, name } => {
                assert_eq!(id, "1480171113253175356");
                assert_eq!(name.as_deref(), Some("clawhip-dev"));
            }
            other => panic!("expected Found, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn lookup_channel_returns_not_found() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_once(
            listener,
            "HTTP/1.1 404 Not Found",
            r#"{"message":"Unknown Channel","code":10003}"#,
        ));

        let client =
            DiscordClient::for_tests_with_api_base("test-token", format!("http://{addr}")).unwrap();
        let lookup = client.lookup_channel("9999999999999999").await;
        server.await.unwrap();

        assert!(matches!(lookup, ChannelLookup::NotFound));
    }

    #[tokio::test]
    async fn lookup_channel_returns_forbidden() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_once(
            listener,
            "HTTP/1.1 403 Forbidden",
            r#"{"message":"Missing Access","code":50001}"#,
        ));

        let client =
            DiscordClient::for_tests_with_api_base("test-token", format!("http://{addr}")).unwrap();
        let lookup = client.lookup_channel("1111").await;
        server.await.unwrap();

        assert!(matches!(lookup, ChannelLookup::Forbidden));
    }

    #[tokio::test]
    async fn lookup_channel_returns_unauthorized() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_once(
            listener,
            "HTTP/1.1 401 Unauthorized",
            r#"{"message":"401: Unauthorized","code":0}"#,
        ));

        let client =
            DiscordClient::for_tests_with_api_base("bad-token", format!("http://{addr}")).unwrap();
        let lookup = client.lookup_channel("1111").await;
        server.await.unwrap();

        assert!(matches!(lookup, ChannelLookup::Unauthorized));
    }

    #[tokio::test]
    async fn lookup_channel_returns_no_token_when_missing() {
        // Build a DiscordClient with no bot token (no env, no config).
        // Use a bogus env override so we never hit the real API.
        unsafe {
            std::env::set_var("CLAWHIP_DISCORD_API_BASE", "http://127.0.0.1:1");
        }
        let client = DiscordClient::from_config(Arc::new(AppConfig::default())).unwrap();
        unsafe {
            std::env::remove_var("CLAWHIP_DISCORD_API_BASE");
        }
        // Config has no bot token and no webhook route; lookup should skip.
        let lookup = client.lookup_channel("1111").await;
        assert!(matches!(lookup, ChannelLookup::NoToken));
    }

    #[tokio::test]
    async fn lookup_channel_does_not_touch_dlq_on_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(serve_once(
            listener,
            "HTTP/1.1 404 Not Found",
            r#"{"message":"Unknown Channel"}"#,
        ));

        let client =
            DiscordClient::for_tests_with_api_base("test-token", format!("http://{addr}")).unwrap();
        let _ = client.lookup_channel("1").await;
        server.await.unwrap();

        // Lookup failures must NOT pollute the DLQ — it's a read-only probe.
        assert!(client.dlq_entries().is_empty());
    }

    #[tokio::test]
    async fn exhausted_failures_land_in_dlq() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0_u8; 4096];
                let _ = stream.read(&mut buf).await.unwrap();
                let body = r#"{"retry_after":0.0}"#;
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let client = DiscordClient::from_config(Arc::new(AppConfig::default())).unwrap();
        let message = SinkMessage {
            event_kind: "github.ci-failed".into(),
            format: MessageFormat::Alert,
            content: "boom".into(),
            payload: json!({"repo":"clawhip", "correlation_id":"corr-214"}),
            telemetry: None,
        };
        let error = client
            .send(
                &SinkTarget::DiscordWebhook(format!("http://{addr}/webhook")),
                &message,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("429"));
        server.await.unwrap();
        let dlq = client.dlq_entries();
        assert_eq!(dlq.len(), 1);
        assert_eq!(dlq[0].payload["repo"], "clawhip");
        assert_eq!(dlq[0].retry_count, 3);
        assert!(dlq[0].target.starts_with("discord:webhook:"));
        assert!(!dlq[0].target.contains(&format!("http://{addr}/webhook")));
        assert_eq!(dlq[0].correlation_id.as_deref(), Some("corr-214"));
        assert_eq!(dlq[0].content_bytes, Some(4));
        assert!(dlq[0].payload_bytes.is_some());
    }

    #[test]
    fn record_dlq_redacts_target_and_preserves_correlation() {
        let client = DiscordClient::from_config(Arc::new(AppConfig::default())).unwrap();
        let message = SinkMessage {
            event_kind: "github.ci-failed".into(),
            format: MessageFormat::Alert,
            content: "boom".into(),
            payload: json!({"repo":"clawhip", "correlation_id":"corr-214"}),
            telemetry: None,
        };
        let target = SinkTarget::DiscordWebhook(
            "https://discord.com/api/webhooks/123456/secret-token".into(),
        );

        client.record_dlq(&target, &message, 3, "failed".into());

        let dlq = client.dlq_entries();
        assert_eq!(dlq.len(), 1);
        assert!(
            dlq[0]
                .target
                .starts_with("discord:webhook:discord.com/redacted/")
        );
        assert!(!dlq[0].target.contains("123456"));
        assert!(!dlq[0].target.contains("secret-token"));
        assert_eq!(dlq[0].correlation_id.as_deref(), Some("corr-214"));
        assert_eq!(dlq[0].content_bytes, Some(4));
        assert!(dlq[0].payload_bytes.is_some());
    }
}
