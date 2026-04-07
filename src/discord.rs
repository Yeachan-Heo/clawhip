use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Result;
use crate::config::AppConfig;
use crate::core::circuit_breaker::CircuitBreaker;
use crate::core::dlq::{Dlq, DlqEntry};
use crate::core::rate_limit::RateLimiter;
use crate::sink::{SinkMessage, SinkTarget};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DashboardSlotIds {
    pub status: Option<String>,
    pub summary: Option<String>,
    pub alert: Option<String>,
    pub activity: Option<String>,
    pub keywords: Option<String>,
}

const DASHBOARD_SEPARATOR: &str = "\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}\u{2501}";
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
    /// Tracks the last Discord message ID for edit-in-place event types.
    /// Key: "{channel_id}:{edit_key}".
    last_heartbeat_msg_ids: HashMap<String, String>,
    /// Accumulated rendered content for append-only keyword messages.
    /// Key: "{channel_id}:keywords:{session}".
    accumulated_keyword_content: HashMap<String, String>,
    /// Dashboard message IDs per "{channel}:{session}".
    dashboard_ids: HashMap<String, DashboardSlotIds>,
    /// Rolling activity log per "{channel}:{session}" -- last 5 events.
    activity_log: HashMap<String, VecDeque<String>>,
    /// Rolling keyword hit log per "{channel}:{session}".
    keyword_log: HashMap<String, VecDeque<String>>,
    /// Path to dashboard persistence file.
    dashboard_path: Option<PathBuf>,
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

        let dashboard_path = std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".clawhip").join("dashboard.json"));
        let dashboard_ids: HashMap<String, DashboardSlotIds> = dashboard_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        Ok(Self {
            bot_client,
            webhook_client,
            api_base,
            state: Arc::new(Mutex::new(DiscordState {
                limiter: RateLimiter::new(RATE_LIMIT_CAPACITY, RATE_LIMIT_REFILL_PER_SEC),
                circuits: HashMap::new(),
                dlq: Dlq::default(),
                last_heartbeat_msg_ids: HashMap::new(),
                accumulated_keyword_content: HashMap::new(),
                dashboard_ids,
                activity_log: HashMap::new(),
                keyword_log: HashMap::new(),
                dashboard_path,
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
                    let session = message.payload["session"].as_str().unwrap_or("unknown");
                    let dashboard_slot = message.payload["dashboard_component"].as_str();

                    if let Some(slot) = dashboard_slot {
                        let ts = time::OffsetDateTime::now_utc()
                            .format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_default();
                        let dashboard_content = if slot == "keywords" {
                            let entry = format!("`{ts}` {}", truncate_keyword_entry(&message.content));
                            let log_text = self.push_keyword_log(channel_id, session, &entry);
                            format!("\u{1f511} `{session}` \u{00b7} Keyword Hits\n{DASHBOARD_SEPARATOR}\n{log_text}")
                        } else if slot == "alert"
                            && message.payload["resolved"].as_bool().unwrap_or(false)
                        {
                            format!("✅ `{session}` — Input received, continuing...")
                        } else {
                            render_dashboard_slot(
                                slot,
                                session,
                                &message.event_kind,
                                &message.content,
                            )
                        };

                        let activity_entry =
                            format!("`{ts}` {} -- {}", message.event_kind, truncate_activity(&message.content));
                        let activity_text = self.push_activity(channel_id, session, &activity_entry);
                        let activity_content = format!(
                            "\u{1f4dc} `{session}` \u{00b7} Recent Activity\n{DASHBOARD_SEPARATOR}\n{activity_text}"
                        );
                        let should_pin_activity = message.payload["pin_activity"].as_bool().unwrap_or(true);
                        let _ = self
                            .update_dashboard_slot(channel_id, session, "activity", &activity_content, should_pin_activity)
                            .await;

                        self.update_dashboard_slot(channel_id, session, slot, &dashboard_content, true)
                            .await
                    } else {
                        match message.event_kind.as_str() {
                            "tmux.session_ended" => {
                                self.clear_session_dashboard(channel_id, session).await;
                                return Ok(());
                            }
                            "tmux.heartbeat" => {
                                self.send_or_edit_keyed(
                                    channel_id,
                                    &format!("heartbeat:{session}"),
                                    &message.content,
                                )
                                .await
                            }
                            "tmux.waiting_for_input" => {
                                let content = if message.payload["resolved"].as_bool().unwrap_or(false) {
                                    format!("✅ `{session}` — Input received, continuing...")
                                } else {
                                    message.content.clone()
                                };
                                self.send_or_edit_keyed(
                                    channel_id,
                                    &format!("waiting:{session}"),
                                    &content,
                                )
                                .await
                            }
                            "tmux.content_changed"
                                if message.payload["content_mode"].as_str() == Some("raw") =>
                            {
                                self.send_or_edit_keyed(
                                    channel_id,
                                    &format!("raw:{session}"),
                                    &message.content,
                                )
                                .await
                            }
                            "tmux.stale" => {
                                self.send_or_edit_keyed(
                                    channel_id,
                                    &format!("stale:{session}"),
                                    &message.content,
                                )
                                .await
                            }
                            "tmux.keyword" => {
                                self.append_keyword_keyed(
                                    channel_id,
                                    &format!("keywords:{session}"),
                                    &message.content,
                                )
                                .await
                            }
                            _ => self.send_message(channel_id, &message.content).await,
                        }
                    }
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
            client.post(url).json(&json!({ "content": truncate_discord(content) })),
            "Discord API request",
        )
        .await
    }

    /// Send a message and return the Discord message ID on success.
    async fn send_message_returning_id(
        &self,
        channel_id: &str,
        content: &str,
    ) -> std::result::Result<Option<String>, DiscordSendError> {
        let url = format!(
            "{}/channels/{}/messages",
            self.api_base.trim_end_matches('/'),
            channel_id
        );
        let client = self.bot_client.as_ref().ok_or_else(|| DiscordSendError {
            message: "missing Discord bot token for channel delivery".to_string(),
            retry_after: None,
        })?;
        self.execute_request_returning_id(
            client.post(url).json(&json!({ "content": truncate_discord(content) })),
            "Discord API request",
        )
        .await
    }

    /// Edit an existing Discord message.
    async fn edit_message(
        &self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> std::result::Result<(), DiscordSendError> {
        let url = format!(
            "{}/channels/{}/messages/{}",
            self.api_base.trim_end_matches('/'),
            channel_id,
            message_id
        );
        let client = self.bot_client.as_ref().ok_or_else(|| DiscordSendError {
            message: "missing Discord bot token for channel delivery".to_string(),
            retry_after: None,
        })?;
        self.execute_request(
            client.patch(url).json(&json!({ "content": truncate_discord(content) })),
            "Discord edit request",
        )
        .await
    }

    /// For edit-in-place event types (heartbeat, raw, waiting): edit the previous
    /// message for this key if possible, otherwise post a new one and store the ID.
    /// `edit_key` is a logical key like "heartbeat:session-name" or "content:session-name".
    async fn send_or_edit_keyed(
        &self,
        channel_id: &str,
        edit_key: &str,
        content: &str,
    ) -> std::result::Result<(), DiscordSendError> {
        let key = format!("{channel_id}:{edit_key}");
        let existing_id = {
            let state = self.state.lock().expect("discord state lock");
            state.last_heartbeat_msg_ids.get(&key).cloned()
        };

        if let Some(msg_id) = existing_id {
            match self.edit_message(channel_id, &msg_id, content).await {
                Ok(()) => return Ok(()),
                Err(e) if e.message.contains("404") || e.message.contains("Unknown Message") => {
                    // Message was deleted; fall through to create a new one
                }
                Err(e) => return Err(e),
            }
        }

        let msg_id = self.send_message_returning_id(channel_id, content).await?;
        if let Some(id) = msg_id {
            let mut state = self.state.lock().expect("discord state lock");
            state.last_heartbeat_msg_ids.insert(key, id);
        }
        Ok(())
    }

    /// Append new keyword hits to an existing message, or post a new one if none exists.
    /// When appended content would exceed the Discord limit, starts a fresh message.
    async fn append_keyword_keyed(
        &self,
        channel_id: &str,
        edit_key: &str,
        new_content: &str,
    ) -> std::result::Result<(), DiscordSendError> {
        let key = format!("{channel_id}:{edit_key}");
        let (existing_id, accumulated) = {
            let state = self.state.lock().expect("discord state lock");
            let id = state.last_heartbeat_msg_ids.get(&key).cloned();
            let acc = state
                .accumulated_keyword_content
                .get(&key)
                .cloned()
                .unwrap_or_default();
            (id, acc)
        };

        let updated = if accumulated.is_empty() {
            new_content.to_string()
        } else {
            format!("{accumulated}\n{new_content}")
        };
        // If appending would overflow, start a fresh message
        let (content_to_send, is_continuation) = if updated.len() > 1990 {
            (new_content.to_string(), false)
        } else {
            (updated, true)
        };
        let content_to_send = truncate_discord(&content_to_send).to_string();

        if is_continuation {
            if let Some(msg_id) = existing_id {
                match self.edit_message(channel_id, &msg_id, &content_to_send).await {
                    Ok(()) => {
                        let mut state = self.state.lock().expect("discord state lock");
                        state
                            .accumulated_keyword_content
                            .insert(key, content_to_send);
                        return Ok(());
                    }
                    Err(e)
                        if e.message.contains("404")
                            || e.message.contains("Unknown Message") => {}
                    Err(e) => return Err(e),
                }
            }
        }

        let msg_id = self
            .send_message_returning_id(channel_id, &content_to_send)
            .await?;
        if let Some(id) = msg_id {
            let mut state = self.state.lock().expect("discord state lock");
            state.last_heartbeat_msg_ids.insert(key.clone(), id);
            state
                .accumulated_keyword_content
                .insert(key, content_to_send);
        }
        Ok(())
    }

    async fn send_webhook(
        &self,
        webhook_url: &str,
        content: &str,
    ) -> std::result::Result<(), DiscordSendError> {
        self.execute_request(
            self.webhook_client
                .post(webhook_url_with_wait(webhook_url))
                .json(&json!({ "content": truncate_discord(content) })),
            "Discord webhook request",
        )
        .await
    }

    async fn execute_request_returning_id(
        &self,
        request: reqwest::RequestBuilder,
        label: &str,
    ) -> std::result::Result<Option<String>, DiscordSendError> {
        let response = request.send().await.map_err(|error| DiscordSendError {
            message: format!("{label} failed: {error}"),
            retry_after: None,
        })?;

        if response.status().is_success() {
            let body = response
                .json::<serde_json::Value>()
                .await
                .ok();
            return Ok(body.and_then(|v| v["id"].as_str().map(str::to_string)));
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(DiscordSendError {
            message: format!("{label} failed with {status}: {body}"),
            retry_after: parse_retry_after(status, &body),
        })
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

    /// Pin a message in a Discord channel.
    async fn pin_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> std::result::Result<(), DiscordSendError> {
        let url = format!(
            "{}/channels/{}/pins/{}",
            self.api_base.trim_end_matches('/'),
            channel_id,
            message_id
        );
        let client = self.bot_client.as_ref().ok_or_else(|| DiscordSendError {
            message: "missing Discord bot token for pinning".to_string(),
            retry_after: None,
        })?;
        self.execute_request(client.put(url).json(&json!({})), "Discord pin request")
            .await
    }

    async fn unpin_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> std::result::Result<(), DiscordSendError> {
        let url = format!(
            "{}/channels/{}/pins/{}",
            self.api_base.trim_end_matches('/'),
            channel_id,
            message_id
        );
        let client = self.bot_client.as_ref().ok_or_else(|| DiscordSendError {
            message: "missing Discord bot token for unpinning".to_string(),
            retry_after: None,
        })?;
        self.execute_request(client.delete(url), "Discord unpin request")
            .await
    }

    /// Unpin all dashboard messages for a session and clear its slot IDs.
    async fn clear_session_dashboard(&self, channel_id: &str, session: &str) {
        let slot_key = format!("{channel_id}:{session}");

        // Collect message IDs while holding the lock, then release before async calls
        let ids_to_unpin = {
            let state = self.state.lock().expect("discord state lock");
            let Some(ids) = state.dashboard_ids.get(&slot_key) else {
                return;
            };
            [
                ids.status.clone(),
                ids.summary.clone(),
                ids.alert.clone(),
                ids.activity.clone(),
                ids.keywords.clone(),
            ]
        };

        for msg_id in ids_to_unpin.into_iter().flatten() {
            let _ = self.unpin_message(channel_id, &msg_id).await;
        }

        // Clear the IDs and persist
        {
            let mut state = self.state.lock().expect("discord state lock");
            state.dashboard_ids.remove(&slot_key);
            // Also clear in-memory keyword and activity logs for this session
            state.keyword_log.retain(|k, _| !k.starts_with(&format!("{channel_id}:{session}")));
            state.activity_log.retain(|k, _| !k.starts_with(&format!("{channel_id}:{session}")));
        }
        self.persist_dashboard();
    }

    /// Persist dashboard message IDs to disk.
    fn persist_dashboard(&self) {
        let state = self.state.lock().expect("discord state lock");
        let Some(ref path) = state.dashboard_path else {
            return;
        };
        if let Ok(json) = serde_json::to_string_pretty(&state.dashboard_ids) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Update (or create) a pinned dashboard slot message.
    async fn update_dashboard_slot(
        &self,
        channel_id: &str,
        session: &str,
        slot: &str,
        content: &str,
        should_pin: bool,
    ) -> std::result::Result<(), DiscordSendError> {
        let slot_key = format!("{channel_id}:{session}");

        let existing_id = {
            let state = self.state.lock().expect("discord state lock");
            state
                .dashboard_ids
                .get(&slot_key)
                .and_then(|ids| match slot {
                    "status" => ids.status.clone(),
                    "summary" => ids.summary.clone(),
                    "alert" => ids.alert.clone(),
                    "activity" => ids.activity.clone(),
                    "keywords" => ids.keywords.clone(),
                    _ => None,
                })
        };

        if let Some(msg_id) = existing_id {
            match self.edit_message(channel_id, &msg_id, content).await {
                Ok(()) => return Ok(()),
                Err(e) if e.message.contains("404") || e.message.contains("Unknown Message") => {
                    let mut state = self.state.lock().expect("discord state lock");
                    if let Some(ids) = state.dashboard_ids.get_mut(&slot_key) {
                        match slot {
                            "status" => ids.status = None,
                            "summary" => ids.summary = None,
                            "alert" => ids.alert = None,
                            "activity" => ids.activity = None,
                            "keywords" => ids.keywords = None,
                            _ => {}
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }

        let new_id = self.send_message_returning_id(channel_id, content).await?;
        if let Some(id) = new_id {
            if should_pin {
                let _ = self.pin_message(channel_id, &id).await;
            }
            {
                let mut state = self.state.lock().expect("discord state lock");
                let ids = state.dashboard_ids.entry(slot_key).or_default();
                match slot {
                    "status" => ids.status = Some(id),
                    "summary" => ids.summary = Some(id),
                    "alert" => ids.alert = Some(id),
                    "activity" => ids.activity = Some(id),
                    "keywords" => ids.keywords = Some(id),
                    _ => {}
                }
            }
            self.persist_dashboard();
        }
        Ok(())
    }

    /// Push an entry to the rolling activity log and return the combined log text.
    fn push_activity(&self, channel_id: &str, session: &str, entry: &str) -> String {
        let key = format!("{channel_id}:{session}");
        let mut state = self.state.lock().expect("discord state lock");
        let log = state.activity_log.entry(key).or_default();
        if log.len() >= 5 {
            log.pop_front();
        }
        log.push_back(entry.to_string());
        log.iter().cloned().collect::<Vec<_>>().join("\n")
    }

    /// Push an entry to the rolling keyword log. Drops oldest entries when total chars exceed 1800.
    fn push_keyword_log(&self, channel_id: &str, session: &str, entry: &str) -> String {
        let key = format!("{channel_id}:{session}");
        let mut state = self.state.lock().expect("discord state lock");
        let log = state.keyword_log.entry(key).or_default();
        log.push_back(entry.to_string());
        // Drop oldest entries until total content fits within ~1800 chars
        while log.iter().map(|e| e.len() + 1).sum::<usize>() > 1800 && log.len() > 1 {
            log.pop_front();
        }
        log.iter().cloned().collect::<Vec<_>>().join("\n")
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

/// Truncate content to Discord's 2000-char message limit, appending "…" if clipped.
fn truncate_discord(content: &str) -> &str {
    const LIMIT: usize = 1990;
    if content.len() <= LIMIT {
        return content;
    }
    // Walk back to a char boundary
    let mut end = LIMIT;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    &content[..end]
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

/// Render dashboard-formatted content for a given slot.
fn render_dashboard_slot(slot: &str, session: &str, event_kind: &str, content: &str) -> String {
    match slot {
        "status" => {
            let status_icon = if event_kind == "tmux.stale" {
                "\u{23f1}\u{fe0f} Stale"
            } else {
                "\u{1f493} Active"
            };
            let body = truncate_dashboard(content, 1500);
            format!("\u{1f4ca} `{session}` \u{00b7} Status\n{DASHBOARD_SEPARATOR}\n{status_icon}\n{body}")
        }
        "summary" => {
            let body = truncate_dashboard(content, 1500);
            format!("\u{1f4cb} `{session}` \u{00b7} Latest Output\n{DASHBOARD_SEPARATOR}\n{body}")
        }
        "alert" => {
            let body = truncate_dashboard(content, 1500);
            format!("\u{1f6a8} `{session}` \u{00b7} **WAITING FOR INPUT**\n{DASHBOARD_SEPARATOR}\n{body}")
        }
        _ => content.to_string(),
    }
}

/// Truncate content for dashboard slots to stay within Discord limits.
fn truncate_dashboard(content: &str, max_len: usize) -> String {
    if content.len() <= max_len {
        content.to_string()
    } else {
        let mut end = max_len;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\u{2026}", &content[..end])
    }
}

/// Truncate a single activity log entry to keep the rolling log compact.
/// Truncate for keywords rolling log entries — preserves all lines (for multi-hit aggregated
/// events), capping total entry length to 300 chars.
fn truncate_keyword_entry(content: &str) -> String {
    if content.len() <= 300 {
        return content.to_string();
    }
    let mut end = 300;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\u{2026}", &content[..end])
}

fn truncate_activity(content: &str) -> String {
    let first_line = content.lines().next().unwrap_or(content);
    if first_line.len() > 120 {
        let mut end = 120;
        while !first_line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\u{2026}", &first_line[..end])
    } else {
        first_line.to_string()
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

    #[test]
    fn push_keyword_log_accumulates_and_evicts_oldest() {
        let config = Arc::new(AppConfig::default());
        let client = DiscordClient::from_config(config).unwrap();

        // Small entries accumulate
        let r1 = client.push_keyword_log("ch1", "sess", "entry1");
        assert_eq!(r1, "entry1");
        let r2 = client.push_keyword_log("ch1", "sess", "entry2");
        assert_eq!(r2, "entry1\nentry2");

        // Large entry forces eviction of oldest
        let big = "x".repeat(1000);
        client.push_keyword_log("ch1", "sess", &big);
        let r4 = client.push_keyword_log("ch1", "sess", &big);
        // After adding two ~1000-char entries, total > 1800 so oldest should be evicted
        assert!(!r4.starts_with("entry1"), "oldest entry should have been evicted");
    }

    #[test]
    fn render_dashboard_slot_keywords_falls_through_to_content() {
        // keywords slot content is built inline in send(), render_dashboard_slot is not used for it
        let result = render_dashboard_slot("keywords", "test-session", "tmux.keyword", "some hits");
        assert_eq!(result, "some hits");
    }
}
