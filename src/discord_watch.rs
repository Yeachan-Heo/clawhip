use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Result;
use crate::config::DiscordWatchConfig;
use crate::events::IncomingEvent;

const REASON_T1_PENDING_MENTIONS: &str = "t1-pending-mentions";
const REASON_T2_UNANSWERED_DIRECT_MENTION: &str = "t2-unanswered-direct-mention";
const REASON_T3_CHANNEL_BACKLOG: &str = "t3-channel-backlog";
const REASON_OWNER_DIRECT_MESSAGE: &str = "owner-direct-message";
const REASON_OWNER_WATCHED_MESSAGE: &str = "owner-watched-message";
const REASON_DIRECT_GAEBAL_MENTION: &str = "direct-gaebal-mention";
const REASON_KEYWORD_SIGNAL: &str = "keyword-signal";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordMessageCreateEvent {
    pub message_id: String,
    pub channel_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guild_id: Option<String>,
    pub author_id: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub mentions: Vec<String>,
    #[serde(default)]
    pub direct_message: bool,
    #[serde(default)]
    pub author_is_owner: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NudgeIntent {
    pub id: String,
    pub created_at_ms: i64,
    pub reasons: Vec<String>,
    pub source_channel_id: String,
    pub source_channel_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nudge_target_channel_id: Option<String>,
    pub content: String,
    pub local_only: bool,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct DiscordWatchState {
    #[serde(default)]
    pub channels: BTreeMap<String, ChannelState>,
    #[serde(default)]
    pub pending_mentions: BTreeMap<String, PendingMention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_last_nudge_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ChannelState {
    #[serde(default)]
    pub channel_name: Option<String>,
    #[serde(default)]
    pub new_messages_since_gaebal: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_gaebal_activity_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_nudge_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_message_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingMention {
    pub message_id: String,
    pub channel_id: String,
    pub author_id: String,
    pub first_seen_at_ms: i64,
    pub direct: bool,
}

pub fn process_incoming_event(
    config: &DiscordWatchConfig,
    state: &mut DiscordWatchState,
    event: &IncomingEvent,
    now_ms: i64,
) -> Result<Option<NudgeIntent>> {
    if !config.enabled || event.canonical_kind() != "discord.message-create" {
        return Ok(None);
    }
    let message: DiscordMessageCreateEvent = serde_json::from_value(event.payload.clone())?;
    Ok(process_message(config, state, &message, now_ms))
}

pub fn process_message(
    config: &DiscordWatchConfig,
    state: &mut DiscordWatchState,
    message: &DiscordMessageCreateEvent,
    now_ms: i64,
) -> Option<NudgeIntent> {
    if message.author_id == config.gaebal_gajae_user_id {
        reset_channel_for_gaebal_activity(state, &message.channel_id, now_ms);
        return None;
    }

    if is_banned(config, message) {
        return None;
    }

    let owner_direct = message.direct_message && is_owner(config, message);
    let mentions_gaebal = message
        .mentions
        .iter()
        .any(|id| id == &config.gaebal_gajae_user_id)
        || message
            .content
            .contains(&format!("<@{}>", config.gaebal_gajae_user_id));
    let watched = watched_channel_name(config, &message.channel_id);
    let direct_gaebal_mention = message.direct_message && mentions_gaebal;
    if watched.is_none() && !owner_direct && !direct_gaebal_mention {
        return None;
    }

    let channel_name = watched.clone().unwrap_or_else(|| {
        message
            .channel_name
            .clone()
            .unwrap_or_else(|| "direct-message".to_string())
    });

    if watched_channel_name(config, &message.channel_id).is_some() {
        let count_for_t3 = !in_cooldown(config, state, &message.channel_id, now_ms);
        let channel = state
            .channels
            .entry(message.channel_id.clone())
            .or_default();
        channel.channel_name = Some(channel_name.clone());
        let new_message = record_recent_message_id(channel, &message.message_id);
        if count_for_t3 && new_message {
            channel.new_messages_since_gaebal = channel.new_messages_since_gaebal.saturating_add(1);
        }
    }

    if mentions_gaebal {
        state
            .pending_mentions
            .entry(message.message_id.clone())
            .or_insert(PendingMention {
                message_id: message.message_id.clone(),
                channel_id: message.channel_id.clone(),
                author_id: message.author_id.clone(),
                first_seen_at_ms: message.timestamp_ms.unwrap_or(now_ms),
                direct: message.direct_message,
            });
    }

    let mut reasons = BTreeSet::new();
    if owner_direct {
        reasons.insert(REASON_OWNER_DIRECT_MESSAGE.to_string());
    } else if is_owner(config, message) && watched.is_some() {
        reasons.insert(REASON_OWNER_WATCHED_MESSAGE.to_string());
    }

    if direct_gaebal_mention {
        reasons.insert(REASON_DIRECT_GAEBAL_MENTION.to_string());
    }

    if watched.is_some() && contains_keyword_signal(&message.content) {
        reasons.insert(REASON_KEYWORD_SIGNAL.to_string());
    }

    let pending_watched_count = state
        .pending_mentions
        .values()
        .filter(|pending| watched_channel_name(config, &pending.channel_id).is_some())
        .count() as u64;
    if pending_watched_count >= config.pending_mentions_threshold {
        reasons.insert(REASON_T1_PENDING_MENTIONS.to_string());
    }

    if state.pending_mentions.values().any(|pending| {
        pending.direct
            && now_ms.saturating_sub(pending.first_seen_at_ms) >= config.direct_mention_persist_ms
    }) {
        reasons.insert(REASON_T2_UNANSWERED_DIRECT_MENTION.to_string());
    }

    if state
        .channels
        .get(&message.channel_id)
        .is_some_and(|channel| {
            channel.new_messages_since_gaebal >= config.channel_message_threshold
        })
    {
        reasons.insert(REASON_T3_CHANNEL_BACKLOG.to_string());
    }

    if reasons.is_empty() || in_cooldown(config, state, &message.channel_id, now_ms) {
        return None;
    }

    state.global_last_nudge_at_ms = Some(now_ms);
    let reasons = reasons.into_iter().collect::<Vec<_>>();
    let channel = state
        .channels
        .entry(message.channel_id.clone())
        .or_default();
    channel.channel_name = Some(channel_name.clone());
    channel.last_nudge_at_ms = Some(now_ms);
    if reasons
        .iter()
        .any(|reason| reason == REASON_T3_CHANNEL_BACKLOG)
    {
        channel.new_messages_since_gaebal = 0;
    }

    let content = render_nudge_content(config, &message.channel_id, &channel_name);
    let id = format!(
        "discord-watch-{}-{}-{}",
        message.channel_id,
        message.message_id,
        reasons.join("+")
    );
    Some(NudgeIntent {
        id,
        created_at_ms: now_ms,
        reasons,
        source_channel_id: message.channel_id.clone(),
        source_channel_name: channel_name,
        nudge_target_channel_id: config.nudge_target_channel_id.clone(),
        content,
        local_only: true,
        metadata: BTreeMap::from([
            (
                "contract".to_string(),
                "discord-watch.nudge-intent.v1".to_string(),
            ),
            ("delivery".to_string(), "local-only".to_string()),
        ]),
    })
}

pub fn render_nudge_content(
    config: &DiscordWatchConfig,
    channel_id: &str,
    channel_name: &str,
) -> String {
    config
        .doctrine_template
        .replace("{channel_id}", channel_id)
        .replace("{channel_name}", channel_name)
}

pub fn load_state(path: &Path) -> Result<DiscordWatchState> {
    if !path.exists() {
        return Ok(DiscordWatchState::default());
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(DiscordWatchState::default());
    }
    Ok(serde_json::from_str(&raw)?)
}

pub fn save_state(path: &Path, state: &DiscordWatchState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp_path = state_temp_path(path);
    let payload = serde_json::to_string_pretty(state)?;
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        file.write_all(payload.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)?;
        sync_parent_dir(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn state_temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .unwrap_or("discord-watch-state.json");
    path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()))
}

fn sync_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        let dir = OpenOptions::new().read(true).open(parent)?;
        dir.sync_all()?;
    }
    Ok(())
}

pub fn append_intent(path: &Path, intent: &NudgeIntent) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let existed = path.exists();
    if intent_already_persisted(path, &intent.id)? {
        return Ok(());
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", serde_json::to_string(intent)?)?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    if !existed {
        sync_parent_dir(path)?;
    }
    Ok(())
}

