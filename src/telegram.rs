use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;

use crate::Result;
use crate::config::AppConfig;
use crate::core::circuit_breaker::CircuitBreaker;
use crate::core::dlq::{Dlq, DlqEntry};
use crate::core::rate_limit::RateLimiter;
use crate::sink::{SinkMessage, SinkTarget};

const MAX_ATTEMPTS: u32 = 3;
const JITTER_MS: u64 = 50;
const CIRCUIT_FAILURE_THRESHOLD: u32 = 3;
const CIRCUIT_COOLDOWN_SECS: u64 = 5;
/// Telegram allows up to 30 messages/second across all chats.
const GLOBAL_RATE_LIMIT_CAPACITY: u32 = 30;
const GLOBAL_RATE_LIMIT_REFILL_PER_SEC: f64 = 30.0;
/// Telegram allows at most 1 message/second to the same chat.
const CHAT_RATE_LIMIT_CAPACITY: u32 = 1;
const CHAT_RATE_LIMIT_REFILL_PER_SEC: f64 = 1.0;

#[derive(Clone)]
pub struct TelegramClient {
    http: reqwest::Client,
    bot_token: String,
    api_base: String,
    state: Arc<Mutex<TelegramState>>,
}

struct TelegramState {
    global_limiter: RateLimiter,
    chat_limiters: HashMap<String, RateLimiter>,
    circuits: HashMap<String, CircuitBreaker>,
    dlq: Dlq,
}

#[derive(Debug)]
struct TelegramSendError {
    message: String,
    retry_after: Option<Duration>,
    /// True for transient errors (5xx, network failures) that warrant a retry
    /// even without an explicit retry_after hint.
    is_transient: bool,
}

#[derive(Debug, Deserialize)]
struct TelegramErrorBody {
    parameters: Option<TelegramParameters>,
}

#[derive(Debug, Deserialize)]
struct TelegramParameters {
    retry_after: Option<u64>,
}

