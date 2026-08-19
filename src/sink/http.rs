use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::redirect::Policy;
use ring::{digest, hmac};
use serde_json::{Map, Value, json};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

use crate::Result;
use crate::config::{HttpConfig, validate_http_endpoint};
use crate::telemetry;

use super::{Sink, SinkMessage, SinkTarget};

const EVENT_SCHEMA: &str = "clawhip.http-event.v1";
const MAX_EVENT_BODY_BYTES: usize = 16 * 1024;
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SUMMARY_CHARS: usize = 1_024;
const MAX_PUBLIC_STRING_CHARS: usize = 512;
const MAX_EVENT_TIMESTAMP_BYTES: usize = 35;
const MAX_PUBLIC_FIELDS: usize = 48;
const MAX_PUBLIC_ARRAY_ITEMS: usize = 16;
const MAX_PUBLIC_DEPTH: usize = 3;
const FALLBACK_EVENT_TIMESTAMP: &str = "1970-01-01T00:00:00Z";

#[derive(Clone)]
pub struct HttpSink {
    client: reqwest::Client,
    secret: Arc<[u8]>,
}

impl HttpSink {
    pub fn new(secret: impl AsRef<[u8]>) -> Result<Self> {
        Self::with_request_timeout(secret, HTTP_REQUEST_TIMEOUT)
    }

    fn with_request_timeout(secret: impl AsRef<[u8]>, timeout: Duration) -> Result<Self> {
        let secret = secret.as_ref();
        if secret.is_empty() {
            return Err("HTTP sink HMAC secret must not be empty".into());
        }

        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(Policy::none())
                .timeout(timeout)
                .build()?,
            secret: Arc::from(secret),
        })
    }

    pub fn from_config(config: &HttpConfig) -> Result<Self> {
        let endpoint = config
            .endpoint()
            .ok_or_else(|| "providers.http.endpoint is required for the HTTP sink".to_string())?;
        validate_http_endpoint(endpoint)?;

        let secret_env = config.hmac_secret_env().ok_or_else(|| {
            "providers.http.hmac_secret_env is required for the HTTP sink".to_string()
        })?;
        let secret = std::env::var(secret_env).map_err(|_| {
            format!(
                "HTTP sink HMAC secret environment variable {secret_env} is not set or is not valid UTF-8"
            )
        })?;
        if secret.is_empty() {
            return Err(format!(
                "HTTP sink HMAC secret environment variable {secret_env} is empty"
            )
            .into());
        }

        Self::new(secret.as_bytes())
    }
}

#[async_trait]
impl Sink for HttpSink {
    async fn send(&self, target: &SinkTarget, message: &SinkMessage) -> Result<()> {
        let SinkTarget::HttpEndpoint(endpoint) = target else {
            return Err("HTTP sink received a non-HTTP target".into());
        };
        validate_http_endpoint(endpoint)?;

        let request_id = request_id_for_message(message);
        let body = event_body(message, target, &request_id)?;
        let body_bytes = body.len();
        let signature = hmac_signature(&self.secret, &body);
        emit_http_telemetry(
            telemetry::event_name::HTTP_SEND_ATTEMPT,
            telemetry::reason::HTTP_PRE_SEND,
            message,
            target,
            &request_id,
            body_bytes,
            None,
        );

        let response = match self
            .client
            .post(endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-Hub-Signature-256", signature)
            .header("X-Request-ID", &request_id)
            .body(body)
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => {
                emit_http_telemetry(
                    telemetry::event_name::HTTP_SEND_FAILURE,
                    telemetry::reason::HTTP_TRANSPORT_ERROR,
                    message,
                    target,
                    &request_id,
                    body_bytes,
                    None,
                );
                return Err("HTTP sink request failed before receiving a response".into());
            }
        };

        let status = response.status();
        if status.is_success() {
            emit_http_telemetry(
                telemetry::event_name::HTTP_SEND_SUCCESS,
                telemetry::reason::HTTP_SUCCESS,
                message,
                target,
                &request_id,
                body_bytes,
                Some(status.as_u16()),
            );
            return Ok(());
        }

        emit_http_telemetry(
            telemetry::event_name::HTTP_SEND_FAILURE,
            telemetry::reason::HTTP_STATUS_ERROR,
            message,
            target,
            &request_id,
            body_bytes,
            Some(status.as_u16()),
        );
        Err(format!("HTTP sink request failed with status {}", status.as_u16()).into())
    }
}