fn intent_already_persisted(path: &Path, intent_id: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let raw = fs::read_to_string(path)?;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("id").and_then(serde_json::Value::as_str) == Some(intent_id) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn default_state_path(cron_state_path: &Path) -> PathBuf {
    cron_state_path.with_file_name("discord-watch-state.json")
}

pub fn default_intents_path(cron_state_path: &Path) -> PathBuf {
    cron_state_path.with_file_name("discord-watch-intents.jsonl")
}

pub fn handle_local_intent_event(
    config: &DiscordWatchConfig,
    cron_state_path: &Path,
    event: &IncomingEvent,
    now_ms: i64,
) -> Result<Option<NudgeIntent>> {
    if !config.enabled || event.canonical_kind() != "discord.message-create" {
        return Ok(None);
    }
    let state_path = config
        .state_file
        .clone()
        .unwrap_or_else(|| default_state_path(cron_state_path));
    let intents_path = config
        .intent_file
        .clone()
        .unwrap_or_else(|| default_intents_path(cron_state_path));
    let mut state = load_state(&state_path)?;
    let intent = process_incoming_event(config, &mut state, event, now_ms)?;
    if let Some(intent) = intent.as_ref() {
        append_intent(&intents_path, intent)?;
    }
    save_state(&state_path, &state)?;
    Ok(intent)
}

fn reset_channel_for_gaebal_activity(state: &mut DiscordWatchState, channel_id: &str, now_ms: i64) {
    let channel = state.channels.entry(channel_id.to_string()).or_default();
    channel.new_messages_since_gaebal = 0;
    channel.last_gaebal_activity_at_ms = Some(now_ms);
    state
        .pending_mentions
        .retain(|_, pending| pending.channel_id != channel_id);
}

fn in_cooldown(
    config: &DiscordWatchConfig,
    state: &DiscordWatchState,
    channel_id: &str,
    now_ms: i64,
) -> bool {
    if state
        .global_last_nudge_at_ms
        .is_some_and(|last| now_ms.saturating_sub(last) < config.global_cooldown_ms)
    {
        return true;
    }
    state
        .channels
        .get(channel_id)
        .and_then(|channel| channel.last_nudge_at_ms)
        .is_some_and(|last| now_ms.saturating_sub(last) < config.channel_cooldown_ms)
}

fn is_owner(config: &DiscordWatchConfig, message: &DiscordMessageCreateEvent) -> bool {
    message.author_is_owner
        || config
            .owner_user_ids
            .iter()
            .any(|id| id == &message.author_id)
}

fn is_banned(config: &DiscordWatchConfig, message: &DiscordMessageCreateEvent) -> bool {
    config
        .banned_channel_ids
        .iter()
        .any(|id| id == &message.channel_id)
        || message
            .channel_name
            .as_ref()
            .into_iter()
            .chain(watched_channel_name(config, &message.channel_id).as_ref())
            .any(|name| is_banned_channel_name(config, name))
}

fn is_banned_channel_name(config: &DiscordWatchConfig, name: &str) -> bool {
    let normalized = name.trim().trim_start_matches('#');
    config.banned_channel_names.iter().any(|banned| {
        banned
            .trim()
            .trim_start_matches('#')
            .eq_ignore_ascii_case(normalized)
    })
}

fn contains_keyword_signal(content: &str) -> bool {
    let normalized = content.to_ascii_lowercase();
    [
        "?", "question", "error", "failed", "failure", "setup", "install", "help",
    ]
    .iter()
    .any(|keyword| normalized.contains(keyword))
}

fn record_recent_message_id(channel: &mut ChannelState, message_id: &str) -> bool {
    const MAX_RECENT_MESSAGE_IDS: usize = 256;
    if channel.recent_message_ids.iter().any(|id| id == message_id) {
        return false;
    }
    channel.recent_message_ids.push(message_id.to_string());
    let excess = channel
        .recent_message_ids
        .len()
        .saturating_sub(MAX_RECENT_MESSAGE_IDS);
    if excess > 0 {
        channel.recent_message_ids.drain(..excess);
    }
    true
}

fn watched_channel_name(config: &DiscordWatchConfig, channel_id: &str) -> Option<String> {
    config
        .watched_channels
        .iter()
        .find(|channel| channel.id == channel_id)
        .map(|channel| channel.name.clone())
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn intent_to_local_event(intent: &NudgeIntent) -> IncomingEvent {
    IncomingEvent {
        kind: "discord-watch.nudge-intent".to_string(),
        channel: None,
        mention: None,
        format: None,
        template: None,
        payload: json!(intent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DiscordWatchChannel, DiscordWatchConfig};
    use serde_json::json;
    use tempfile::tempdir;

    fn config() -> DiscordWatchConfig {
        DiscordWatchConfig {
            enabled: true,
            global_cooldown_ms: 1_000,
            channel_cooldown_ms: 1_000,
            owner_user_ids: vec!["owner".into()],
            gaebal_gajae_user_id: "fixture-gaebal".into(),
            nudge_target_channel_id: Some("fixture-nudge-target".into()),
            watched_channels: vec![
                DiscordWatchChannel {
                    id: "fixture-general".into(),
                    name: "general".into(),
                },
                DiscordWatchChannel {
                    id: "fixture-general-ko".into(),
                    name: "general-ko".into(),
                },
            ],
            ..DiscordWatchConfig::default()
        }
    }

    fn msg(id: &str, channel_id: &str, author_id: &str, now_ms: i64) -> DiscordMessageCreateEvent {
        DiscordMessageCreateEvent {
            message_id: id.into(),
            channel_id: channel_id.into(),
            channel_name: Some("general".into()),
            guild_id: Some("fixture-guild".into()),
            author_id: author_id.into(),
            content: String::new(),
            mentions: Vec::new(),
            direct_message: false,
            author_is_owner: false,
            timestamp_ms: Some(now_ms),
        }
    }

    #[test]
    fn owner_message_in_watched_channel_triggers_immediate_intent() {
        let cfg = config();
        let mut state = DiscordWatchState::default();
        let intent = process_message(
            &cfg,
            &mut state,
            &msg("owner-watch", "fixture-general", "owner", 0),
            0,
        )
        .expect("owner watched message intent");

        assert_eq!(intent.reasons, vec![REASON_OWNER_WATCHED_MESSAGE]);
    }

    #[test]
    fn direct_gaebal_mention_triggers_immediate_intent() {
        let cfg = config();
        let mut state = DiscordWatchState::default();
        let mut direct = msg("dm-mention", "dm", "user", 0);
        direct.direct_message = true;
        direct.channel_name = Some("dm".into());
        direct.mentions.push(cfg.gaebal_gajae_user_id.clone());

        let intent =
            process_message(&cfg, &mut state, &direct, 0).expect("direct gaebal mention intent");

        assert!(
            intent
                .reasons
                .contains(&REASON_DIRECT_GAEBAL_MENTION.to_string())
        );
    }

    #[test]
    fn question_error_setup_keywords_are_additive_signals_and_respect_cooldown() {
        let mut cfg = config();
        cfg.global_cooldown_ms = 1_000;
        cfg.channel_cooldown_ms = 1_000;
        let mut state = DiscordWatchState::default();
        let mut first = msg("kw1", "fixture-general", "user", 0);
        first.content = "setup failed? need help".into();
        let intent = process_message(&cfg, &mut state, &first, 0).expect("keyword intent");
        assert_eq!(intent.reasons, vec![REASON_KEYWORD_SIGNAL]);

        let mut second = msg("kw2", "fixture-general", "user", 1);
        second.content = "error installing".into();
        assert!(
            process_message(&cfg, &mut state, &second, 1).is_none(),
            "keyword signals must respect cooldown"
        );
    }

    #[test]
    fn t1_fires_at_five_pending_mentions_not_four() {
        let cfg = config();
        let mut state = DiscordWatchState::default();
        for i in 0..4 {
            let mut m = msg(&format!("m{i}"), "fixture-general", "user", i);
            m.mentions.push(cfg.gaebal_gajae_user_id.clone());
            assert!(process_message(&cfg, &mut state, &m, i).is_none());
        }
        let mut fifth = msg("m5", "fixture-general-ko", "user", 5);
        fifth.channel_name = Some("general-ko".into());
        fifth.mentions.push(cfg.gaebal_gajae_user_id.clone());
        let intent = process_message(&cfg, &mut state, &fifth, 5).expect("t1 intent");
        assert_eq!(intent.reasons, vec![REASON_T1_PENDING_MENTIONS]);
        assert!(
            process_message(
                &cfg,
                &mut state,
                &msg("m6", "fixture-general-ko", "user", 6),
                6
            )
            .is_none()
        );
    }

    #[test]
    fn t2_waits_three_minutes_and_reply_clears_pending() {
        let cfg = config();
        let mut state = DiscordWatchState::default();
        let mut direct = msg("dm1", "dm", "user", 0);
        direct.channel_name = Some("owner-dm".into());
        direct.direct_message = true;
        direct.mentions.push(cfg.gaebal_gajae_user_id.clone());
        let immediate = process_message(&cfg, &mut state, &direct, 120_000)
            .expect("direct mention immediate intent");
        assert!(
            immediate
                .reasons
                .contains(&REASON_DIRECT_GAEBAL_MENTION.to_string())
        );
        let intent = process_message(
            &cfg,
            &mut state,
            &msg("tick", "fixture-general", "user", 180_000),
            180_000,
        )
        .expect("t2 intent");
        assert!(
            intent
                .reasons
                .contains(&REASON_T2_UNANSWERED_DIRECT_MENTION.to_string())
        );
        let reply = msg("reply", "dm", &cfg.gaebal_gajae_user_id, 181_000);
        assert!(process_message(&cfg, &mut state, &reply, 181_000).is_none());
        assert!(state.pending_mentions.is_empty());
        assert!(
            process_message(
                &cfg,
                &mut state,
                &msg("later", "fixture-general", "user", 300_000),
                300_000
            )
            .is_none()
        );
    }

    #[test]
    fn t3_counts_per_channel_and_respects_cooldown() {
        let cfg = config();
        let mut state = DiscordWatchState::default();
        for i in 0..99 {
            assert!(
                process_message(
                    &cfg,
                    &mut state,
                    &msg(&format!("m{i}"), "fixture-general", "user", i),
                    i
                )
                .is_none()
            );
        }
        let first = process_message(
            &cfg,
            &mut state,
            &msg("m100", "fixture-general", "user", 100),
            100,
        )
        .expect("100th");
        assert_eq!(first.reasons, vec![REASON_T3_CHANNEL_BACKLOG]);
        for i in 101..151 {
            assert!(
                process_message(
                    &cfg,
                    &mut state,
                    &msg(&format!("m{i}"), "fixture-general", "user", i),
                    i
                )
                .is_none()
            );
        }
        for i in 151..250 {
            assert!(
                process_message(
                    &cfg,
                    &mut state,
                    &msg(&format!("m{i}"), "fixture-general", "user", i + 2_000),
                    i + 2_000
                )
                .is_none()
            );
        }
        let second = process_message(
            &cfg,
            &mut state,
            &msg("m250", "fixture-general", "user", 2_250),
            2_250,
        )
        .expect("second after cooldown and +100");
        assert_eq!(second.reasons, vec![REASON_T3_CHANNEL_BACKLOG]);
    }

    #[test]
    fn t3_records_message_ids_seen_during_cooldown_before_replay() {
        let mut cfg = config();
        cfg.global_cooldown_ms = 1_000;
        cfg.channel_cooldown_ms = 1_000;
        let mut state = DiscordWatchState {
            global_last_nudge_at_ms: Some(0),
            ..DiscordWatchState::default()
        };
        state
            .channels
            .entry("fixture-general".into())
            .or_default()
            .last_nudge_at_ms = Some(0);

        for i in 0..100 {
            assert!(
                process_message(
                    &cfg,
                    &mut state,
                    &msg(&format!("cooldown-{i}"), "fixture-general", "user", 100),
                    100,
                )
                .is_none()
            );
        }
        assert_eq!(
            state.channels["fixture-general"].new_messages_since_gaebal, 0,
            "cooldown messages should not advance backlog"
        );

        for i in 0..100 {
            assert!(
                process_message(
                    &cfg,
                    &mut state,
                    &msg(&format!("cooldown-{i}"), "fixture-general", "user", 2_000),
                    2_000,
                )
                .is_none(),
                "replayed cooldown message should remain debounced"
            );
        }
        assert_eq!(
            state.channels["fixture-general"].new_messages_since_gaebal,
            0
        );
    }

    #[test]
    fn t3_debounces_replayed_message_ids() {
        let cfg = config();
        let mut state = DiscordWatchState::default();
        for _ in 0..100 {
            assert!(
                process_message(
                    &cfg,
                    &mut state,
                    &msg("same", "fixture-general", "user", 0),
                    0
                )
                .is_none()
            );
        }
        assert_eq!(
            state.channels["fixture-general"].new_messages_since_gaebal,
            1
        );
    }

    #[test]
    fn gaebal_reply_resets_t1_and_t3() {
        let cfg = config();
        let mut state = DiscordWatchState::default();
        for i in 0..4 {
            let mut m = msg(&format!("m{i}"), "fixture-general", "user", i);
            m.mentions.push(cfg.gaebal_gajae_user_id.clone());
            process_message(&cfg, &mut state, &m, i);
        }
        for i in 4..99 {
            process_message(
                &cfg,
                &mut state,
                &msg(&format!("m{i}"), "fixture-general", "user", i),
                i,
            );
        }
        process_message(
            &cfg,
            &mut state,
            &msg("reply", "fixture-general", &cfg.gaebal_gajae_user_id, 100),
            100,
        );
        assert!(state.pending_mentions.is_empty());
        assert_eq!(
            state.channels["fixture-general"].new_messages_since_gaebal,
            0
        );
        let mut next = msg("next", "fixture-general", "user", 101);
        next.mentions.push(cfg.gaebal_gajae_user_id.clone());
        assert!(process_message(&cfg, &mut state, &next, 101).is_none());
    }

    #[test]
    fn omo_ban_uses_configured_channel_name_when_payload_omits_name() {
        let mut cfg = config();
        cfg.watched_channels.push(DiscordWatchChannel {
            id: "omo-id".into(),
            name: "omo".into(),
        });
        let mut state = DiscordWatchState::default();
        let mut message = msg("omo", "omo-id", "user", 0);
        message.channel_name = None;
        message.mentions.push(cfg.gaebal_gajae_user_id.clone());

        assert!(process_message(&cfg, &mut state, &message, 0).is_none());
        assert!(state.channels.is_empty());
        assert!(state.pending_mentions.is_empty());
    }

    #[test]
    fn omo_banned_channels_never_count_or_trigger() {
        let mut cfg = config();
        cfg.watched_channels.push(DiscordWatchChannel {
            id: "omo-id".into(),
            name: "omo".into(),
        });
        let mut state = DiscordWatchState::default();
        for i in 0..1000 {
            let mut m = msg(&format!("omo{i}"), "omo-id", "user", i);
            m.channel_name = Some("omo".into());
            m.mentions.push(cfg.gaebal_gajae_user_id.clone());
            assert!(process_message(&cfg, &mut state, &m, i).is_none());
        }
        assert!(state.channels.is_empty());
        assert!(state.pending_mentions.is_empty());
    }

    #[test]
    fn simultaneous_triggers_emit_one_intent_with_unioned_reasons() {
        let mut cfg = config();
        cfg.global_cooldown_ms = 0;
        cfg.channel_cooldown_ms = 0;
        let mut state = DiscordWatchState::default();
        for i in 0..4 {
            let mut m = msg(&format!("m{i}"), "fixture-general-ko", "user", i);
            m.channel_name = Some("general-ko".into());
            m.mentions.push(cfg.gaebal_gajae_user_id.clone());
            process_message(&cfg, &mut state, &m, i);
        }
        let mut direct = msg("dm", "dm", "user", 0);
        direct.direct_message = true;
        direct.channel_name = Some("owner-dm".into());
        direct.mentions.push(cfg.gaebal_gajae_user_id.clone());
        process_message(&cfg, &mut state, &direct, 0);
        for i in 0..99 {
            process_message(
                &cfg,
                &mut state,
                &msg(&format!("b{i}"), "fixture-general", "user", i),
                i,
            );
        }
        let mut hit = msg("hit", "fixture-general", "user", 180_000);
        hit.mentions.push(cfg.gaebal_gajae_user_id.clone());
        let intent = process_message(&cfg, &mut state, &hit, 180_000).expect("union intent");
        assert_eq!(intent.reasons.len(), 3);
        assert!(
            intent
                .reasons
                .contains(&REASON_T1_PENDING_MENTIONS.to_string())
        );
        assert!(
            intent
                .reasons
                .contains(&REASON_T2_UNANSWERED_DIRECT_MENTION.to_string())
        );
        assert!(
            intent
                .reasons
                .contains(&REASON_T3_CHANNEL_BACKLOG.to_string())
        );
    }

    #[test]
    fn fixed_doctrine_formatting_and_metadata_are_local_only() {
        let cfg = config();
        let content = render_nudge_content(&cfg, "fixture-general", "general");
        assert_eq!(
            content,
            "UltraWorkers: <#fixture-general> / general 스윕하라. 기존 크론 독트린 기준으로 최근 메시지를 읽고 필요한 답변/액션만 수행하라."
        );
        let mut state = DiscordWatchState::default();
        let mut intent = None;
        for i in 0..100 {
            intent = process_message(
                &cfg,
                &mut state,
                &msg(&format!("m{i}"), "fixture-general", "user", i),
                i,
            );
        }
        let intent = intent.expect("intent");
        assert!(intent.local_only);
        assert_eq!(
            intent.nudge_target_channel_id.as_deref(),
            Some("fixture-nudge-target")
        );
        let local_event = intent_to_local_event(&intent);
        assert_eq!(local_event.canonical_kind(), "discord-watch.nudge-intent");
        assert!(
            local_event.channel.is_none(),
            "metadata target must not become a Discord route channel"
        );
    }

    #[test]
    fn channel_state_deserializes_without_recent_message_ids_for_compatibility() {
        let state: DiscordWatchState = serde_json::from_str(
            r#"{
  "channels": {
    "fixture-general": {
      "channel_name": "general",
      "new_messages_since_gaebal": 3
    }
  },
  "pending_mentions": {}
}"#,
        )
        .expect("legacy state");

        assert!(
            state.channels["fixture-general"]
                .recent_message_ids
                .is_empty()
        );
    }

    #[test]
    fn local_persistence_writes_state_and_jsonl_without_dispatch() {
        let dir = tempdir().expect("tempdir");
        let cron = dir.path().join("cron-state.json");
        let cfg = config();
        let event = IncomingEvent {
            kind: "discord.message-create".into(),
            channel: Some("should-not-route".into()),
            mention: None,
            format: None,
            template: None,
            payload: json!(msg("m", "fixture-general", "user", 0)),
        };
        let mut state = DiscordWatchState::default();
        for i in 0..99 {
            let _ = process_message(
                &cfg,
                &mut state,
                &msg(&format!("pre{i}"), "fixture-general", "user", i),
                i,
            );
        }
        save_state(&default_state_path(&cron), &state).expect("state seed");
        let intent = handle_local_intent_event(&cfg, &cron, &event, 100)
            .expect("handle")
            .expect("intent");
        assert!(intent.local_only);
        assert!(default_state_path(&cron).exists());
        let jsonl = fs::read_to_string(default_intents_path(&cron)).expect("intent jsonl");
        assert_eq!(jsonl.lines().count(), 1);
        assert!(jsonl.contains("local_only"));
    }

    #[test]
    fn atomic_state_save_preserves_existing_state_when_temp_path_exists() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("discord-watch-state.json");
        let original = DiscordWatchState {
            global_last_nudge_at_ms: Some(1),
            ..DiscordWatchState::default()
        };
        save_state(&state_path, &original).expect("initial save");
        let temp_path = state_temp_path(&state_path);
        fs::write(&temp_path, "stale temp").expect("stale temp");

        let mut updated = original.clone();
        updated.global_last_nudge_at_ms = Some(2);
        let error = save_state(&state_path, &updated).expect_err("create_new temp collision fails");
        assert!(
            error.to_string().contains("exists") || error.to_string().contains("File exists"),
            "unexpected error: {error}"
        );

        let saved = load_state(&state_path).expect("saved state");
        assert_eq!(saved.global_last_nudge_at_ms, Some(1));
        assert!(
            !temp_path.exists(),
            "failed atomic state writes should clean their temporary sidecar"
        );
    }

    #[test]
    fn deterministic_intent_append_is_idempotent_across_state_save_failure_replay() {
        let dir = tempdir().expect("tempdir");
        let cron = dir.path().join("cron-state.json");
        let state_path = dir.path().join("discord-watch-state.json");
        let intent_path = dir.path().join("discord-watch-intents.jsonl");
        let mut cfg = config();
        cfg.state_file = Some(state_path.clone());
        cfg.intent_file = Some(intent_path.clone());
        let mut state = DiscordWatchState::default();
        for i in 0..99 {
            let _ = process_message(
                &cfg,
                &mut state,
                &msg(&format!("pre{i}"), "fixture-general", "user", i),
                i,
            );
        }
        save_state(&state_path, &state).expect("state seed");
        let temp_path = state_temp_path(&state_path);
        fs::write(&temp_path, "stale temp").expect("force save failure");
        let event = IncomingEvent {
            kind: "discord.message-create".into(),
            channel: Some("should-not-route".into()),
            mention: None,
            format: None,
            template: None,
            payload: json!(msg("trigger", "fixture-general", "user", 100)),
        };

        let first = handle_local_intent_event(&cfg, &cron, &event, 100)
            .expect_err("state save should fail after intent append");
        assert!(
            first.to_string().contains("exists") || first.to_string().contains("File exists"),
            "unexpected error: {first}"
        );
        fs::write(&temp_path, "stale temp").expect("force second save failure");
        let second = handle_local_intent_event(&cfg, &cron, &event, 100)
            .expect_err("state save should still fail after idempotent append");
        assert!(
            second.to_string().contains("exists") || second.to_string().contains("File exists"),
            "unexpected error: {second}"
        );

        let jsonl = fs::read_to_string(intent_path).expect("intent jsonl");
        assert_eq!(
            jsonl.lines().count(),
            1,
            "replaying after state-save failure must not duplicate the persisted intent"
        );
    }

    #[test]
    fn intent_append_failure_does_not_advance_saved_state() {
        let dir = tempdir().expect("tempdir");
        let cron = dir.path().join("cron-state.json");
        let mut cfg = config();
        cfg.intent_file = Some(dir.path().to_path_buf());
        let state_path = default_state_path(&cron);
        let mut state = DiscordWatchState::default();
        for i in 0..99 {
            let _ = process_message(
                &cfg,
                &mut state,
                &msg(&format!("pre{i}"), "fixture-general", "user", i),
                i,
            );
        }
        save_state(&state_path, &state).expect("state seed");
        let event = IncomingEvent {
            kind: "discord.message-create".into(),
            channel: Some("should-not-route".into()),
            mention: None,
            format: None,
            template: None,
            payload: json!(msg("m", "fixture-general", "user", 100)),
        };

        let error = handle_local_intent_event(&cfg, &cron, &event, 100)
            .expect_err("intent append should fail when path is a directory");
        assert!(
            error.to_string().contains("Is a directory") || error.to_string().contains("directory")
        );
        let saved = load_state(&state_path).expect("saved state");
        assert_eq!(
            saved.channels["fixture-general"].new_messages_since_gaebal, 99,
            "failed intent append must not save threshold-advancing state"
        );
    }
}