impl TelegramClient {
    pub fn new(bot_token: String, api_base: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            bot_token,
            api_base,
            state: Arc::new(Mutex::new(TelegramState {
                global_limiter: RateLimiter::new(
                    GLOBAL_RATE_LIMIT_CAPACITY,
                    GLOBAL_RATE_LIMIT_REFILL_PER_SEC,
                ),
                chat_limiters: HashMap::new(),
                circuits: HashMap::new(),
                dlq: Dlq::default(),
            })),
        }
    }

    pub fn from_config(config: Arc<AppConfig>) -> Result<Self> {
        let token = config
            .providers
            .telegram
            .bot_token
            .clone()
            .or_else(|| std::env::var("CLAWHIP_TELEGRAM_BOT_TOKEN").ok())
            .ok_or("Telegram bot_token not configured; set [providers.telegram].bot_token or CLAWHIP_TELEGRAM_BOT_TOKEN")?;
        let api_base = std::env::var("CLAWHIP_TELEGRAM_API_BASE")
            .unwrap_or_else(|_| "https://api.telegram.org".to_string());
        Ok(Self::new(token, api_base))
    }

    pub async fn send(&self, target: &SinkTarget, message: &SinkMessage) -> Result<()> {
        let chat_id = match target {
            SinkTarget::TelegramChat(id) => id.clone(),
            _ => return Err("TelegramClient only handles TelegramChat targets".into()),
        };

        let circuit_key = format!("telegram:chat:{chat_id}");

        if !self.allow_request(&circuit_key) {
            let error = format!("Telegram circuit open for {circuit_key}");
            self.record_dlq(target, message, MAX_ATTEMPTS, error.clone());
            return Err(error.into());
        }

        for attempt in 1..=MAX_ATTEMPTS {
            // Apply global rate limit delay.
            let global_delay = self.global_rate_limit_delay();
            if !global_delay.is_zero() {
                tokio::time::sleep(global_delay).await;
            }

            // Apply per-chat rate limit delay.
            let chat_delay = self.chat_rate_limit_delay(&chat_id);
            if !chat_delay.is_zero() {
                tokio::time::sleep(chat_delay).await;
            }

            let result = self.send_message(&chat_id, &message.content).await;

            match result {
                Ok(()) => {
                    self.record_success(&circuit_key);
                    return Ok(());
                }
                Err(error) => {
                    self.record_failure(&circuit_key);
                    if attempt < MAX_ATTEMPTS && (error.retry_after.is_some() || error.is_transient) {
                        let backoff = error.retry_after.unwrap_or_else(|| {
                            Duration::from_millis(100 * u64::from(attempt))
                        });
                        tokio::time::sleep(backoff + jitter_for_attempt(attempt)).await;
                        continue;
                    }

                    self.record_dlq(target, message, attempt, error.message.clone());
                    return Err(error.message.into());
                }
            }
        }

        // Unreachable: every loop iteration either returns or continues.
        // Kept as a safety net in case MAX_ATTEMPTS is ever set to 0.
        let error = format!("Telegram delivery exhausted retries for {circuit_key}");
        self.record_dlq(target, message, MAX_ATTEMPTS, error.clone());
        Err(error.into())
    }

    async fn send_message(
        &self,
        chat_id: &str,
        text: &str,
    ) -> std::result::Result<(), TelegramSendError> {
        let url = format!(
            "{}/bot{}/sendMessage",
            self.api_base.trim_end_matches('/'),
            self.bot_token
        );

        let response = self
            .http
            .post(&url)
            .json(&json!({
                "chat_id": chat_id,
                "text": text,
                "parse_mode": "HTML"
            }))
            .send()
            .await
            .map_err(|e| {
                // Redact the token from the error string — reqwest may include the URL.
                let msg = redact_token(&format!("Telegram API request failed: {e}"), &self.bot_token);
                TelegramSendError {
                    message: msg,
                    retry_after: None,
                    is_transient: true,
                }
            })?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let is_transient = status.is_server_error();
        Err(TelegramSendError {
            message: format!("Telegram API request failed with {status}: {body}"),
            retry_after: parse_retry_after(status, &body),
            is_transient,
        })
    }

    fn allow_request(&self, key: &str) -> bool {
        let mut state = self.state.lock().expect("telegram state lock");
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

    fn global_rate_limit_delay(&self) -> Duration {
        let mut state = self.state.lock().expect("telegram state lock");
        state.global_limiter.delay_for("global")
    }

    fn chat_rate_limit_delay(&self, chat_id: &str) -> Duration {
        let mut state = self.state.lock().expect("telegram state lock");
        state
            .chat_limiters
            .entry(chat_id.to_string())
            .or_insert_with(|| {
                RateLimiter::new(CHAT_RATE_LIMIT_CAPACITY, CHAT_RATE_LIMIT_REFILL_PER_SEC)
            })
            .delay_for(chat_id)
    }

    fn record_success(&self, key: &str) {
        let mut state = self.state.lock().expect("telegram state lock");
        state
            .circuits
            .entry(key.to_string())
            .or_insert_with(|| {
                CircuitBreaker::new(
                    CIRCUIT_FAILURE_THRESHOLD,
                    Duration::from_secs(CIRCUIT_COOLDOWN_SECS),
                )
            })
            .record_success();
    }

    fn record_failure(&self, key: &str) {
        let mut state = self.state.lock().expect("telegram state lock");
        state
            .circuits
            .entry(key.to_string())
            .or_insert_with(|| {
                CircuitBreaker::new(
                    CIRCUIT_FAILURE_THRESHOLD,
                    Duration::from_secs(CIRCUIT_COOLDOWN_SECS),
                )
            })
            .record_failure();
    }

    fn record_dlq(&self, target: &SinkTarget, message: &SinkMessage, attempts: u32, error: String) {
        let SinkTarget::TelegramChat(chat_id) = target else {
            return;
        };
        let target_key = format!("telegram:chat:{chat_id}");

        let entry = DlqEntry {
            original_topic: message.event_kind.clone(),
            retry_count: attempts,
            last_error: error,
            target: target_key,
            event_kind: message.event_kind.clone(),
            format: message.format.as_str().to_string(),
            content: message.content.clone(),
            payload: message.payload.clone(),
        };

        eprintln!(
            "clawhip dlq bury: {}",
            serde_json::to_string(&entry)
                .unwrap_or_else(|_| "{\"error\":\"dlq serialize failed\"}".to_string())
        );

        let mut state = self.state.lock().expect("telegram state lock");
        state.dlq.push(entry);
    }

    #[cfg(test)]
    fn dlq_entries(&self) -> Vec<DlqEntry> {
        self.state
            .lock()
            .expect("telegram state lock")
            .dlq
            .entries()
            .to_vec()
    }
}

fn redact_token(message: &str, token: &str) -> String {
    if token.is_empty() {
        return message.to_string();
    }
    message.replace(token, "[REDACTED]")
}

fn parse_retry_after(status: StatusCode, body: &str) -> Option<Duration> {
    if status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }

    serde_json::from_str::<TelegramErrorBody>(body)
        .ok()
        .and_then(|parsed| parsed.parameters)
        .and_then(|params| params.retry_after)
        .map(Duration::from_secs)
}