fn event_body(message: &SinkMessage, target: &SinkTarget, request_id: &str) -> Result<Vec<u8>> {
    let mut state = NormalizationState::default();
    let timestamp = event_timestamp(&message.payload);
    let payload = normalize_payload(&message.payload, &mut state);
    let summary = public_text(&message.content, MAX_SUMMARY_CHARS, &mut state);
    let source = message
        .event_kind
        .split('.')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or("custom");
    let provenance = message.telemetry.as_ref().map(|context| {
        json!({
            "route_result": context.route_result,
            "route_index": context.route_index,
            "target": telemetry::safe_target_id(target),
            "batch_count": context.batch_count,
        })
    });

    let mut value = json!({
        "schema": EVENT_SCHEMA,
        "request_id": request_id,
        "timestamp": timestamp,
        "type": bounded_identifier(&message.event_kind, 160),
        "source": bounded_identifier(source, 80),
        "format": message.format.as_str(),
        "summary": summary,
        "payload": payload,
        "provenance": provenance,
        "target": telemetry::safe_target_id(target),
        "public_safe": true,
        "redacted": state.redacted,
        "truncated": state.truncated,
    });

    let mut body = serde_json::to_vec(&value)?;
    if body.len() > MAX_EVENT_BODY_BYTES {
        value["summary"] = json!(truncate_chars(
            value["summary"].as_str().unwrap_or_default(),
            256
        ));
        value["payload"] = json!({"truncated": true});
        value["truncated"] = json!(true);
        body = serde_json::to_vec(&value)?;
    }
    if body.len() > MAX_EVENT_BODY_BYTES {
        return Err("normalized HTTP event body exceeds the public-safe size limit".into());
    }

    Ok(body)
}

fn event_timestamp(payload: &Value) -> String {
    ["event_timestamp", "timestamp", "first_seen_at"]
        .into_iter()
        .filter_map(|key| payload.get(key))
        .find_map(canonical_timestamp)
        .unwrap_or_else(|| FALLBACK_EVENT_TIMESTAMP.to_string())
}

fn canonical_timestamp(value: &Value) -> Option<String> {
    let parsed = match value {
        Value::String(value) => parse_timestamp(value),
        Value::Number(value) => value.as_i64().and_then(timestamp_from_unix_integer),
        _ => None,
    }?;
    let formatted = parsed.to_offset(UtcOffset::UTC).format(&Rfc3339).ok()?;

    (formatted.len() <= MAX_EVENT_TIMESTAMP_BYTES).then_some(formatted)
}

fn parse_timestamp(value: &str) -> Option<OffsetDateTime> {
    let value = value.trim();
    OffsetDateTime::parse(value, &Rfc3339).ok().or_else(|| {
        value
            .parse::<i64>()
            .ok()
            .and_then(timestamp_from_unix_integer)
    })
}

fn timestamp_from_unix_integer(value: i64) -> Option<OffsetDateTime> {
    let seconds = if value.unsigned_abs() >= 10_000_000_000 {
        value / 1_000
    } else {
        value
    };
    OffsetDateTime::from_unix_timestamp(seconds).ok()
}

#[derive(Default)]
struct NormalizationState {
    redacted: bool,
    truncated: bool,
}

fn normalize_payload(payload: &Value, state: &mut NormalizationState) -> Value {
    normalize_public_value(None, payload, 0, state).unwrap_or_else(|| json!({}))
}

