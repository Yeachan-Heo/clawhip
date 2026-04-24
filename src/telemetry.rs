use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde_json::{Map, Value, json};

use crate::events::IncomingEvent;
use crate::sink::{SinkMessage, SinkTarget};

pub mod event_name {
    pub const DAEMON_PHASE: &str = "daemon_phase";
    pub const EVENT_ACCEPTED: &str = "event_accepted";
    pub const EVENT_DROPPED: &str = "event_dropped";
    pub const ROUTE_TRACE: &str = "route_trace";
    pub const ROUTINE_DEFERRED: &str = "routine_deferred";
    pub const ROUTINE_FLUSHED: &str = "routine_flushed";
    pub const DISPATCH_FAILURE: &str = "dispatch_failure";
    pub const DISCORD_SEND_ATTEMPT: &str = "discord_send_attempt";
    pub const DISCORD_SEND_FAILURE: &str = "discord_send_failure";
    pub const DISCORD_SEND_SUCCESS: &str = "discord_send_success";
    pub const CIRCUIT_TRANSITION: &str = "circuit_transition";
    pub const DLQ_BURY: &str = "dlq_bury";
    pub const SOURCE_DEGRADED: &str = "source_degraded";
    pub const SOURCE_INVENTORY: &str = "source_inventory";
}

pub mod reason {
    pub const DAEMON_STARTUP: &str = "daemon_startup";
    pub const DAEMON_LISTENING: &str = "daemon_listening";
    pub const SOURCE_START: &str = "source_start";
    pub const SOURCE_STOPPED: &str = "source_stopped";
    pub const ACCEPT_ENQUEUED: &str = "accept_enqueued";
    pub const DROP_NON_GIT_NATIVE_HOOK: &str = "drop_non_git_native_hook";
    pub const ROUTE_MATCHED: &str = "route_matched";
    pub const ROUTE_FALLBACK: &str = "route_fallback";
    pub const ROUTE_NONE: &str = "route_none";
    pub const ROUTINE_BATCH_DEFERRED: &str = "routine_batch_deferred";
    pub const ROUTINE_BATCH_FLUSHED: &str = "routine_batch_flushed";
    pub const RENDER_FAILED: &str = "render_failed";
    pub const SINK_MISSING: &str = "sink_missing";
    pub const SINK_SEND_FAILED: &str = "sink_send_failed";
    pub const DISCORD_PRE_SEND: &str = "discord_pre_send";
    pub const DISCORD_RETRY: &str = "discord_retry";
    pub const DISCORD_EXHAUSTED: &str = "discord_exhausted";
    pub const DISCORD_SUCCESS: &str = "discord_success";
    pub const CIRCUIT_OPEN: &str = "circuit_open";
    pub const CIRCUIT_TRANSITION: &str = "circuit_transition";
    pub const DLQ_WRITE: &str = "dlq_write";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryContext {
    pub correlation_id: String,
}

impl TelemetryContext {
    pub fn from_message(message: &SinkMessage) -> Self {
        Self {
            correlation_id: correlation_id_for_message(&message.event_kind, &message.payload),
        }
    }
}

pub fn correlation_id_for_event(event: &IncomingEvent) -> String {
    event
        .payload
        .get("correlation_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            event
                .payload
                .get("event_id")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
        })
        .map(ToString::to_string)
        .unwrap_or_else(|| stable_correlation_id(event.canonical_kind(), &event.payload))
}

pub fn correlation_id_for_message(event_kind: &str, payload: &Value) -> String {
    payload
        .get("correlation_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            payload
                .get("event_id")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
        })
        .map(ToString::to_string)
        .unwrap_or_else(|| stable_correlation_id(event_kind, payload))
}

pub fn stable_correlation_id(event_kind: &str, payload: &Value) -> String {
    let mut hasher = DefaultHasher::new();
    event_kind.hash(&mut hasher);
    serde_json::to_string(payload)
        .unwrap_or_else(|_| "<unserializable>".to_string())
        .hash(&mut hasher);
    format!("derived:{:016x}", hasher.finish())
}

pub fn safe_target_id(target: &SinkTarget) -> String {
    match target {
        SinkTarget::DiscordChannel(channel_id) => format!("discord:channel:{channel_id}"),
        SinkTarget::DiscordWebhook(webhook_url) => {
            format!("discord:webhook:{}", redacted_url_fingerprint(webhook_url))
        }
        SinkTarget::SlackWebhook(webhook_url) => {
            format!("slack:webhook:{}", redacted_url_fingerprint(webhook_url))
        }
    }
}

pub fn redacted_url_fingerprint(url: &str) -> String {
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("unknown-host")
        .trim()
        .split('@')
        .next_back()
        .unwrap_or("unknown-host");
    format!("{host}/redacted/{:016x}", fingerprint(url))
}

pub fn payload_bytes(payload: &Value) -> Option<usize> {
    serde_json::to_vec(payload).ok().map(|bytes| bytes.len())
}

pub fn record(
    event: &str,
    reason_code: &str,
    correlation_id: impl Into<String>,
) -> Map<String, Value> {
    let mut object = Map::new();
    object.insert("telemetry_event".to_string(), json!(event));
    object.insert("reason_code".to_string(), json!(reason_code));
    object.insert("correlation_id".to_string(), json!(correlation_id.into()));
    object
}

pub fn render_line(mut object: Map<String, Value>) -> String {
    object
        .entry("schema".to_string())
        .or_insert_with(|| json!("clawhip.telemetry.v1"));
    serde_json::to_string(&Value::Object(object)).unwrap_or_else(|_| {
        r#"{"schema":"clawhip.telemetry.v1","telemetry_event":"serialize_failed"}"#.to_string()
    })
}

pub fn emit(object: Map<String, Value>) {
    eprintln!("{}", render_line(object));
}

fn fingerprint(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::IncomingEvent;

    #[test]
    fn safe_target_id_redacts_webhook_secret() {
        let url = "https://discord.com/api/webhooks/123456/secret-token";
        let safe = safe_target_id(&SinkTarget::DiscordWebhook(url.into()));
        assert!(safe.starts_with("discord:webhook:discord.com/redacted/"));
        assert!(!safe.contains("123456"));
        assert!(!safe.contains("secret-token"));
        assert!(!safe.contains(url));
    }

    #[test]
    fn channel_target_keeps_non_secret_identifier() {
        assert_eq!(
            safe_target_id(&SinkTarget::DiscordChannel("ops".into())),
            "discord:channel:ops"
        );
    }

    #[test]
    fn correlation_prefers_existing_payload_fields_without_mutating_payload() {
        let event = IncomingEvent::custom(None, "hello".into());
        let before = event.payload.clone();
        let first = correlation_id_for_event(&event);
        let second = correlation_id_for_event(&event);
        assert_eq!(first, second);
        assert_eq!(event.payload, before);
    }

    #[test]
    fn render_line_uses_expected_stable_fields() {
        let line = render_line(record(
            event_name::EVENT_DROPPED,
            reason::DROP_NON_GIT_NATIVE_HOOK,
            "corr-1",
        ));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["schema"], json!("clawhip.telemetry.v1"));
        assert_eq!(parsed["telemetry_event"], json!("event_dropped"));
        assert_eq!(parsed["reason_code"], json!("drop_non_git_native_hook"));
        assert_eq!(parsed["correlation_id"], json!("corr-1"));
    }
}