fn jitter_for_attempt(attempt: u32) -> Duration {
    Duration::from_millis(JITTER_MS * u64::from(attempt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, TelegramConfig, ProvidersConfig};
    use crate::events::MessageFormat;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn make_config_with_token(token: &str) -> Arc<AppConfig> {
        Arc::new(AppConfig {
            providers: ProvidersConfig {
                telegram: TelegramConfig {
                    bot_token: Some(token.to_string()),
                },
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn test_message() -> SinkMessage {
        SinkMessage {
            event_kind: "custom".into(),
            format: MessageFormat::Compact,
            content: "hello from clawhip".into(),
            payload: json!({}),
        }
    }

    #[test]
    fn from_config_fails_without_token() {
        // Ensure env var is not set for this test.
        unsafe { std::env::remove_var("CLAWHIP_TELEGRAM_BOT_TOKEN") };
        let config = Arc::new(AppConfig::default());
        assert!(TelegramClient::from_config(config).is_err());
    }

    #[test]
    fn from_config_succeeds_with_token_in_config() {
        let config = make_config_with_token("test-token");
        assert!(TelegramClient::from_config(config).is_ok());
    }

    #[test]
    fn from_config_succeeds_with_env_token() {
        unsafe { std::env::remove_var("CLAWHIP_TELEGRAM_BOT_TOKEN") };
        let config = Arc::new(AppConfig::default());
        unsafe { std::env::set_var("CLAWHIP_TELEGRAM_BOT_TOKEN", "env-token") };
        let result = TelegramClient::from_config(config);
        unsafe { std::env::remove_var("CLAWHIP_TELEGRAM_BOT_TOKEN") };
        assert!(result.is_ok());
    }

    #[test]
    fn parse_retry_after_for_429() {
        assert_eq!(
            parse_retry_after(
                StatusCode::TOO_MANY_REQUESTS,
                r#"{"ok":false,"error_code":429,"parameters":{"retry_after":5}}"#
            ),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            parse_retry_after(StatusCode::BAD_REQUEST, "{}"),
            None
        );
    }

    #[tokio::test]
    async fn send_message_posts_correct_payload() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);

            // Verify path contains sendMessage and token.
            assert!(request.contains("/bottest-token/sendMessage"));
            // Verify JSON body contains expected fields.
            assert!(request.contains("\"chat_id\""));
            assert!(request.contains("\"-100123\""));
            assert!(request.contains("\"parse_mode\""));
            assert!(request.contains("\"HTML\""));

            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 15\r\n\r\n{\"ok\":true}\r\n\r\n")
                .await
                .unwrap();
        });

        let client = TelegramClient::new(
            "test-token".into(),
            format!("http://{addr}"),
        );
        let message = test_message();
        client
            .send(&SinkTarget::TelegramChat("-100123".into()), &message)
            .await
            .unwrap();
        server.await.unwrap();
        assert!(client.dlq_entries().is_empty());
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
                    let body =
                        r#"{"ok":false,"error_code":429,"parameters":{"retry_after":0}}"#;
                    let response = format!(
                        "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                } else {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 15\r\n\r\n{\"ok\":true}\r\n\r\n")
                        .await
                        .unwrap();
                }
            }
        });

        let client = TelegramClient::new(
            "test-token".into(),
            format!("http://{addr}"),
        );
        let message = test_message();
        client
            .send(&SinkTarget::TelegramChat("-100123".into()), &message)
            .await
            .unwrap();
        server.await.unwrap();
        assert!(client.dlq_entries().is_empty());
    }

    #[tokio::test]
    async fn exhausted_failures_land_in_dlq() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            for _ in 0..MAX_ATTEMPTS {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0_u8; 4096];
                let _ = stream.read(&mut buf).await.unwrap();
                let body =
                    r#"{"ok":false,"error_code":429,"parameters":{"retry_after":0}}"#;
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let client = TelegramClient::new(
            "test-token".into(),
            format!("http://{addr}"),
        );
        let message = test_message();
        let result = client
            .send(&SinkTarget::TelegramChat("-100123".into()), &message)
            .await;
        server.await.unwrap();
        assert!(result.is_err());
        assert_eq!(client.dlq_entries().len(), 1);
    }

    #[tokio::test]
    async fn retries_5xx_then_succeeds() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            for idx in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0_u8; 4096];
                let _ = stream.read(&mut buf).await.unwrap();
                if idx == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n",
                        )
                        .await
                        .unwrap();
                } else {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 15\r\n\r\n{\"ok\":true}\r\n\r\n")
                        .await
                        .unwrap();
                }
            }
        });

        let client = TelegramClient::new("test-token".into(), format!("http://{addr}"));
        let message = test_message();
        client
            .send(&SinkTarget::TelegramChat("-100123".into()), &message)
            .await
            .unwrap();
        server.await.unwrap();
        assert!(client.dlq_entries().is_empty());
    }

    #[tokio::test]
    async fn error_message_redacts_bot_token() {
        // Point at a port nothing is listening on to force a connection error.
        let client = TelegramClient::new("my-secret-token".into(), "http://127.0.0.1:1".into());
        let message = test_message();
        let result = client
            .send(&SinkTarget::TelegramChat("-100123".into()), &message)
            .await;
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(
            !err_str.contains("my-secret-token"),
            "token must not appear in error message: {err_str}"
        );
    }
}