fn normalize_public_value(
    key: Option<&str>,
    value: &Value,
    depth: usize,
    state: &mut NormalizationState,
) -> Option<Value> {
    if depth > MAX_PUBLIC_DEPTH {
        state.truncated = true;
        return None;
    }

    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => {
            let key = key?;
            if is_public_scalar_field(key) {
                Some(value.clone())
            } else {
                state.redacted = true;
                None
            }
        }
        Value::String(value) => {
            let key = key?;
            if is_url_field(key) {
                Some(json!(public_url(value, state)))
            } else if is_public_text_field(key) {
                Some(json!(public_text(value, MAX_PUBLIC_STRING_CHARS, state)))
            } else {
                state.redacted = true;
                None
            }
        }
        Value::Array(values) => {
            let key = key?;
            if !is_public_array_field(key) {
                state.redacted = true;
                return None;
            }

            if values.len() > MAX_PUBLIC_ARRAY_ITEMS {
                state.truncated = true;
            }
            let values = values
                .iter()
                .take(MAX_PUBLIC_ARRAY_ITEMS)
                .filter_map(|value| match value {
                    Value::String(value) => {
                        Some(json!(public_text(value, MAX_PUBLIC_STRING_CHARS, state)))
                    }
                    _ => normalize_public_value(None, value, depth + 1, state),
                })
                .collect();
            Some(Value::Array(values))
        }
        Value::Object(object) => {
            if key.is_some() {
                state.redacted = true;
                return None;
            }
            let mut safe = Map::new();
            if object.len() > MAX_PUBLIC_FIELDS {
                state.truncated = true;
            }
            for (key, value) in object {
                if safe.len() >= MAX_PUBLIC_FIELDS {
                    state.truncated = true;
                    break;
                }
                if is_sensitive_field(key) {
                    state.redacted = true;
                    continue;
                }
                if let Some(value) = normalize_public_value(Some(key), value, depth + 1, state) {
                    safe.insert(key.clone(), value);
                } else if matches!(value, Value::Object(_) | Value::Array(_)) {
                    state.redacted = true;
                }
            }
            Some(Value::Object(safe))
        }
    }
}

fn is_public_text_field(key: &str) -> bool {
    matches!(
        key,
        "action"
            | "actor"
            | "agent_name"
            | "branch"
            | "commit"
            | "conclusion"
            | "contract"
            | "contract_event"
            | "correlation_id"
            | "event"
            | "event_id"
            | "event_kind"
            | "event_name"
            | "error_message"
            | "error_summary"
            | "first_seen_at"
            | "hook_event_name"
            | "id"
            | "keyword"
            | "line"
            | "last_line"
            | "message"
            | "name"
            | "new_branch"
            | "new_status"
            | "normalized_event"
            | "observation_confidence"
            | "observation_source"
            | "old_branch"
            | "old_status"
            | "project"
            | "provider"
            | "question_summary"
            | "repo"
            | "repo_name"
            | "route_key"
            | "session"
            | "session_id"
            | "session_name"
            | "sha"
            | "short_commit"
            | "source"
            | "state_family"
            | "status"
            | "summary"
            | "summary_text"
            | "tag"
            | "title"
            | "tool_name"
            | "workflow"
    )
}

fn is_url_field(key: &str) -> bool {
    matches!(key, "url" | "pr_url")
}

fn is_public_scalar_field(key: &str) -> bool {
    matches!(
        key,
        "active_sessions_needing_action"
            | "approval_hold"
            | "batch_count"
            | "comments"
            | "commit_count"
            | "count"
            | "elapsed_secs"
            | "fallback_evidence"
            | "hit_count"
            | "is_prerelease"
            | "issue_number"
            | "local_only"
            | "minutes"
            | "number"
            | "open_issues"
            | "open_prs"
            | "pr_number"
            | "release_hold"
            | "run_job_count"
            | "zero_backlog"
    )
}

fn is_public_array_field(key: &str) -> bool {
    matches!(key, "commits" | "event_kinds" | "hits" | "reasons")
}

