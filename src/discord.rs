use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
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
}

#[derive(Debug, Deserialize)]
struct DiscordRateLimitBody {
    retry_after: Option<f64>,
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
        if !self.allow_request(&key) {
            let error = format!("Discord circuit open for {key}");
            self.record_dlq(target, message, MAX_ATTEMPTS, error.clone());
            return Err(error.into());
        }

        for attempt in 1..=MAX_ATTEMPTS {
            let delay = self.rate_limit_delay(&key);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }

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
                    self.record_success(&key);
                    return Ok(());
                }
                Err(error) => {
                    self.record_failure(&key);
                    if let Some(retry_after) = error.retry_after
                        && attempt < MAX_ATTEMPTS
                    {
                        tokio::time::sleep(retry_after + jitter_for_attempt(attempt)).await;
                        continue;
                    }

                    self.record_dlq(target, message, attempt, error.message.clone());
                    return Err(error.message.into());
                }
            }
        }

        let error = format!("Discord delivery exhausted retries for {key}");
        self.record_dlq(target, message, MAX_ATTEMPTS, error.clone());
        Err(error.into())
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
        let response = request.send().await.map_err(|error| DiscordSendError {
            message: format!("{label} failed: {error}"),
            retry_after: None,
        })?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(DiscordSendError {
            message: format!("{label} failed with {status}: {body}"),
            retry_after: parse_retry_after(status, &body),
        })
    }

    fn allow_request(&self, key: &str) -> bool {
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

    fn record_success(&self, key: &str) {
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
            .record_success();
    }

    fn record_failure(&self, key: &str) {
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
            .record_failure();
    }

    fn record_dlq(&self, target: &SinkTarget, message: &SinkMessage, attempts: u32, error: String) {
        let entry = DlqEntry {
            original_topic: message.event_kind.clone(),
            retry_count: attempts,
            last_error: error,
            target: target_rate_limit_key(target),
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

        let mut state = self.state.lock().expect("discord state lock");
        state.dlq.push(entry);
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
            payload: json!({"repo":"clawhip"}),
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
    }
}