fn is_sensitive_field(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("token")
        || key.contains("secret")
        || key.contains("password")
        || key.contains("passwd")
        || key.contains("authorization")
        || key.contains("credential")
        || key.contains("cookie")
        || key.contains("webhook")
        || key.ends_with("_path")
        || matches!(
            key.as_str(),
            "command"
                | "cwd"
                | "directory"
                | "event_payload"
                | "payload"
                | "raw"
                | "tool_input"
                | "tool_response"
                | "transcript_path"
                | "worktree_path"
        )
}

fn public_text(value: &str, max_chars: usize, state: &mut NormalizationState) -> String {
    let mut collapsed = String::with_capacity(value.len().min(max_chars));
    let mut previous_space = false;
    let mut output_chars = 0;
    let max_scanned_chars = max_chars.saturating_mul(4).max(max_chars);
    for (index, ch) in value.chars().enumerate() {
        if index >= max_scanned_chars || output_chars > max_chars {
            state.truncated = true;
            break;
        }
        let normalized = if ch.is_control() || ch.is_whitespace() {
            ' '
        } else {
            ch
        };
        if normalized == ' ' {
            if previous_space {
                continue;
            }
            previous_space = true;
        } else {
            previous_space = false;
        }
        collapsed.push(normalized);
        output_chars += 1;
    }

    let redacted_urls = redact_urls_in_text(collapsed.trim(), state);
    let redacted_secrets = redact_secret_assignments(&redacted_urls, state);
    if redacted_secrets.chars().count() > max_chars {
        state.truncated = true;
    }
    truncate_chars(&redacted_secrets, max_chars)
}

fn redact_urls_in_text(value: &str, state: &mut NormalizationState) -> String {
    let mut output = String::with_capacity(value.len());
    let mut remainder = value;

    loop {
        let lower = remainder.to_ascii_lowercase();
        let http = lower.find("http://");
        let https = lower.find("https://");
        let Some(start) = (match (http, https) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(index), None) | (None, Some(index)) => Some(index),
            (None, None) => None,
        }) else {
            output.push_str(remainder);
            break;
        };

        output.push_str(&remainder[..start]);
        let url_and_rest = &remainder[start..];
        let end = url_and_rest
            .find(char::is_whitespace)
            .unwrap_or(url_and_rest.len());
        let token = &url_and_rest[..end];
        let trimmed = token.trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '>', '\'', '"']);
        let suffix = &token[trimmed.len()..];
        output.push_str(&public_url(trimmed, state));
        output.push_str(suffix);
        remainder = &url_and_rest[end..];
    }

    output
}

fn redact_secret_assignments(value: &str, state: &mut NormalizationState) -> String {
    value
        .split(' ')
        .map(|word| {
            let lower = word.to_ascii_lowercase();
            for marker in [
                "authorization:",
                "authorization=",
                "password=",
                "passwd=",
                "secret=",
                "token=",
            ] {
                if let Some(index) = lower.find(marker) {
                    state.redacted = true;
                    return format!(
                        "{}{}[redacted]",
                        &word[..index],
                        &word[index..index + marker.len()]
                    );
                }
            }
            word.to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn public_url(value: &str, state: &mut NormalizationState) -> String {
    let Ok(mut url) = reqwest::Url::parse(value) else {
        state.redacted = true;
        return "[redacted-url]".to_string();
    };
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        state.redacted = true;
        return "[redacted-url]".to_string();
    }

    let path = url.path().to_ascii_lowercase();
    let sensitive = !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || path.contains("/webhook")
        || path.contains("/hooks/")
        || path.contains("token")
        || path.contains("secret")
        || path.contains("signature");
    if sensitive {
        state.redacted = true;
        return format!(
            "{}://{}",
            url.scheme(),
            telemetry::redacted_url_fingerprint(value)
        );
    }

    url.set_query(None);
    url.set_fragment(None);
    truncate_chars(url.as_str(), MAX_PUBLIC_STRING_CHARS)
}

fn request_id_for_message(message: &SinkMessage) -> String {
    let candidate = message
        .telemetry
        .as_ref()
        .map(|context| context.correlation_id.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| {
            telemetry::correlation_id_for_message(&message.event_kind, &message.payload)
        });

    if candidate.len() <= 128
        && !candidate.is_empty()
        && candidate.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
    {
        return candidate;
    }

    let digest = digest::digest(&digest::SHA256, candidate.as_bytes());
    format!("clawhip-{}", hex(digest.as_ref()))
}

fn bounded_identifier(value: &str, max_chars: usize) -> String {
    let normalized = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
        .collect::<String>();
    truncate_chars(&normalized, max_chars)
}

fn hmac_signature(secret: &[u8], body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let signature = hmac::sign(&key, body);
    format!("sha256={}", hex(signature.as_ref()))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

#[allow(clippy::too_many_arguments)]
fn emit_http_telemetry(
    event_name: &str,
    reason_code: &str,
    message: &SinkMessage,
    target: &SinkTarget,
    request_id: &str,
    body_bytes: usize,
    status: Option<u16>,
) {
    let correlation_id = message
        .telemetry
        .as_ref()
        .map(|context| context.correlation_id.clone())
        .unwrap_or_else(|| {
            telemetry::correlation_id_for_message(&message.event_kind, &message.payload)
        });
    let mut record = telemetry::record(event_name, reason_code, correlation_id);
    record.insert(
        "target".to_string(),
        json!(telemetry::safe_target_id(target)),
    );
    record.insert("request_id".to_string(), json!(request_id));
    record.insert("event_kind".to_string(), json!(message.event_kind));
    record.insert("format".to_string(), json!(message.format.as_str()));
    record.insert("body_bytes".to_string(), json!(body_bytes));
    record.insert("status".to_string(), json!(status));
    if let Some(context) = &message.telemetry {
        record.insert("route_result".to_string(), json!(context.route_result));
        record.insert("route_index".to_string(), json!(context.route_index));
        record.insert("batch_count".to_string(), json!(context.batch_count));
    }
    telemetry::emit(record);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::MessageFormat;
    use crate::sink::SinkTelemetry;
    use serial_test::serial;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{Duration, timeout};

    fn message() -> SinkMessage {
        SinkMessage {
            event_kind: "session.finished".into(),
            format: MessageFormat::Compact,
            content: "done https://example.test/public token=hidden".into(),
            payload: json!({
                "event_id": "evt-1",
                "repo_name": "clawhip",
                "session_id": "sess-1",
                "summary": "complete",
                "url": "https://example.test/result?token=private",
                "repo_path": "/private/repo",
                "api_token": "must-not-leak",
                "event_payload": {"raw": "must-not-leak"}
            }),
            telemetry: Some(SinkTelemetry {
                correlation_id: "request-123".into(),
                route_result: Some("matched".into()),
                route_index: Some(1),
                target: "http:example.test/redacted/1234".into(),
                batch_count: None,
            }),
        }
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 2048];
        let header_end;
        loop {
            let count = stream.read(&mut buffer).await.unwrap();
            assert!(count > 0, "connection closed before request headers");
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = index + 4;
                break;
            }
        }

        let headers = String::from_utf8_lossy(&bytes[..header_end]).to_string();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or_default();
        while bytes.len() < header_end + content_length {
            let count = stream.read(&mut buffer).await.unwrap();
            assert!(count > 0, "connection closed before request body");
            bytes.extend_from_slice(&buffer[..count]);
        }

        (
            headers,
            bytes[header_end..header_end + content_length].to_vec(),
        )
    }

    #[test]
    fn hmac_sha256_matches_known_vector() {
        assert_eq!(
            hmac_signature(b"key", b"The quick brown fox jumps over the lazy dog"),
            "sha256=f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn request_id_is_stable_and_header_safe() {
        let message = message();
        assert_eq!(request_id_for_message(&message), "request-123");
        assert_eq!(
            request_id_for_message(&message),
            request_id_for_message(&message)
        );

        let mut unsafe_message = message;
        unsafe_message.telemetry.as_mut().unwrap().correlation_id =
            "private request\r\nX-Injected: yes".into();
        let request_id = request_id_for_message(&unsafe_message);
        assert!(request_id.starts_with("clawhip-"));
        assert_eq!(request_id.len(), "clawhip-".len() + 64);
        assert!(
            request_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        );
        assert_eq!(request_id, request_id_for_message(&unsafe_message));
    }

    #[test]
    fn normalized_body_is_bounded_and_public_safe() {
        let target = SinkTarget::HttpEndpoint(
            "https://controller.example/webhooks/clawhip-controller?token=endpoint-secret".into(),
        );
        let mut message = message();
        message.content = format!("{} https://hooks.example/secret/token", "x".repeat(30_000));

        let body = event_body(&message, &target, "request-123").unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        let rendered = String::from_utf8(body.clone()).unwrap();

        assert!(body.len() <= MAX_EVENT_BODY_BYTES);
        assert_eq!(parsed["schema"], EVENT_SCHEMA);
        assert_eq!(parsed["request_id"], "request-123");
        assert_eq!(parsed["timestamp"], FALLBACK_EVENT_TIMESTAMP);
        assert_eq!(parsed["type"], "session.finished");
        assert_eq!(parsed["source"], "session");
        assert_eq!(parsed["payload"]["repo_name"], "clawhip");
        assert!(parsed["payload"].get("repo_path").is_none());
        assert!(parsed["payload"].get("api_token").is_none());
        assert!(parsed["payload"].get("event_payload").is_none());
        assert!(parsed["truncated"].as_bool().unwrap());
        assert!(parsed["redacted"].as_bool().unwrap());
        for secret in [
            "/private/repo",
            "must-not-leak",
            "endpoint-secret",
            "https://controller.example/webhooks/clawhip-controller",
        ] {
            assert!(!rendered.contains(secret), "body leaked {secret}");
        }
    }

    #[test]
    fn top_level_timestamp_prefers_valid_event_timestamp_then_timestamp() {
        let target = SinkTarget::HttpEndpoint("https://controller.example/events".into());
        let mut message = message();
        message.payload["event_timestamp"] = json!("2026-08-17T12:34:56+09:00");
        message.payload["timestamp"] = json!("2026-08-17T04:05:06Z");

        let body = event_body(&message, &target, "request-123").unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(parsed["timestamp"], "2026-08-17T03:34:56Z");
        assert!(parsed["timestamp"].as_str().unwrap().len() <= MAX_EVENT_TIMESTAMP_BYTES);

        message.payload["event_timestamp"] = json!("not-a-timestamp");
        let body = event_body(&message, &target, "request-123").unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(parsed["timestamp"], "2026-08-17T04:05:06Z");
    }

    #[test]
    fn top_level_timestamp_uses_stable_ingress_and_epoch_fallbacks() {
        let target = SinkTarget::HttpEndpoint("https://controller.example/events".into());
        let mut message = message();
        message.payload["event_timestamp"] = json!("invalid");
        message.payload["timestamp"] = json!({"raw": "invalid"});
        message.payload["first_seen_at"] = json!("2026-08-17T05:06:07Z");

        let body = event_body(&message, &target, "request-123").unwrap();
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["timestamp"], "2026-08-17T05:06:07Z");

        message
            .payload
            .as_object_mut()
            .unwrap()
            .remove("first_seen_at");
        let first = event_body(&message, &target, "request-123").unwrap();
        let second = event_body(&message, &target, "request-123").unwrap();
        let parsed: Value = serde_json::from_slice(&first).unwrap();

        assert_eq!(parsed["timestamp"], FALLBACK_EVENT_TIMESTAMP);
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn sends_exact_signed_body_with_stable_request_id() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, body) = read_request(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            (headers, body)
        });
        let endpoint = format!("http://{addr}/webhooks/clawhip-controller");
        let target = SinkTarget::HttpEndpoint(endpoint);
        let sink = HttpSink::new(b"shared-secret").unwrap();
        let message = message();

        let send = tokio::spawn({
            let sink = sink.clone();
            let target = target.clone();
            let message = message.clone();
            async move { sink.send(&target, &message).await }
        });
        let (headers, body) = timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();

        send.await.unwrap().unwrap();
        let expected_body = event_body(&message, &target, "request-123").unwrap();
        assert_eq!(body, expected_body);
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert!(
            headers.contains("x-request-id: request-123")
                || headers.contains("X-Request-ID: request-123")
        );
        let signature = hmac_signature(b"shared-secret", &body);
        assert!(
            headers.contains(&format!("x-hub-signature-256: {signature}"))
                || headers.contains(&format!("X-Hub-Signature-256: {signature}"))
        );
    }

    #[tokio::test]
    async fn refuses_redirects_and_does_not_contact_redirect_target() {
        let redirect_target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = redirect_target.local_addr().unwrap();
        let redirector = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirector_addr = redirector.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = redirector.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            let response = format!(
                "HTTP/1.1 302 Found\r\nlocation: http://{target_addr}/redirected\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        let sink = HttpSink::new(b"secret").unwrap();
        let endpoint = SinkTarget::HttpEndpoint(format!(
            "http://{redirector_addr}/webhooks/clawhip-controller"
        ));
        let error = sink
            .send(&endpoint, &message())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("status 302"));
        server.await.unwrap();
        assert!(
            timeout(Duration::from_millis(150), redirect_target.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn status_error_does_not_expose_endpoint_or_response_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            let body = "private-controller-error secret-token";
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let endpoint = format!("http://{addr}/webhooks/private-controller");
        let sink = HttpSink::new(b"secret").unwrap();

        let error = sink
            .send(&SinkTarget::HttpEndpoint(endpoint.clone()), &message())
            .await
            .unwrap_err()
            .to_string();
        server.await.unwrap();

        assert_eq!(error, "HTTP sink request failed with status 500");
        assert!(!error.contains(&endpoint));
        assert!(!error.contains("private-controller-error"));
        assert!(!error.contains("secret-token"));
    }

    #[tokio::test]
    async fn request_timeout_is_bounded_and_public_safe() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let endpoint = format!("http://{addr}/webhooks/private-controller");
        let sink = HttpSink::with_request_timeout(b"secret", Duration::from_millis(50)).unwrap();

        let error = sink
            .send(&SinkTarget::HttpEndpoint(endpoint.clone()), &message())
            .await
            .unwrap_err()
            .to_string();
        server.await.unwrap();

        assert_eq!(
            error,
            "HTTP sink request failed before receiving a response"
        );
        assert!(!error.contains(&endpoint));
        assert!(!error.contains("private-controller"));
    }

    #[test]
    #[serial]
    fn from_config_loads_secret_from_referenced_environment_variable() {
        let name = "CLAWHIP_TEST_HTTP_HMAC_SECRET";
        unsafe { std::env::set_var(name, "env-secret") };
        let config = HttpConfig {
            endpoint: Some("http://127.0.0.1:8644/webhooks/clawhip-controller".into()),
            hmac_secret_env: Some(name.into()),
        };

        let result = HttpSink::from_config(&config);
        unsafe { std::env::remove_var(name) };

        assert!(result.is_ok());
    }

    #[test]
    #[serial]
    fn from_config_reports_missing_secret_without_exposing_endpoint() {
        let name = "CLAWHIP_TEST_MISSING_HTTP_HMAC_SECRET";
        unsafe { std::env::remove_var(name) };
        let endpoint = "https://controller.example/webhooks/private?token=endpoint-secret";
        let config = HttpConfig {
            endpoint: Some(endpoint.into()),
            hmac_secret_env: Some(name.into()),
        };

        let error = match HttpSink::from_config(&config) {
            Ok(_) => panic!("missing secret must fail"),
            Err(error) => error.to_string(),
        };

        assert!(error.contains(name));
        assert!(!error.contains(endpoint));
        assert!(!error.contains("controller.example"));
        assert!(!error.contains("endpoint-secret"));
    }
}
