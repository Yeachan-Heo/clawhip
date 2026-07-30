use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
#[cfg(unix)]
use std::ffi::CString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::{Date, Duration as TimeDuration, OffsetDateTime, PrimitiveDateTime, format_description};

use crate::Result;
use crate::events::MessageFormat;
use crate::source::workspace::{default_workspace_debounce_ms, default_workspace_watch_dirs};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default, skip_serializing_if = "DiscordConfig::is_empty")]
    pub discord: DiscordConfig,
    #[serde(default, skip_serializing_if = "ProvidersConfig::is_empty")]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub dispatch: DispatchConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    #[serde(default)]
    pub routes: Vec<RouteRule>,
    #[serde(default)]
    pub monitors: MonitorConfig,
    #[serde(default, skip_serializing_if = "CronConfig::is_empty")]
    pub cron: CronConfig,
    #[serde(default, skip_serializing_if = "DiscordWatchConfig::is_empty")]
    pub discord_watch: DiscordWatchConfig,
    #[serde(default, skip_serializing_if = "crate::update::UpdateConfig::is_empty")]
    pub update: crate::update::UpdateConfig,
    #[serde(default, skip_serializing_if = "GajaeConfig::is_empty")]
    pub gajae: GajaeConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subscriptions: Vec<SubscriptionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionConfig {
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    pub kind: String,
    pub endpoint_env: String,
    #[serde(default = "default_subscription_max_frame_bytes")]
    pub max_frame_bytes: usize,
    #[serde(default = "default_subscription_max_json_depth")]
    pub max_json_depth: usize,
    pub filter: SubscriptionFilterConfig,
    pub projection: BTreeMap<String, String>,
    pub adapter: SubscriptionAdapterConfig,
    #[serde(default)]
    pub reconnect: SubscriptionReconnectConfig,
    #[serde(default)]
    pub routing: SubscriptionRoutingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionFilterConfig {
    pub discriminator_pointer: String,
    pub discriminator_equals: String,
    #[serde(default)]
    pub predicates: Vec<SubscriptionPredicateConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionPredicateConfig {
    pub pointer: String,
    pub equals: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionAdapterConfig {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_subscription_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_subscription_stdin_bytes")]
    pub max_stdin_bytes: usize,
    #[serde(default = "default_subscription_stdout_bytes")]
    pub max_stdout_bytes: usize,
    #[serde(default = "default_subscription_stderr_bytes")]
    pub max_stderr_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionReconnectConfig {
    #[serde(default = "default_subscription_initial_delay_ms")]
    pub initial_delay_ms: u64,
    #[serde(default = "default_subscription_max_delay_ms")]
    pub max_delay_ms: u64,
    #[serde(default = "default_subscription_max_attempts")]
    pub max_attempts: u64,
}
impl Default for SubscriptionReconnectConfig {
    fn default() -> Self {
        Self {
            initial_delay_ms: default_subscription_initial_delay_ms(),
            max_delay_ms: default_subscription_max_delay_ms(),
            max_attempts: default_subscription_max_attempts(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionRoutingConfig {
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub repo_name: Option<String>,
    #[serde(default)]
    pub repo_path: Option<String>,
    #[serde(default)]
    pub worktree_path: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
}

fn default_subscription_max_frame_bytes() -> usize {
    65_536
}
fn default_subscription_max_json_depth() -> usize {
    16
}
fn default_subscription_timeout_ms() -> u64 {
    5_000
}
fn default_subscription_stdin_bytes() -> usize {
    16_384
}
fn default_subscription_stdout_bytes() -> usize {
    16_384
}
fn default_subscription_stderr_bytes() -> usize {
    4_096
}
fn default_subscription_initial_delay_ms() -> u64 {
    250
}
fn default_subscription_max_delay_ms() -> u64 {
    5_000
}
fn default_subscription_max_attempts() -> u64 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GajaeConfig {
    #[serde(default)]
    pub handlers_enabled: bool,
    #[serde(default = "default_gajae_handler_timeout_ms")]
    pub handler_timeout_ms: u64,
    #[serde(default = "default_gajae_handler_max_output_bytes")]
    pub handler_max_output_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_target_channel: Option<String>,
}

impl Default for GajaeConfig {
    fn default() -> Self {
        Self {
            handlers_enabled: false,
            handler_timeout_ms: default_gajae_handler_timeout_ms(),
            handler_max_output_bytes: default_gajae_handler_max_output_bytes(),
            hold_target_channel: None,
        }
    }
}

impl GajaeConfig {
    fn is_empty(&self) -> bool {
        !self.handlers_enabled
            && self.handler_timeout_ms == default_gajae_handler_timeout_ms()
            && self.handler_max_output_bytes == default_gajae_handler_max_output_bytes()
            && self.hold_target_channel.is_none()
    }
}

fn default_gajae_handler_timeout_ms() -> u64 {
    5_000
}

fn default_gajae_handler_max_output_bytes() -> usize {
    16 * 1024
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub slack: SlackConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscordConfig {
    #[serde(alias = "token")]
    pub bot_token: Option<String>,
    #[serde(alias = "default_channel")]
    pub legacy_default_channel: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SlackConfig {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_bind_host")]
    pub bind_host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_base_url")]
    pub base_url: String,
}

impl DiscordConfig {
    fn is_empty(&self) -> bool {
        self.bot_token.is_none() && self.legacy_default_channel.is_none()
    }
}

impl ProvidersConfig {
    fn is_empty(&self) -> bool {
        self.discord.is_empty() && self.slack.is_empty()
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            bind_host: default_bind_host(),
            port: default_port(),
            base_url: default_base_url(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchConfig {
    #[serde(default = "default_ci_batch_window_secs")]
    pub ci_batch_window_secs: u64,
    #[serde(default = "default_routine_batch_window_secs")]
    pub routine_batch_window_secs: u64,
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            ci_batch_window_secs: default_ci_batch_window_secs(),
            routine_batch_window_secs: default_routine_batch_window_secs(),
        }
    }
}

impl DispatchConfig {
    pub fn ci_batch_window(&self) -> Duration {
        Duration::from_secs(self.ci_batch_window_secs.max(1))
    }

    pub fn routine_batch_window(&self) -> Option<Duration> {
        (self.routine_batch_window_secs > 0)
            .then(|| Duration::from_secs(self.routine_batch_window_secs))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultsConfig {
    pub channel: Option<String>,
    /// Human-readable channel name hint for the default channel (binding verification).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    #[serde(default)]
    pub format: MessageFormat,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            channel: None,
            channel_name: None,
            format: MessageFormat::Compact,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRule {
    pub event: String,
    #[serde(default)]
    pub filter: BTreeMap<String, String>,
    #[serde(default = "default_sink_name")]
    pub sink: String,
    pub channel: Option<String>,
    /// Explicit Discord thread ID target. Discord threads are channel-like
    /// endpoints, but keeping this separate from `channel` preserves operator
    /// intent and avoids hidden ID heuristics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// Human-readable Discord channel name hint for binding verification.
    /// When set, `clawhip config verify-bindings` compares the live channel
    /// name against this value to detect drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    pub webhook: Option<String>,
    pub slack_webhook: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    pub mention: Option<String>,
    #[serde(default)]
    pub allow_dynamic_tokens: bool,
    pub format: Option<MessageFormat>,
    pub template: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gajae: Option<GajaeRouteAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GajaeRouteAction {
    pub subcommand: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub requires_approval: bool,
}

impl Default for RouteRule {
    fn default() -> Self {
        Self {
            event: String::new(),
            filter: BTreeMap::new(),
            sink: default_sink_name(),
            channel: None,
            thread: None,
            channel_name: None,
            webhook: None,
            slack_webhook: None,
            local_path: None,
            mention: None,
            allow_dynamic_tokens: false,
            format: None,
            template: None,
            gajae: None,
        }
    }
}

impl SlackConfig {
    fn is_empty(&self) -> bool {
        true
    }
}

impl RouteRule {
    pub fn effective_sink(&self) -> &str {
        let sink = self.sink.trim();
        if self.slack_webhook_target().is_some() && (sink.is_empty() || sink == "discord") {
            "slack"
        } else if sink.is_empty() {
            "discord"
        } else {
            sink
        }
    }

    pub fn discord_webhook_target(&self) -> Option<&str> {
        (self.effective_sink() == "discord")
            .then(|| non_empty_trimmed(self.webhook.as_deref()))
            .flatten()
    }

    pub fn discord_thread_target(&self) -> Option<&str> {
        (self.effective_sink() == "discord")
            .then(|| non_empty_trimmed(self.thread.as_deref()))
            .flatten()
    }

    pub fn slack_webhook_target(&self) -> Option<&str> {
        non_empty_trimmed(self.slack_webhook.as_deref()).or_else(|| {
            (self.sink.trim() == "slack").then(|| non_empty_trimmed(self.webhook.as_deref()))?
        })
    }

    pub fn local_file_target(&self) -> Option<&str> {
        (self.effective_sink() == "localfile")
            .then(|| non_empty_trimmed(self.local_path.as_deref()))
            .flatten()
    }

    fn has_any_webhook_target(&self) -> bool {
        self.discord_webhook_target().is_some() || self.slack_webhook_target().is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorConfig {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    pub github_token: Option<String>,
    #[serde(default = "default_github_api_base")]
    pub github_api_base: String,
    #[serde(default)]
    pub git: GitMonitorConfig,
    #[serde(default)]
    pub tmux: TmuxMonitorConfig,
    #[serde(default)]
    pub workspace: Vec<WorkspaceMonitor>,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval(),
            github_token: None,
            github_api_base: default_github_api_base(),
            git: GitMonitorConfig::default(),
            tmux: TmuxMonitorConfig::default(),
            workspace: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitMonitorConfig {
    #[serde(default)]
    pub repos: Vec<GitRepoMonitor>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TmuxMonitorConfig {
    #[serde(default)]
    pub sessions: Vec<TmuxSessionMonitor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitRepoMonitor {
    pub path: String,
    pub name: Option<String>,
    #[serde(default = "default_remote")]
    pub remote: String,
    pub github_repo: Option<String>,
    #[serde(default = "default_true")]
    pub emit_commits: bool,
    #[serde(default = "default_true")]
    pub emit_branch_changes: bool,
    #[serde(default = "default_true")]
    pub emit_issue_opened: bool,
    #[serde(default)]
    pub emit_pr_status: bool,
    pub channel: Option<String>,
    /// Human-readable channel name hint for binding verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    /// Marks git monitors created and owned by `clawhip setup --bind`.
    /// Manual monitors default to false so binding drift audits do not infer
    /// ownership from repo/channel metadata alone.
    #[serde(default, skip_serializing_if = "is_false")]
    pub setup_owned: bool,
    pub mention: Option<String>,
    pub format: Option<MessageFormat>,
}

impl Default for GitRepoMonitor {
    fn default() -> Self {
        Self {
            path: String::new(),
            name: None,
            remote: default_remote(),
            github_repo: None,
            emit_commits: true,
            emit_branch_changes: true,
            emit_issue_opened: true,
            emit_pr_status: false,
            channel: None,
            channel_name: None,
            setup_owned: false,
            mention: None,
            format: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmuxSessionMonitor {
    pub session: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default = "default_keyword_window_secs")]
    pub keyword_window_secs: u64,
    #[serde(default = "default_stale_minutes")]
    pub stale_minutes: u64,
    pub channel: Option<String>,
    /// Human-readable channel name hint for binding verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    pub mention: Option<String>,
    pub format: Option<MessageFormat>,
}

impl Default for TmuxSessionMonitor {
    fn default() -> Self {
        Self {
            session: String::new(),
            keywords: Vec::new(),
            keyword_window_secs: default_keyword_window_secs(),
            stale_minutes: default_stale_minutes(),
            channel: None,
            channel_name: None,
            mention: None,
            format: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMonitor {
    pub path: String,
    #[serde(default = "default_workspace_watch_dirs")]
    pub watch_dirs: Vec<String>,
    #[serde(default)]
    pub discover_worktrees: bool,
    pub channel: Option<String>,
    pub mention: Option<String>,
    pub format: Option<MessageFormat>,
    #[serde(default)]
    pub events: Vec<String>,
    pub poll_interval_secs: Option<u64>,
    #[serde(default = "default_workspace_debounce_ms")]
    pub debounce_ms: u64,
}

impl Default for WorkspaceMonitor {
    fn default() -> Self {
        Self {
            path: String::new(),
            watch_dirs: default_workspace_watch_dirs(),
            discover_worktrees: false,
            channel: None,
            mention: None,
            format: None,
            events: Vec::new(),
            poll_interval_secs: None,
            debounce_ms: default_workspace_debounce_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordWatchConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_discord_watch_channels")]
    pub watched_channels: Vec<DiscordWatchChannel>,
    #[serde(default)]
    pub banned_channel_ids: Vec<String>,
    #[serde(default = "default_discord_watch_banned_channel_names")]
    pub banned_channel_names: Vec<String>,
    #[serde(default = "default_gaebal_gajae_user_id")]
    pub gaebal_gajae_user_id: String,
    #[serde(default)]
    pub owner_user_ids: Vec<String>,
    #[serde(default = "default_nudge_target_channel_id")]
    pub nudge_target_channel_id: Option<String>,
    #[serde(default = "default_pending_mentions_threshold")]
    pub pending_mentions_threshold: u64,
    #[serde(default = "default_direct_mention_persist_ms")]
    pub direct_mention_persist_ms: i64,
    #[serde(default = "default_channel_message_threshold")]
    pub channel_message_threshold: u64,
    #[serde(default = "default_discord_watch_global_cooldown_ms")]
    pub global_cooldown_ms: i64,
    #[serde(default = "default_discord_watch_channel_cooldown_ms")]
    pub channel_cooldown_ms: i64,
    #[serde(default = "default_discord_watch_doctrine_template")]
    pub doctrine_template: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordWatchChannel {
    pub id: String,
    pub name: String,
}

impl Default for DiscordWatchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            watched_channels: default_discord_watch_channels(),
            banned_channel_ids: Vec::new(),
            banned_channel_names: default_discord_watch_banned_channel_names(),
            gaebal_gajae_user_id: default_gaebal_gajae_user_id(),
            owner_user_ids: Vec::new(),
            nudge_target_channel_id: default_nudge_target_channel_id(),
            pending_mentions_threshold: default_pending_mentions_threshold(),
            direct_mention_persist_ms: default_direct_mention_persist_ms(),
            channel_message_threshold: default_channel_message_threshold(),
            global_cooldown_ms: default_discord_watch_global_cooldown_ms(),
            channel_cooldown_ms: default_discord_watch_channel_cooldown_ms(),
            doctrine_template: default_discord_watch_doctrine_template(),
            state_file: None,
            intent_file: None,
        }
    }
}

impl DiscordWatchConfig {
    fn is_empty(&self) -> bool {
        !self.enabled
            && self.watched_channels == default_discord_watch_channels()
            && self.banned_channel_ids.is_empty()
            && self.banned_channel_names == default_discord_watch_banned_channel_names()
            && self.gaebal_gajae_user_id == default_gaebal_gajae_user_id()
            && self.owner_user_ids.is_empty()
            && self.nudge_target_channel_id == default_nudge_target_channel_id()
            && self.pending_mentions_threshold == default_pending_mentions_threshold()
            && self.direct_mention_persist_ms == default_direct_mention_persist_ms()
            && self.channel_message_threshold == default_channel_message_threshold()
            && self.global_cooldown_ms == default_discord_watch_global_cooldown_ms()
            && self.channel_cooldown_ms == default_discord_watch_channel_cooldown_ms()
            && self.doctrine_template == default_discord_watch_doctrine_template()
            && self.state_file.is_none()
            && self.intent_file.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronConfig {
    #[serde(default = "default_cron_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default)]
    pub jobs: Vec<CronJob>,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_cron_poll_interval_secs(),
            jobs: Vec::new(),
        }
    }
}

impl CronConfig {
    fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub schedule: String,
    #[serde(default = "default_cron_timezone")]
    pub timezone: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub channel: Option<String>,
    pub mention: Option<String>,
    pub format: Option<MessageFormat>,
    /// Optional path to a JSON GAJAE receipt/state file that gates this job's emissions.
    ///
    /// When set, the cron scheduler reads the file before emitting. Validated
    /// GAJAE zero-backlog/follow-up receipts with zero open issues, zero open
    /// PRs, green dev CI, no action-needed sessions, and no holds suppress
    /// repeated notifications only while the same public-safe key remains within
    /// `zero_backlog_suppression_ttl_secs`. New public events, non-zero backlog,
    /// CI failures, branch head or check-summary changes, stale sessions, holds,
    /// missing files, or malformed JSON fail
    /// open and emit normally.
    ///
    /// GitHub API/rate-limit failures are detected separately from an empty
    /// backlog via `github_api_status` (e.g. `rate_limited`, `error`,
    /// `unavailable`). Such a degraded fallback is marked with an
    /// `observation_source`/`observation_confidence` pair on the emitted event
    /// and never counts as zero-backlog merge/close authority unless the receipt
    /// also sets `fallback_evidence: true` to assert the required corroborating
    /// evidence was gathered from a safe source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_file: Option<PathBuf>,
    /// Maximum seconds that a validated zero-backlog GAJAE receipt may suppress
    /// repeated follow-up notifications with the same public-safe suppression key.
    /// Set to `0` to disable suppression.
    #[serde(default = "default_zero_backlog_suppression_ttl_secs")]
    pub zero_backlog_suppression_ttl_secs: u64,
    #[serde(flatten)]
    pub kind: CronJobKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum CronJobKind {
    CustomMessage { message: String },
}

pub fn default_config_path() -> PathBuf {
    if let Ok(override_path) = env::var("CLAWHIP_CONFIG") {
        return PathBuf::from(override_path);
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".clawhip").join("config.toml")
}

fn default_bind_host() -> String {
    "0.0.0.0".to_string()
}
fn default_port() -> u16 {
    25294
}
fn default_base_url() -> String {
    format!("http://127.0.0.1:{}", default_port())
}
fn default_poll_interval() -> u64 {
    5
}
fn default_github_api_base() -> String {
    "https://api.github.com".to_string()
}
fn default_remote() -> String {
    "origin".to_string()
}
fn default_stale_minutes() -> u64 {
    10
}
fn default_zero_backlog_suppression_ttl_secs() -> u64 {
    60 * 60
}
fn default_ci_batch_window_secs() -> u64 {
    30
}
fn default_routine_batch_window_secs() -> u64 {
    5
}
fn default_keyword_window_secs() -> u64 {
    30
}
fn default_cron_poll_interval_secs() -> u64 {
    30
}
fn default_cron_timezone() -> String {
    "UTC".to_string()
}

fn default_discord_watch_channels() -> Vec<DiscordWatchChannel> {
    Vec::new()
}
fn default_discord_watch_banned_channel_names() -> Vec<String> {
    vec!["omo".into(), "omo-help".into()]
}
fn default_gaebal_gajae_user_id() -> String {
    String::new()
}
fn default_nudge_target_channel_id() -> Option<String> {
    None
}
fn default_pending_mentions_threshold() -> u64 {
    5
}
fn default_direct_mention_persist_ms() -> i64 {
    180_000
}
fn default_channel_message_threshold() -> u64 {
    100
}
fn default_discord_watch_global_cooldown_ms() -> i64 {
    300_000
}
fn default_discord_watch_channel_cooldown_ms() -> i64 {
    300_000
}
fn default_discord_watch_doctrine_template() -> String {
    "UltraWorkers: <#{channel_id}> / {channel_name} 스윕하라. 기존 크론 독트린 기준으로 최근 메시지를 읽고 필요한 답변/액션만 수행하라.".into()
}
fn default_true() -> bool {
    true
}

pub fn default_sink_name() -> String {
    "discord".to_string()
}

const DISCORD_TOKEN_ENV_VARS: [&str; 2] = ["DISCORD_TOKEN", "CLAWHIP_DISCORD_BOT_TOKEN"];
pub const CONFIG_EDITOR_MENU_ITEMS: [&str; 8] = [
    "Set Discord bot token",
    "Set daemon base URL",
    "Set default channel",
    "Set default format",
    "Set Discord webhook quickstart route",
    "Save and exit",
    "Exit without saving",
    "Print manual config template hint",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupEdits {
    pub webhook: Option<String>,
    pub bot_token: Option<String>,
    pub default_channel: Option<String>,
    pub default_format: Option<MessageFormat>,
    pub daemon_base_url: Option<String>,
}

impl SetupEdits {
    pub fn is_empty(&self) -> bool {
        self.webhook.is_none()
            && self.bot_token.is_none()
            && self.default_channel.is_none()
            && self.default_format.is_none()
            && self.daemon_base_url.is_none()
    }
}

fn merge_legacy_discord_field(
    field: &str,
    legacy: Option<String>,
    provider: &mut Option<String>,
) -> Result<()> {
    let legacy = normalize_text(legacy);
    let provider_value = normalize_text(provider.clone());

    match (legacy, provider_value) {
        (Some(legacy), Some(provider_value)) if legacy != provider_value => Err(format!(
            "conflicting legacy [discord].{field} and [providers.discord].{field} values"
        )
        .into()),
        (Some(legacy), None) => {
            *provider = Some(legacy);
            Ok(())
        }
        (_, Some(provider_value)) => {
            *provider = Some(provider_value);
            Ok(())
        }
        (None, None) => {
            *provider = None;
            Ok(())
        }
    }
}

fn normalize_secret(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn non_empty_trimmed(value: Option<&str>) -> Option<&str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    })
}

fn discord_token_from_env_with<F>(mut get_env: F) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    DISCORD_TOKEN_ENV_VARS
        .iter()
        .find_map(|name| normalize_secret(get_env(name)))
}

impl AppConfig {
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)?;
        let raw_toml: toml::Value = toml::from_str(&raw)?;
        let mut config: Self = toml::from_str(&raw)?;
        config.merge_legacy_discord(&raw_toml)?;
        config.normalize();
        if config.defaults.channel.is_none() {
            config.defaults.channel = config.discord_default_channel();
        }
        Ok(config)
    }

    fn merge_legacy_discord(&mut self, raw_toml: &toml::Value) -> Result<()> {
        if raw_toml.get("discord").is_some() {
            merge_legacy_discord_field(
                "token",
                self.discord.bot_token.clone(),
                &mut self.providers.discord.bot_token,
            )?;
            merge_legacy_discord_field(
                "default_channel",
                self.discord.legacy_default_channel.clone(),
                &mut self.providers.discord.legacy_default_channel,
            )?;
        }

        self.discord = DiscordConfig::default();
        Ok(())
    }

    fn discord_default_channel(&self) -> Option<String> {
        normalize_text(self.providers.discord.legacy_default_channel.clone())
            .or_else(|| normalize_text(self.discord.legacy_default_channel.clone()))
    }

    pub fn to_pretty_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn save_with_backup(&self, path: &Path) -> Result<()> {
        let mut before_delete = |_: &BackupCandidate| Ok(());
        self.save_with_backup_at(path, OffsetDateTime::now_utc(), &mut before_delete)
    }

    fn save_with_backup_at<F>(
        &self,
        path: &Path,
        now: OffsetDateTime,
        before_delete: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&BackupCandidate) -> Result<()>,
    {
        let mut before_rename = || Ok(());
        let mut before_snapshot = || Ok(());
        self.save_with_backup_at_hooks(
            path,
            now,
            &mut before_rename,
            &mut before_snapshot,
            before_delete,
        )
    }

    fn save_with_backup_at_hooks<F, G, H>(
        &self,
        path: &Path,
        now: OffsetDateTime,
        before_rename: &mut G,
        before_snapshot: &mut H,
        before_delete: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&BackupCandidate) -> Result<()>,
        G: FnMut() -> Result<()>,
        H: FnMut() -> Result<()>,
    {
        let mut after_preflight = |_: &BackupCandidate| Ok(());
        self.save_with_backup_at_candidate_hooks(
            path,
            now,
            before_rename,
            before_snapshot,
            before_delete,
            &mut after_preflight,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn save_with_backup_at_candidate_hooks<F, G, H, P>(
        &self,
        path: &Path,
        now: OffsetDateTime,
        before_rename: &mut G,
        before_snapshot: &mut H,
        before_delete: &mut F,
        after_preflight: &mut P,
    ) -> Result<()>
    where
        F: FnMut(&BackupCandidate) -> Result<()>,
        G: FnMut() -> Result<()>,
        H: FnMut() -> Result<()>,
        P: FnMut(&BackupCandidate) -> Result<()>,
    {
        let mut after_check = |_: &BackupCandidate| Ok(());
        self.save_with_backup_at_all_candidate_hooks(
            path,
            now,
            before_rename,
            before_snapshot,
            before_delete,
            after_preflight,
            &mut after_check,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn save_with_backup_at_all_candidate_hooks<F, G, H, P, A>(
        &self,
        path: &Path,
        now: OffsetDateTime,
        before_rename: &mut G,
        before_snapshot: &mut H,
        before_delete: &mut F,
        after_preflight: &mut P,
        after_check: &mut A,
    ) -> Result<()>
    where
        F: FnMut(&BackupCandidate) -> Result<()>,
        G: FnMut() -> Result<()>,
        H: FnMut() -> Result<()>,
        P: FnMut(&BackupCandidate) -> Result<()>,
        A: FnMut(&BackupCandidate) -> Result<()>,
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "config path must have a UTF-8 filename".to_string())?;
        let parent_dir = validate_config_parent_dir(parent, true)?;
        revalidate_config_parent_dir(&parent_dir)?;
        let serialized = self.to_pretty_toml()?;
        let new_bytes = serialized.as_bytes();
        let active = read_active_config_no_follow(path)?;

        if active.bytes() == Some(new_bytes) {
            revalidate_config_parent_dir(&parent_dir).map_err(|error| {
                io::Error::other(format!(
                    "config was already current; backup retention cleanup remains incomplete: {error}"
                ))
            })?;
            let managed_dir =
                validate_managed_backup_dir(&parent_dir.path, false).map_err(|error| {
                    io::Error::other(format!(
                        "config was already current; backup retention cleanup remains incomplete: {error}"
                    ))
                })?;
            let protected_identities = active.identity().into_iter().collect::<Vec<_>>();
            cleanup_config_backups_with_hooks(
                &parent_dir.path,
                parent_dir.identity,
                filename,
                managed_dir.as_ref(),
                &protected_identities,
                now,
                before_delete,
                after_preflight,
                after_check,
            )
            .map_err(|error| {
                io::Error::other(format!(
                    "config was already current; backup retention cleanup remains incomplete: {error}"
                ))
            })?;
            return Ok(());
        }

        revalidate_config_parent_dir(&parent_dir)?;
        let managed_dir = validate_managed_backup_dir(&parent_dir.path, active.bytes().is_some())?;
        if let Some(old_bytes) = active.bytes() {
            let managed_dir = managed_dir
                .as_ref()
                .ok_or_else(|| "managed config backup directory was not created".to_string())?;
            create_managed_snapshot_create_new(
                managed_dir,
                filename,
                old_bytes,
                now,
                before_snapshot,
            )?;
        }

        revalidate_config_parent_dir(&parent_dir)?;
        commit_config_bytes_via_temp(
            &parent_dir,
            path,
            filename,
            new_bytes,
            &active,
            before_rename,
        )?;

        revalidate_config_parent_dir(&parent_dir).map_err(|error| {
            io::Error::other(format!(
                "config was saved; backup retention cleanup remains incomplete: {error}"
            ))
        })?;
        let current_identity = current_active_config_identity(path).map_err(|error| {
            io::Error::other(format!(
                "config was saved; backup retention cleanup remains incomplete: {error}"
            ))
        })?;
        let mut protected_identities = active.identity().into_iter().collect::<Vec<_>>();
        if let Some(current_identity) = current_identity
            && !protected_identities.contains(&current_identity)
        {
            protected_identities.push(current_identity);
        }
        cleanup_config_backups_with_hooks(
            &parent_dir.path,
            parent_dir.identity,
            filename,
            managed_dir.as_ref(),
            &protected_identities,
            now,
            before_delete,
            after_preflight,
            after_check,
        )
        .map_err(|error| {
            io::Error::other(format!(
                "config was saved; backup retention cleanup remains incomplete: {error}"
            ))
        })?;
        Ok(())
    }

    pub fn effective_token(&self) -> Option<String> {
        self.effective_token_with(|name| env::var(name).ok())
    }

    fn effective_token_with<F>(&self, get_env: F) -> Option<String>
    where
        F: FnMut(&str) -> Option<String>,
    {
        discord_token_from_env_with(get_env)
            .or_else(|| normalize_secret(self.providers.discord.bot_token.clone()))
            .or_else(|| normalize_secret(self.discord.bot_token.clone()))
    }

    pub fn discord_token_source(&self) -> &'static str {
        self.discord_token_source_with(|name| env::var(name).ok())
    }

    fn discord_token_source_with<F>(&self, get_env: F) -> &'static str
    where
        F: FnMut(&str) -> Option<String>,
    {
        if discord_token_from_env_with(get_env).is_some() {
            "env"
        } else if normalize_secret(self.providers.discord.bot_token.clone()).is_some()
            || normalize_secret(self.discord.bot_token.clone()).is_some()
        {
            "config"
        } else {
            "missing"
        }
    }

    /// Returns the name of the environment variable whose Discord token shadows
    /// a token that is also present in the config file. Returns `None` when env
    /// does not win or when no config token is set, so a `Some(_)` value always
    /// means env precedence is silently overriding a configured token.
    ///
    /// Only the precedence source is exposed — never the token value itself.
    pub fn discord_token_env_shadow(&self) -> Option<&'static str> {
        self.discord_token_env_shadow_with(|name| env::var(name).ok())
    }

    fn discord_token_env_shadow_with<F>(&self, mut get_env: F) -> Option<&'static str>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let env_var = DISCORD_TOKEN_ENV_VARS
            .iter()
            .copied()
            .find(|name| normalize_secret(get_env(name)).is_some())?;
        let config_token_present = normalize_secret(self.providers.discord.bot_token.clone())
            .is_some()
            || normalize_secret(self.discord.bot_token.clone()).is_some();
        config_token_present.then_some(env_var)
    }

    pub fn webhook_route_count(&self) -> usize {
        self.routes
            .iter()
            .filter(|route| route.has_any_webhook_target())
            .count()
    }

    pub fn has_webhook_routes(&self) -> bool {
        self.webhook_route_count() > 0
    }

    fn has_localfile_routes(&self) -> bool {
        self.routes.iter().any(|route| {
            route.effective_sink() == "localfile" && route.local_file_target().is_some()
        })
    }

    fn has_discord_delivery_requiring_bot_token(&self) -> bool {
        self.default_channel_can_fallback_to_discord()
            || self.routes.iter().any(|route| {
                route.effective_sink() == "discord" && route.discord_webhook_target().is_none()
            })
            || self.monitors.git.repos.iter().any(|repo| {
                repo.channel
                    .as_ref()
                    .is_some_and(|channel| !channel.trim().is_empty())
            })
            || self.monitors.tmux.sessions.iter().any(|session| {
                session
                    .channel
                    .as_ref()
                    .is_some_and(|channel| !channel.trim().is_empty())
            })
            || self.monitors.workspace.iter().any(|workspace| {
                workspace
                    .channel
                    .as_ref()
                    .is_some_and(|channel| !channel.trim().is_empty())
            })
            || self.cron.jobs.iter().any(|job| {
                job.channel
                    .as_ref()
                    .is_some_and(|channel| !channel.trim().is_empty())
            })
    }

    fn default_channel_can_fallback_to_discord(&self) -> bool {
        self.defaults
            .channel
            .as_ref()
            .is_some_and(|channel| !channel.trim().is_empty())
            && !self
                .routes
                .iter()
                .any(|route| route.event.trim() == "*" && route.filter.is_empty())
    }

    fn validate_subscription_config(subscription: &SubscriptionConfig) -> Result<()> {
        let name = &subscription.name;
        let valid_name = !name.is_empty()
            && name == name.trim()
            && name.len() <= 63
            && name.as_bytes()[0].is_ascii_lowercase()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if !valid_name || subscription.kind != "websocket" {
            return Err("invalid_subscription_config".into());
        }
        let endpoint = &subscription.endpoint_env;
        if endpoint.is_empty()
            || endpoint.len() > 128
            || (!endpoint.as_bytes()[0].is_ascii_uppercase() && endpoint.as_bytes()[0] != b'_')
            || !endpoint
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err("invalid_subscription_config".into());
        }
        if !(1024..=1_048_576).contains(&subscription.max_frame_bytes)
            || !(1..=16).contains(&subscription.max_json_depth)
            || subscription.filter.discriminator_equals.is_empty()
            || subscription.filter.discriminator_equals.len() > 128
            || subscription.filter.predicates.len() > 8
            || !(1..=16).contains(&subscription.projection.len())
        {
            return Err("invalid_subscription_config".into());
        }
        if !subscription.adapter.program.starts_with('/')
            || subscription.adapter.program.len() > 4096
            || subscription.adapter.args.len() > 16
            || subscription.adapter.args.iter().any(|arg| arg.len() > 256)
            || !(100..=30_000).contains(&subscription.adapter.timeout_ms)
            || !(1..=65_536).contains(&subscription.adapter.max_stdin_bytes)
            || !(1..=65_536).contains(&subscription.adapter.max_stdout_bytes)
            || !(1..=16_384).contains(&subscription.adapter.max_stderr_bytes)
            || !(1..=10).contains(&subscription.reconnect.max_attempts)
            || !(10..=5_000).contains(&subscription.reconnect.initial_delay_ms)
            || subscription.reconnect.max_delay_ms < subscription.reconnect.initial_delay_ms
            || subscription.reconnect.max_delay_ms > 30_000
        {
            return Err("invalid_subscription_config".into());
        }
        if subscription.enabled
            && !crate::source::subscription::is_regular_executable(&subscription.adapter.program)
        {
            return Err("invalid_subscription_config".into());
        }
        crate::source::subscription::validate_projection_policy(subscription)?;
        crate::source::subscription::validate_filter_policy(subscription)?;

        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.dispatch.ci_batch_window_secs == 0 {
            return Err("dispatch.ci_batch_window_secs must be at least 1".into());
        }
        if self.subscriptions.len() > 32 {
            return Err("invalid_subscription_config".into());
        }
        let mut subscription_names = std::collections::BTreeSet::new();
        for subscription in &self.subscriptions {
            Self::validate_subscription_config(subscription)?;
            if !subscription_names.insert(subscription.name.trim().to_ascii_lowercase()) {
                return Err("invalid_subscription_config".into());
            }
        }
        if self.cron.poll_interval_secs == 0 {
            return Err("cron.poll_interval_secs must be at least 1".into());
        }
        if self.discord_watch.enabled {
            if self.discord_watch.gaebal_gajae_user_id.trim().is_empty() {
                return Err(
                    "discord_watch.gaebal_gajae_user_id is required when discord_watch is enabled"
                        .into(),
                );
            }
            for (index, channel) in self.discord_watch.watched_channels.iter().enumerate() {
                if channel.id.trim().is_empty() || channel.name.trim().is_empty() {
                    return Err(format!(
                        "discord_watch.watched_channels[{index}] requires non-empty id and name"
                    )
                    .into());
                }
            }
        }
        if self.discord_watch.pending_mentions_threshold == 0 {
            return Err("discord_watch.pending_mentions_threshold must be at least 1".into());
        }
        if self.discord_watch.direct_mention_persist_ms < 0 {
            return Err("discord_watch.direct_mention_persist_ms must be non-negative".into());
        }
        if self.discord_watch.channel_message_threshold == 0 {
            return Err("discord_watch.channel_message_threshold must be at least 1".into());
        }
        if self.discord_watch.global_cooldown_ms < 0 || self.discord_watch.channel_cooldown_ms < 0 {
            return Err("discord_watch cooldowns must be non-negative".into());
        }

        if self.gajae.handlers_enabled {
            if self.gajae.handler_timeout_ms == 0 {
                return Err(
                    "gajae.handler_timeout_ms must be at least 1 when GAJAE handlers are enabled"
                        .into(),
                );
            }
            if self.gajae.handler_max_output_bytes == 0 {
                return Err("gajae.handler_max_output_bytes must be at least 1 when GAJAE handlers are enabled".into());
            }
        }

        for (index, route) in self.routes.iter().enumerate() {
            let sink = route.effective_sink();
            let has_channel = normalize_secret(route.channel.clone()).is_some();
            let has_thread = route.discord_thread_target().is_some();
            let has_discord_webhook = route.discord_webhook_target().is_some();
            let has_slack_webhook = route.slack_webhook_target().is_some();
            if let Some(gajae) = &route.gajae {
                let subcommand = gajae.subcommand.trim();
                if subcommand.is_empty() {
                    return Err(format!(
                        "route #{} ({}) GAJAE handler must set subcommand",
                        index + 1,
                        route.event
                    )
                    .into());
                }
            }
            if route.sink.trim().is_empty() && !has_slack_webhook {
                return Err(
                    format!("route #{} ({}) must set a sink", index + 1, route.event).into(),
                );
            }
            if !matches!(sink, "discord" | "slack" | "localfile") {
                return Err(format!(
                    "route #{} ({}) uses unsupported sink '{}'",
                    index + 1,
                    route.event,
                    sink
                )
                .into());
            }

            match sink {
                "discord" => {
                    let configured_targets = usize::from(has_channel)
                        + usize::from(has_thread)
                        + usize::from(has_discord_webhook);
                    if configured_targets > 1 {
                        return Err(format!(
                            "route #{} ({}) must set only one Discord target: channel, thread, or webhook",
                            index + 1,
                            route.event
                        )
                        .into());
                    }
                }
                "slack" => {
                    if has_channel {
                        return Err(format!(
                            "route #{} ({}) cannot set channel when sink = \"slack\"",
                            index + 1,
                            route.event
                        )
                        .into());
                    }
                    if normalize_secret(route.webhook.clone()).is_some()
                        && normalize_secret(route.slack_webhook.clone()).is_some()
                    {
                        return Err(format!(
                            "route #{} ({}) cannot set both webhook and slack_webhook for Slack delivery",
                            index + 1,
                            route.event
                        )
                        .into());
                    }
                    if !has_slack_webhook {
                        return Err(format!(
                            "route #{} ({}) must set webhook or slack_webhook when sink = \"slack\"",
                            index + 1,
                            route.event
                        )
                        .into());
                    }
                }
                "localfile" => {
                    if has_channel || has_discord_webhook || has_slack_webhook {
                        return Err(format!(
                            "route #{} ({}) cannot set channel/webhook fields when sink = \"localfile\"",
                            index + 1,
                            route.event
                        )
                        .into());
                    }
                    if route.local_file_target().is_none() {
                        return Err(format!(
                            "route #{} ({}) must set local_path when sink = \"localfile\"",
                            index + 1,
                            route.event
                        )
                        .into());
                    }
                }
                _ => unreachable!(),
            }
        }

        for (index, workspace) in self.monitors.workspace.iter().enumerate() {
            if workspace.path.trim().is_empty() {
                return Err(format!("workspace monitor #{} must set path", index + 1).into());
            }
            if workspace.watch_dirs.is_empty() {
                return Err(format!(
                    "workspace monitor #{} must set at least one watch_dirs entry",
                    index + 1
                )
                .into());
            }
            if workspace.channel.is_none()
                && self.defaults.channel.is_none()
                && !self.has_webhook_routes()
            {
                return Err(format!(
                    "workspace monitor #{} has no channel and no default Discord destination",
                    index + 1
                )
                .into());
            }
        }

        let mut cron_ids = std::collections::BTreeSet::new();
        for (index, job) in self.cron.jobs.iter().enumerate() {
            crate::cron::validate_job(job)
                .map_err(|error| format!("cron job #{}: {error}", index + 1))?;
            if !cron_ids.insert(job.id.as_str()) {
                return Err(format!("duplicate cron job id '{}'", job.id).into());
            }
        }

        if self.effective_token().is_none() {
            if self.has_discord_delivery_requiring_bot_token() {
                return Err(
                    "missing Discord bot token for configured Discord channel delivery; configure [providers.discord].token (or legacy [discord].token), use route webhooks, or remove Discord channel routes"
                        .into(),
                );
            }

            if !self.has_webhook_routes()
                && !self.has_localfile_routes()
                && !self.discord_watch.enabled
            {
                return Err(
                    "missing Discord delivery config: configure [providers.discord].token (or legacy [discord].token), at least one route webhook, or a localfile route"
                        .into(),
                );
            }
        }

        Ok(())
    }

    pub fn apply_setup_edits(&mut self, edits: SetupEdits) -> Result<()> {
        let normalized = SetupEdits {
            webhook: normalize_text(edits.webhook),
            bot_token: normalize_secret(edits.bot_token),
            default_channel: normalize_text(edits.default_channel),
            default_format: edits.default_format,
            daemon_base_url: normalize_text(edits.daemon_base_url),
        };

        if normalized.is_empty() {
            return Err("setup requires at least one non-empty setup flag".into());
        }

        let SetupEdits {
            webhook,
            bot_token,
            default_channel,
            default_format,
            daemon_base_url,
        } = normalized;

        if let Some(webhook) = webhook {
            self.scaffold_webhook_quickstart(webhook)?;
        }
        if let Some(bot_token) = bot_token {
            self.providers.discord.bot_token = Some(bot_token);
        }
        if let Some(default_channel) = default_channel {
            self.defaults.channel = Some(default_channel);
        }
        if let Some(default_format) = default_format {
            self.defaults.format = default_format;
        }
        if let Some(daemon_base_url) = daemon_base_url {
            self.daemon.base_url = daemon_base_url;
        }

        Ok(())
    }

    pub fn scaffold_webhook_quickstart(&mut self, webhook: String) -> Result<()> {
        let webhook = normalize_text(Some(webhook)).ok_or_else(|| {
            "setup requires a non-empty webhook URL when --webhook is supplied".to_string()
        })?;

        let matches = self
            .routes
            .iter()
            .enumerate()
            .filter(|(_, route)| is_canonical_quickstart_route(route))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();

        match matches.as_slice() {
            [] => {
                self.routes.push(RouteRule {
                    event: "*".to_string(),
                    filter: BTreeMap::new(),
                    sink: default_sink_name(),
                    channel: None,
                    thread: None,
                    channel_name: None,
                    webhook: Some(webhook),
                    slack_webhook: None,
                    local_path: None,
                    mention: None,
                    allow_dynamic_tokens: false,
                    format: None,
                    template: None,
                    gajae: None,
                });
                Ok(())
            }
            [index] => {
                self.routes[*index].webhook = Some(webhook);
                Ok(())
            }
            _ => Err(
                "multiple canonical quickstart routes found; clean up manual config before updating the webhook quickstart route"
                    .into(),
            ),
        }
    }

    pub fn apply_repo_channel_route_binding(
        &mut self,
        repo: &str,
        channel_id: &str,
        channel_name: Option<&str>,
    ) -> Result<()> {
        let repo = normalize_text(Some(repo.to_string()))
            .ok_or_else(|| "repo binding requires a non-empty repo name".to_string())?;
        let channel_id = normalize_text(Some(channel_id.to_string()))
            .ok_or_else(|| "repo binding requires a non-empty channel id".to_string())?;
        let channel_name = channel_name.and_then(|value| normalize_text(Some(value.to_string())));

        let route_matches = self
            .routes
            .iter()
            .enumerate()
            .filter(|(_, route)| is_repo_binding_route(route, &repo))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if route_matches.len() > 1 {
            return Err(format!(
                "multiple setup-owned routes found for repo '{repo}'; clean up duplicates before updating binding"
            )
            .into());
        }
        if self.routes.iter().any(|route| {
            !is_repo_binding_route(route, &repo)
                && route.event == "*"
                && route.effective_sink() == "discord"
                && route.filter.len() == 1
                && route.filter.get("repo").is_some_and(|value| value == &repo)
        }) {
            return Err(format!("manual_route_conflict for repo '{repo}'").into());
        }

        match route_matches.as_slice() {
            [index] => {
                let route = &mut self.routes[*index];
                route.channel = Some(channel_id);
                route.thread = None;
                route.channel_name = channel_name;
                route.webhook = None;
            }
            [] => {
                let mut filter = BTreeMap::new();
                filter.insert("repo".to_string(), repo);
                self.routes.push(RouteRule {
                    event: "*".to_string(),
                    filter,
                    sink: default_sink_name(),
                    channel: Some(channel_id),
                    thread: None,
                    channel_name,
                    webhook: None,
                    slack_webhook: None,
                    local_path: None,
                    mention: None,
                    allow_dynamic_tokens: false,
                    format: None,
                    template: None,
                    gajae: None,
                });
            }
            _ => unreachable!(),
        }

        Ok(())
    }

    pub fn apply_repo_channel_binding(
        &mut self,
        repo: &str,
        channel_id: &str,
        channel_name: Option<&str>,
        checkout_path: &str,
    ) -> Result<()> {
        let repo = normalize_text(Some(repo.to_string()))
            .ok_or_else(|| "repo binding requires a non-empty repo name".to_string())?;
        let channel_id = normalize_text(Some(channel_id.to_string()))
            .ok_or_else(|| "repo binding requires a non-empty channel id".to_string())?;
        let channel_name = channel_name.and_then(|value| normalize_text(Some(value.to_string())));
        let checkout_path = normalize_text(Some(checkout_path.to_string()))
            .ok_or_else(|| "repo binding requires a non-empty checkout path".to_string())?;
        let monitor_name = repo.rsplit('/').next().unwrap_or(&repo).to_string();
        let github_repo = is_owner_repo(&repo).then_some(repo.clone());

        let route_matches = self
            .routes
            .iter()
            .enumerate()
            .filter(|(_, route)| is_repo_binding_route(route, &repo))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if route_matches.len() > 1 {
            return Err(format!(
                "multiple setup-owned routes found for repo '{repo}'; clean up duplicates before updating binding"
            )
            .into());
        }
        if self.routes.iter().any(|route| {
            !is_repo_binding_route(route, &repo)
                && route.event == "*"
                && route.effective_sink() == "discord"
                && route.filter.len() == 1
                && route.filter.get("repo").is_some_and(|value| value == &repo)
        }) {
            return Err(format!("manual_route_conflict for repo '{repo}'").into());
        }

        let requested_owner_repo = github_repo.as_deref();
        if self.monitors.git.repos.iter().any(|monitor| {
            monitor.channel.as_deref() == Some(channel_id.as_str())
                && monitor.github_repo.is_none()
                && monitor.name.as_deref() != Some(monitor_name.as_str())
        }) {
            return Err("manual_monitor_conflict".into());
        }

        let monitor_matches = self
            .monitors
            .git
            .repos
            .iter()
            .enumerate()
            .filter(|(_, monitor)| {
                let path_matches = monitor.path.trim() == checkout_path;
                let github_repo_matches = requested_owner_repo
                    .is_some_and(|repo| monitor.github_repo.as_deref() == Some(repo));
                let name_matches = monitor.name.as_deref() == Some(monitor_name.as_str())
                    && requested_owner_repo.is_none_or(|repo| {
                        monitor
                            .github_repo
                            .as_deref()
                            .is_none_or(|existing| existing.trim().is_empty() || existing == repo)
                    });

                path_matches || github_repo_matches || name_matches
            })
            .map(|(index, monitor)| {
                if is_setup_owned_git_monitor(monitor) {
                    Ok(index)
                } else if monitor.channel.as_deref() == Some(channel_id.as_str())
                    && monitor.github_repo.is_none()
                {
                    Err("manual_monitor_conflict".to_string())
                } else {
                    Err(format!(
                        "git monitor conflict for repo '{repo}'; existing monitor is not setup-owned"
                    ))
                }
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|message| -> Box<dyn std::error::Error + Send + Sync> { message.into() })?;
        if monitor_matches.len() > 1 {
            return Err(format!(
                "multiple setup-owned git monitors found for repo '{repo}'; clean up duplicates before updating binding"
            )
            .into());
        }

        match route_matches.as_slice() {
            [index] => {
                let route = &mut self.routes[*index];
                route.channel = Some(channel_id.clone());
                route.thread = None;
                route.channel_name = channel_name.clone();
                route.webhook = None;
            }
            [] => {
                let mut filter = BTreeMap::new();
                filter.insert("repo".to_string(), repo.clone());
                self.routes.push(RouteRule {
                    event: "*".to_string(),
                    filter,
                    sink: default_sink_name(),
                    channel: Some(channel_id.clone()),
                    thread: None,
                    channel_name: channel_name.clone(),
                    webhook: None,
                    slack_webhook: None,
                    local_path: None,
                    mention: None,
                    allow_dynamic_tokens: false,
                    format: None,
                    template: None,
                    gajae: None,
                });
            }
            _ => unreachable!(),
        }

        match monitor_matches.as_slice() {
            [index] => {
                let monitor = &mut self.monitors.git.repos[*index];
                monitor.path = checkout_path;
                monitor.name = Some(monitor_name);
                monitor.github_repo = github_repo;
                monitor.channel = Some(channel_id);
                monitor.channel_name = channel_name;
                monitor.setup_owned = true;
            }
            [] => self.monitors.git.repos.push(GitRepoMonitor {
                path: checkout_path,
                name: Some(monitor_name),
                github_repo,
                channel: Some(channel_id),
                channel_name,
                setup_owned: true,
                ..GitRepoMonitor::default()
            }),
            _ => unreachable!(),
        }

        Ok(())
    }

    pub fn set_discord_bot_token(&mut self, bot_token: String) {
        self.providers.discord.bot_token = normalize_secret(Some(bot_token));
    }

    pub fn set_default_channel(&mut self, channel: String) {
        self.defaults.channel = normalize_text(Some(channel));
    }

    pub fn set_default_format(&mut self, format: MessageFormat) {
        self.defaults.format = format;
    }

    pub fn set_daemon_base_url(&mut self, base_url: String) {
        self.daemon.base_url = normalize_text(Some(base_url)).unwrap_or_else(default_base_url);
    }

    fn canonical_quickstart_webhook(&self) -> Option<&str> {
        self.routes
            .iter()
            .find(|route| is_canonical_quickstart_route(route))
            .and_then(|route| route.webhook.as_deref())
    }

    pub fn daemon_base_url(&self) -> String {
        env::var("CLAWHIP_DAEMON_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| self.daemon.base_url.clone())
    }

    pub fn monitor_github_token(&self) -> Option<String> {
        env::var("CLAWHIP_GITHUB_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| self.monitors.github_token.clone())
    }

    pub fn run_interactive_editor(&mut self, path: &Path) -> Result<()> {
        println!("clawhip config editor");
        println!("Path: {}", path.display());
        println!();
        loop {
            self.print_summary();
            println!("Choose an action:");
            for (index, item) in CONFIG_EDITOR_MENU_ITEMS.iter().enumerate() {
                println!("  {}) {}", index + 1, item);
            }
            match prompt("Selection")?.trim() {
                "1" => self.set_discord_bot_token(prompt("Bot token")?),
                "2" => self.set_daemon_base_url(prompt_with_default(
                    "Daemon base URL",
                    Some(&self.daemon.base_url),
                )?),
                "3" => self.set_default_channel(prompt("Default channel")?),
                "4" => self.set_default_format(prompt_format(Some(self.defaults.format.clone()))?),
                "5" => {
                    let webhook = prompt_with_default(
                        "Discord webhook quickstart route",
                        self.canonical_quickstart_webhook(),
                    )?;
                    self.scaffold_webhook_quickstart(webhook)?;
                }
                "6" => {
                    self.save_with_backup(path)?;
                    println!("Saved {}", path.display());
                    break;
                }
                "7" => {
                    println!("Discarded changes.");
                    break;
                }
                "8" => self.print_template_hint(),
                _ => println!("Unknown selection."),
            }
            println!();
        }
        Ok(())
    }

    fn print_summary(&self) {
        println!("Current config summary:");
        println!("  Discord token source: {}", self.discord_token_source());
        println!("  Daemon base URL: {}", self.daemon.base_url);
        println!(
            "  Bind host/port: {}:{}",
            self.daemon.bind_host, self.daemon.port
        );
        println!("  CI batch window: {}s", self.dispatch.ci_batch_window_secs);
        println!(
            "  Routine batch window: {}",
            self.dispatch
                .routine_batch_window()
                .map(|window| format!("{}s", window.as_secs()))
                .unwrap_or_else(|| "disabled".to_string())
        );
        println!(
            "  Default channel: {}",
            self.defaults.channel.as_deref().unwrap_or("<unset>")
        );
        println!("  Webhook routes: {}", self.routes_with_webhooks());
        println!("  Default format: {}", self.defaults.format.as_str());
        println!("  Routes: {}", self.routes.len());
        println!("  Git monitors: {}", self.monitors.git.repos.len());
        println!("  Tmux monitors: {}", self.monitors.tmux.sessions.len());
        println!("  Workspace monitors: {}", self.monitors.workspace.len());
        println!("  Cron jobs: {}", self.cron.jobs.len());
    }

    fn print_template_hint(&self) {
        println!("Advanced routes and monitors are still edited manually in the config file.");
        println!(
            "Sections: [providers.discord], [dispatch], [daemon], [cron], [[cron.jobs]], [[routes]], [[monitors.git.repos]], [[monitors.tmux.sessions]], [[monitors.workspace]]"
        );
        println!(
            "Routes may set either channel = \"...\" or webhook = \"https://discord.com/api/webhooks/...\"."
        );
        println!(
            r#"Webhook example: [[routes]] event = "tmux.keyword" webhook = "https://discord.com/api/webhooks/...""#
        );
    }

    fn normalize(&mut self) {
        self.discord.bot_token = normalize_secret(self.discord.bot_token.clone());
        self.discord.legacy_default_channel =
            normalize_text(self.discord.legacy_default_channel.clone());
        self.providers.discord.bot_token =
            normalize_secret(self.providers.discord.bot_token.clone());
        self.providers.discord.legacy_default_channel =
            normalize_text(self.providers.discord.legacy_default_channel.clone());
        self.defaults.channel = normalize_text(self.defaults.channel.clone());
        self.monitors.github_token = normalize_secret(self.monitors.github_token.clone());

        for route in &mut self.routes {
            route.sink = normalize_text(Some(route.sink.clone())).unwrap_or_else(default_sink_name);
            route.channel = normalize_text(route.channel.clone());
            route.channel_name = normalize_text(route.channel_name.clone());
            route.webhook = normalize_text(route.webhook.clone());
            route.slack_webhook = normalize_text(route.slack_webhook.clone());
            route.mention = normalize_text(route.mention.clone());
            route.template = normalize_text(route.template.clone());
            if let Some(gajae) = &mut route.gajae {
                gajae.subcommand =
                    normalize_text(Some(gajae.subcommand.clone())).unwrap_or_default();
                gajae.args = gajae
                    .args
                    .iter()
                    .filter_map(|arg| normalize_text(Some(arg.clone())))
                    .collect();
            }
        }

        for repo in &mut self.monitors.git.repos {
            repo.channel = normalize_text(repo.channel.clone());
            repo.channel_name = normalize_text(repo.channel_name.clone());
            repo.mention = normalize_text(repo.mention.clone());
            repo.name = normalize_text(repo.name.clone());
            repo.github_repo = normalize_text(repo.github_repo.clone());
        }

        for session in &mut self.monitors.tmux.sessions {
            session.channel = normalize_text(session.channel.clone());
            session.channel_name = normalize_text(session.channel_name.clone());
            session.mention = normalize_text(session.mention.clone());
        }

        for workspace in &mut self.monitors.workspace {
            workspace.path = normalize_text(Some(workspace.path.clone())).unwrap_or_default();
            workspace.channel = normalize_text(workspace.channel.clone());
            workspace.mention = normalize_text(workspace.mention.clone());
            workspace.watch_dirs = workspace
                .watch_dirs
                .iter()
                .filter_map(|dir| normalize_text(Some(dir.clone())))
                .collect();
            if workspace.watch_dirs.is_empty() {
                workspace.watch_dirs = default_workspace_watch_dirs();
            }
            workspace.events = workspace
                .events
                .iter()
                .filter_map(|event| normalize_text(Some(event.clone())))
                .collect();
            workspace.debounce_ms = workspace.debounce_ms.max(1);
            workspace.poll_interval_secs = workspace.poll_interval_secs.map(|secs| secs.max(1));
        }

        self.discord_watch.gaebal_gajae_user_id =
            normalize_text(Some(self.discord_watch.gaebal_gajae_user_id.clone()))
                .unwrap_or_else(default_gaebal_gajae_user_id);
        self.discord_watch.owner_user_ids = self
            .discord_watch
            .owner_user_ids
            .iter()
            .filter_map(|id| normalize_text(Some(id.clone())))
            .collect();
        self.discord_watch.banned_channel_ids = self
            .discord_watch
            .banned_channel_ids
            .iter()
            .filter_map(|id| normalize_text(Some(id.clone())))
            .collect();
        self.discord_watch.banned_channel_names = self
            .discord_watch
            .banned_channel_names
            .iter()
            .filter_map(|name| {
                normalize_text(Some(name.trim_start_matches('#').to_ascii_lowercase()))
            })
            .collect();

        for job in &mut self.cron.jobs {
            job.id = normalize_text(Some(job.id.clone())).unwrap_or_default();
            job.schedule = normalize_text(Some(job.schedule.clone())).unwrap_or_default();
            job.timezone =
                normalize_text(Some(job.timezone.clone())).unwrap_or_else(default_cron_timezone);
            job.channel = normalize_text(job.channel.clone());
            job.mention = normalize_text(job.mention.clone());
            match &mut job.kind {
                CronJobKind::CustomMessage { message } => {
                    *message = normalize_text(Some(message.clone())).unwrap_or_default();
                }
            }
        }
    }

    fn routes_with_webhooks(&self) -> usize {
        self.routes
            .iter()
            .filter(|route| route.has_any_webhook_target())
            .count()
    }
}

fn is_repo_binding_route(route: &RouteRule, repo: &str) -> bool {
    route.event == "*"
        && route.sink.trim() == "discord"
        && route.effective_sink() == "discord"
        && route.filter.len() == 1
        && route.filter.get("repo").is_some_and(|value| value == repo)
        && route
            .channel
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
        && route.thread.is_none()
        && route.webhook.is_none()
        && route.slack_webhook.is_none()
        && route.local_path.is_none()
        && route.mention.is_none()
        && route.template.is_none()
        && route.gajae.is_none()
        && !route.allow_dynamic_tokens
        && route.format.is_none()
}

fn is_owner_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty())
}

fn is_setup_owned_git_monitor(monitor: &GitRepoMonitor) -> bool {
    monitor.setup_owned
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug)]
enum ActiveConfigState {
    Absent,
    Present {
        identity: Option<FileIdentity>,
        bytes: Vec<u8>,
    },
}

impl ActiveConfigState {
    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Absent => None,
            Self::Present { bytes, .. } => Some(bytes),
        }
    }

    fn identity(&self) -> Option<FileIdentity> {
        match self {
            Self::Absent => None,
            Self::Present { identity, .. } => *identity,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug, Clone)]
struct ConfigParentDir {
    path: PathBuf,
    identity: Option<FileIdentity>,
}

#[derive(Debug, Clone)]
struct ManagedBackupDir {
    path: PathBuf,
    identity: Option<FileIdentity>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum BackupOrigin {
    RootLegacy,
    Managed,
}

impl BackupOrigin {
    fn retention_rank(self) -> u8 {
        match self {
            Self::RootLegacy => 0,
            Self::Managed => 1,
        }
    }
}

#[derive(Debug, Clone)]
struct BackupCandidate {
    origin: BackupOrigin,
    /// Absolute path retained for adversarial test hooks and diagnostics.
    #[allow(dead_code)]
    path: PathBuf,
    entry_name: String,
    created_at: OffsetDateTime,
    identity: FileIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidatePreserveReason {
    NonRegular,
    IdentityChanged,
    MultipleLinks,
    ProtectedIdentity,
    Unreadable,
    #[cfg(target_os = "linux")]
    CompatibilityProbeUnavailable,
    #[cfg(target_os = "linux")]
    ReadLeaseContended,
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    UnsupportedPlatform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateDeletionOutcome {
    Deleted,
    Disappeared,
    Preserved(CandidatePreserveReason),
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateNameState {
    Missing,
    NonRegular,
    Regular { identity: FileIdentity, nlink: u64 },
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
enum CandidateReadability {
    Readable { descriptor: File },
    Preserved(CandidatePreserveReason),
}

#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static FORCE_FACCESSAT2_ENOSYS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_FACCESSAT2_EPERM: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_PROC_FD_UNAVAILABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FORCE_PROC_FD_REOPEN_EAGAIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn metadata_identity(metadata: &Metadata) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        Some(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}
#[cfg(unix)]
struct BoundDir {
    file: File,
    identity: FileIdentity,
}

#[cfg(unix)]
impl BoundDir {
    fn open_verified(path: &Path, expected: Option<FileIdentity>) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(path)
            .map_err(|error| {
                format!(
                    "failed to open directory for contained IO {}: {error}",
                    path.display()
                )
            })?;
        let metadata = file.metadata().map_err(|error| {
            format!(
                "failed to stat directory for contained IO {}: {error}",
                path.display()
            )
        })?;
        if !metadata.is_dir() {
            return Err(format!(
                "path is not a regular directory for contained IO: {}",
                path.display()
            )
            .into());
        }
        let identity = metadata_identity(&metadata).ok_or_else(|| {
            format!(
                "directory identity unavailable for contained IO: {}",
                path.display()
            )
        })?;
        if expected.is_some_and(|expected| expected != identity) {
            return Err(format!("directory changed during use: {}", path.display()).into());
        }
        Ok(Self { file, identity })
    }

    fn revalidate(&self, expected: Option<FileIdentity>) -> Result<()> {
        let metadata = self
            .file
            .metadata()
            .map_err(|error| format!("failed to revalidate bound directory: {error}"))?;
        let identity = metadata_identity(&metadata);
        if !metadata.is_dir()
            || identity != Some(self.identity)
            || expected.is_some_and(|expected| Some(expected) != identity)
        {
            return Err("bound directory changed during use".to_string().into());
        }
        Ok(())
    }

    fn fd(&self) -> libc::c_int {
        self.file.as_raw_fd()
    }
}

#[cfg(unix)]
fn openat_exclusive_write(dir: &BoundDir, name: &str) -> std::result::Result<File, io::Error> {
    let c_name = CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "entry name contains interior NUL",
        )
    })?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(dir.fd(), c_name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn openat_path_nofollow(dir: &BoundDir, name: &str) -> std::result::Result<File, io::Error> {
    let c_name = CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "entry name contains interior NUL",
        )
    })?;
    let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(dir.fd(), c_name.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "macos")]
fn openat_read_nonblocking_nofollow(
    dir: &BoundDir,
    name: &str,
) -> std::result::Result<File, io::Error> {
    let c_name = CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "entry name contains interior NUL",
        )
    })?;
    // Darwin guarantees O_NONBLOCK returns immediately if open would otherwise block.
    let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(dir.fd(), c_name.as_ptr(), flags, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(target_os = "macos")]
fn apple_candidate_open_error_needs_state_check(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EACCES)
            | Some(libc::EPERM)
            | Some(libc::ELOOP)
            | Some(libc::ENXIO)
            | Some(libc::ENODEV)
            | Some(libc::EOPNOTSUPP)
            | Some(libc::EAGAIN)
            | Some(libc::EBUSY)
            | Some(libc::EISDIR)
    )
}

#[cfg(target_os = "linux")]
fn classify_candidate_path_open_error(
    error: io::Error,
) -> std::result::Result<CandidateDeletionOutcome, io::Error> {
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(CandidateDeletionOutcome::Disappeared)
    } else {
        Err(error)
    }
}

#[cfg(target_os = "linux")]
fn classify_candidate_readability_error(
    error: io::Error,
) -> std::result::Result<CandidateReadability, io::Error> {
    if error.raw_os_error() == Some(libc::EACCES) {
        Ok(CandidateReadability::Preserved(
            CandidatePreserveReason::Unreadable,
        ))
    } else {
        Err(error)
    }
}

#[cfg(target_os = "linux")]
fn classify_proc_fd_reopen_error(
    error: io::Error,
) -> std::result::Result<CandidateReadability, io::Error> {
    match error.raw_os_error() {
        Some(libc::EACCES) | Some(libc::EPERM) => Ok(CandidateReadability::Preserved(
            CandidatePreserveReason::Unreadable,
        )),
        Some(libc::EAGAIN) => Ok(CandidateReadability::Preserved(
            CandidatePreserveReason::ReadLeaseContended,
        )),
        _ => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn classify_proc_fd_dir_error(
    error: io::Error,
) -> std::result::Result<CandidateReadability, io::Error> {
    if matches!(
        error.raw_os_error(),
        Some(libc::ENOENT) | Some(libc::ENOTDIR) | Some(libc::EACCES) | Some(libc::EPERM)
    ) {
        Ok(CandidateReadability::Preserved(
            CandidatePreserveReason::CompatibilityProbeUnavailable,
        ))
    } else {
        Err(error)
    }
}

#[cfg(target_os = "linux")]
fn faccessat2_read_access(file: &File) -> std::result::Result<(), io::Error> {
    #[cfg(test)]
    if FORCE_FACCESSAT2_ENOSYS.with(std::cell::Cell::get) {
        return Err(io::Error::from_raw_os_error(libc::ENOSYS));
    }
    #[cfg(test)]
    if FORCE_FACCESSAT2_EPERM.with(std::cell::Cell::get) {
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }

    const EMPTY_PATH: &[u8] = b"\0";
    let flags = libc::AT_EMPTY_PATH | libc::AT_EACCESS;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            file.as_raw_fd(),
            EMPTY_PATH.as_ptr().cast::<libc::c_char>(),
            libc::R_OK,
            flags,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn candidate_readability_via_proc_fd(
    file: &File,
) -> std::result::Result<CandidateReadability, io::Error> {
    // Reopen the already-held O_PATH object, not the mutable candidate pathname.
    // Each proc-fd descriptor is identity-checked so path substitution fails closed.
    #[cfg(test)]
    if FORCE_PROC_FD_UNAVAILABLE.with(std::cell::Cell::get) {
        return classify_proc_fd_dir_error(io::Error::from_raw_os_error(libc::ENOENT));
    }
    let proc_fd_dir = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open("/proc/self/fd")
    {
        Ok(dir) => dir,
        Err(error) => return classify_proc_fd_dir_error(error),
    };
    let fd_name = CString::new(file.as_raw_fd().to_string()).expect("fd contains no NUL");
    let anchored_fd = unsafe {
        libc::openat(
            proc_fd_dir.as_raw_fd(),
            fd_name.as_ptr(),
            libc::O_PATH | libc::O_CLOEXEC,
            0,
        )
    };
    if anchored_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let anchored = unsafe { File::from_raw_fd(anchored_fd) };
    let original_metadata = file.metadata()?;
    let anchored_metadata = anchored.metadata()?;
    if !anchored_metadata.is_file()
        || metadata_identity(&anchored_metadata) != metadata_identity(&original_metadata)
    {
        return Err(io::Error::other(
            "proc fd readability check anchored a different backup candidate",
        ));
    }

    #[cfg(test)]
    if FORCE_PROC_FD_REOPEN_EAGAIN.with(std::cell::Cell::get) {
        return classify_proc_fd_reopen_error(io::Error::from_raw_os_error(libc::EAGAIN));
    }
    let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC;
    let reopened_fd = unsafe { libc::openat(proc_fd_dir.as_raw_fd(), fd_name.as_ptr(), flags, 0) };
    if reopened_fd < 0 {
        return classify_proc_fd_reopen_error(io::Error::last_os_error());
    }
    let reopened = unsafe { File::from_raw_fd(reopened_fd) };
    let reopened_metadata = reopened.metadata()?;
    if !reopened_metadata.is_file()
        || metadata_identity(&reopened_metadata) != metadata_identity(&original_metadata)
    {
        return Err(io::Error::other(
            "proc fd readability check reopened a different backup candidate",
        ));
    }
    Ok(CandidateReadability::Readable {
        descriptor: reopened,
    })
}

#[cfg(target_os = "linux")]
fn candidate_readability(file: &File) -> std::result::Result<CandidateReadability, io::Error> {
    match faccessat2_read_access(file) {
        Ok(()) => candidate_readability_via_proc_fd(file),
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EPERM)) => {
            candidate_readability_via_proc_fd(file)
        }
        Err(error) => classify_candidate_readability_error(error),
    }
}

#[cfg(unix)]
fn fstatat_regular_identity(
    dir: &BoundDir,
    name: &str,
) -> std::result::Result<Option<(FileIdentity, u64)>, io::Error> {
    let c_name = CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "entry name contains interior NUL",
        )
    })?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            dir.fd(),
            c_name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(err);
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Ok(None);
    }
    Ok(Some((
        FileIdentity {
            device: st.st_dev as u64,
            inode: st.st_ino as u64,
        },
        st.st_nlink as u64,
    )))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn fstatat_candidate_state(
    dir: &BoundDir,
    name: &str,
) -> std::result::Result<CandidateNameState, io::Error> {
    let c_name = CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "entry name contains interior NUL",
        )
    })?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            dir.fd(),
            c_name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(CandidateNameState::Missing);
        }
        return Err(error);
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Ok(CandidateNameState::NonRegular);
    }
    Ok(CandidateNameState::Regular {
        identity: FileIdentity {
            device: st.st_dev as u64,
            inode: st.st_ino as u64,
        },
        nlink: st.st_nlink as u64,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn candidate_name_state_outcome(
    state: CandidateNameState,
    expected_identity: FileIdentity,
    protected_identities: &[FileIdentity],
) -> Option<CandidateDeletionOutcome> {
    match state {
        CandidateNameState::Missing => Some(CandidateDeletionOutcome::Disappeared),
        CandidateNameState::NonRegular => Some(CandidateDeletionOutcome::Preserved(
            CandidatePreserveReason::NonRegular,
        )),
        CandidateNameState::Regular { identity, nlink } => {
            if identity != expected_identity {
                Some(CandidateDeletionOutcome::Preserved(
                    CandidatePreserveReason::IdentityChanged,
                ))
            } else if nlink != 1 {
                Some(CandidateDeletionOutcome::Preserved(
                    CandidatePreserveReason::MultipleLinks,
                ))
            } else if protected_identities.contains(&identity) {
                Some(CandidateDeletionOutcome::Preserved(
                    CandidatePreserveReason::ProtectedIdentity,
                ))
            } else {
                None
            }
        }
    }
}

#[cfg(unix)]
fn renameat_names(dir: &BoundDir, old: &str, new: &str) -> std::result::Result<(), io::Error> {
    let c_old = CString::new(old).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "entry name contains interior NUL",
        )
    })?;
    let c_new = CString::new(new).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "entry name contains interior NUL",
        )
    })?;
    let rc = unsafe { libc::renameat(dir.fd(), c_old.as_ptr(), dir.fd(), c_new.as_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn unlinkat_name(dir: &BoundDir, name: &str) -> std::result::Result<(), io::Error> {
    let c_name = CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "entry name contains interior NUL",
        )
    })?;
    let rc = unsafe { libc::unlinkat(dir.fd(), c_name.as_ptr(), 0) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn commit_config_bytes_via_temp<G>(
    parent_dir: &ConfigParentDir,
    path: &Path,
    filename: &str,
    new_bytes: &[u8],
    active: &ActiveConfigState,
    before_rename: &mut G,
) -> Result<()>
where
    G: FnMut() -> Result<()>,
{
    let temp_name = format!(".{filename}.tmp.{}", std::process::id());

    #[cfg(unix)]
    {
        let dir = BoundDir::open_verified(&parent_dir.path, parent_dir.identity)?;
        let mut temp = match openat_exclusive_write(&dir, &temp_name) {
            Ok(temp) => temp,
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists
                    || error.raw_os_error() == Some(libc::ELOOP) =>
            {
                return Err(
                    format!("config temp collision at {temp_name}; config was not saved").into(),
                );
            }
            Err(error) => {
                return Err(format!(
                    "failed to create config temp {temp_name}; config was not saved: {error}"
                )
                .into());
            }
        };
        if let Err(error) = temp.write_all(new_bytes) {
            let _ = unlinkat_name(&dir, &temp_name);
            return Err(format!(
                "failed to write config temp {temp_name}; config was not saved: {error}"
            )
            .into());
        }
        let temp_identity = metadata_identity(&temp.metadata().map_err(|error| {
            let _ = unlinkat_name(&dir, &temp_name);
            format!("failed to identity config temp {temp_name}; config was not saved: {error}")
        })?)
        .ok_or_else(|| {
            let _ = unlinkat_name(&dir, &temp_name);
            format!("config temp identity unavailable for {temp_name}; config was not saved")
        })?;
        before_rename().inspect_err(|_| {
            let _ = unlinkat_name(&dir, &temp_name);
        })?;
        dir.revalidate(parent_dir.identity).inspect_err(|_| {
            let _ = unlinkat_name(&dir, &temp_name);
        })?;
        revalidate_active_config(path, active).inspect_err(|_| {
            let _ = unlinkat_name(&dir, &temp_name);
        })?;
        match fstatat_regular_identity(&dir, &temp_name) {
            Ok(Some((identity, _))) if identity == temp_identity => {}
            Ok(_) => {
                let _ = unlinkat_name(&dir, &temp_name);
                return Err(format!(
                    "config temp path changed before commit at {temp_name}; config was not saved"
                )
                .into());
            }
            Err(error) => {
                let _ = unlinkat_name(&dir, &temp_name);
                return Err(format!(
                    "failed to revalidate config temp {temp_name}; config was not saved: {error}"
                )
                .into());
            }
        }
        if let Err(error) = renameat_names(&dir, &temp_name, filename) {
            let _ = unlinkat_name(&dir, &temp_name);
            return Err(format!(
                "failed to commit config via {temp_name}; config was not saved: {error}"
            )
            .into());
        }
        match fstatat_regular_identity(&dir, filename) {
            Ok(Some((identity, _))) if identity == temp_identity => Ok(()),
            Ok(_) => Err(format!(
                "config commit identity mismatch at {}; config path may be unsafe",
                path.display()
            )
            .into()),
            Err(error) => Err(format!(
                "failed to verify committed config at {}; config path may be unsafe: {error}",
                path.display()
            )
            .into()),
        }
    }

    #[cfg(not(unix))]
    {
        let temp_path = parent_dir.path.join(&temp_name);
        let mut temp = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(temp) => temp,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(format!(
                    "config temp collision at {}; config was not saved",
                    temp_path.display()
                )
                .into());
            }
            Err(error) => {
                return Err(format!(
                    "failed to create config temp {}; config was not saved: {error}",
                    temp_path.display()
                )
                .into());
            }
        };
        temp.write_all(new_bytes).map_err(|error| {
            let _ = fs::remove_file(&temp_path);
            format!(
                "failed to write config temp {}; config was not saved: {error}",
                temp_path.display()
            )
        })?;
        before_rename().inspect_err(|_| {
            let _ = fs::remove_file(&temp_path);
        })?;
        revalidate_config_parent_dir(parent_dir).inspect_err(|_| {
            let _ = fs::remove_file(&temp_path);
        })?;
        revalidate_active_config(path, active).inspect_err(|_| {
            let _ = fs::remove_file(&temp_path);
        })?;
        fs::rename(&temp_path, path).map_err(|error| {
            let _ = fs::remove_file(&temp_path);
            format!(
                "failed to commit config via {}; config was not saved: {error}",
                temp_path.display()
            )
        })?;
        Ok(())
    }
}

fn config_parent_dir_from_metadata(path: PathBuf, metadata: Metadata) -> Result<ConfigParentDir> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "config parent path must be a regular directory, not a symlink: {}",
            path.display()
        )
        .into());
    }
    let identity = metadata_identity(&metadata);
    Ok(ConfigParentDir { path, identity })
}

fn validate_config_parent_dir(parent: &Path, create: bool) -> Result<ConfigParentDir> {
    match fs::symlink_metadata(parent) {
        Ok(metadata) => config_parent_dir_from_metadata(parent.to_path_buf(), metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            fs::create_dir_all(parent)?;
            let metadata = fs::symlink_metadata(parent)?;
            config_parent_dir_from_metadata(parent.to_path_buf(), metadata)
        }
        Err(error) => Err(error.into()),
    }
}

fn revalidate_config_parent_dir(parent_dir: &ConfigParentDir) -> Result<()> {
    let metadata = fs::symlink_metadata(&parent_dir.path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || parent_dir
            .identity
            .is_some_and(|identity| metadata_identity(&metadata) != Some(identity))
    {
        return Err(format!(
            "config parent directory changed during use: {}",
            parent_dir.path.display()
        )
        .into());
    }
    Ok(())
}

fn read_active_config_no_follow(path: &Path) -> Result<ActiveConfigState> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ActiveConfigState::Absent);
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "active config path must be a regular non-symlink file: {}",
            path.display()
        )
        .into());
    }
    let identity = metadata_identity(&metadata);
    let mut file = File::open(path)?;
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file()
        || identity.is_some_and(|identity| metadata_identity(&opened_metadata) != Some(identity))
    {
        return Err(format!(
            "active config changed while it was being opened: {}",
            path.display()
        )
        .into());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(ActiveConfigState::Present { identity, bytes })
}

fn revalidate_active_config(path: &Path, active: &ActiveConfigState) -> Result<()> {
    match active {
        ActiveConfigState::Absent => match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(format!(
                "active config appeared before replacement; config was not saved: {}",
                path.display()
            )
            .into()),
            Err(error) => Err(error.into()),
        },
        ActiveConfigState::Present { identity, .. } => {
            let metadata = fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || identity.is_some_and(|identity| metadata_identity(&metadata) != Some(identity))
            {
                return Err(format!(
                    "active config changed before replacement; config was not saved: {}",
                    path.display()
                )
                .into());
            }
            Ok(())
        }
    }
}

fn current_active_config_identity(path: &Path) -> Result<Option<FileIdentity>> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "saved config is not a regular non-symlink file: {}",
            path.display()
        )
        .into());
    }
    Ok(metadata_identity(&metadata))
}

fn managed_backup_dir_from_metadata(path: PathBuf, metadata: Metadata) -> Result<ManagedBackupDir> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "managed config backup path must be a regular directory, not a symlink: {}",
            path.display()
        )
        .into());
    }
    let identity = metadata_identity(&metadata);
    Ok(ManagedBackupDir { path, identity })
}

fn validate_managed_backup_dir(parent: &Path, create: bool) -> Result<Option<ManagedBackupDir>> {
    let path = parent.join(".clawhip-config-backups");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => managed_backup_dir_from_metadata(path, metadata).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound && !create => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match fs::create_dir(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            let metadata = fs::symlink_metadata(&path)?;
            managed_backup_dir_from_metadata(path, metadata).map(Some)
        }
        Err(error) => Err(error.into()),
    }
}

fn revalidate_managed_backup_dir(managed_dir: &ManagedBackupDir) -> Result<()> {
    let metadata = fs::symlink_metadata(&managed_dir.path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || managed_dir
            .identity
            .is_some_and(|identity| metadata_identity(&metadata) != Some(identity))
    {
        return Err(format!(
            "managed config backup directory changed during use: {}",
            managed_dir.path.display()
        )
        .into());
    }
    Ok(())
}

fn sha256_first8(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")[..8].to_string()
}

fn utc_backup_timestamp(now: OffsetDateTime) -> Result<String> {
    let format = format_description::parse("[year][month][day]T[hour][minute][second]Z")?;
    Ok(now.format(&format)?)
}

fn create_managed_snapshot_create_new<H>(
    managed_dir: &ManagedBackupDir,
    filename: &str,
    old_bytes: &[u8],
    now: OffsetDateTime,
    before_create: &mut H,
) -> Result<()>
where
    H: FnMut() -> Result<()>,
{
    let timestamp = utc_backup_timestamp(now)?;
    let entry_name = format!("{filename}.{timestamp}.{}.bak", sha256_first8(old_bytes));

    #[cfg(unix)]
    {
        let dir = BoundDir::open_verified(&managed_dir.path, managed_dir.identity)?;
        before_create()?;
        dir.revalidate(managed_dir.identity)?;
        let mut backup = match openat_exclusive_write(&dir, &entry_name) {
            Ok(backup) => backup,
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists
                    || error.raw_os_error() == Some(libc::ELOOP) =>
            {
                return Err(format!(
                    "config backup collision at {}; config was not saved",
                    managed_dir.path.join(&entry_name).display()
                )
                .into());
            }
            Err(error) => {
                return Err(format!(
                    "failed to create config backup {}; config was not saved: {error}",
                    managed_dir.path.join(&entry_name).display()
                )
                .into());
            }
        };
        backup.write_all(old_bytes).map_err(|error| {
            let _ = unlinkat_name(&dir, &entry_name);
            format!(
                "failed to write config backup {}; config was not saved: {error}",
                managed_dir.path.join(&entry_name).display()
            )
        })?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        revalidate_managed_backup_dir(managed_dir)?;
        before_create()?;
        revalidate_managed_backup_dir(managed_dir)?;
        let backup_path = managed_dir.path.join(&entry_name);
        let mut backup = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup_path)
        {
            Ok(backup) => backup,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(format!(
                    "config backup collision at {}; config was not saved",
                    backup_path.display()
                )
                .into());
            }
            Err(error) => {
                return Err(format!(
                    "failed to create config backup {}; config was not saved: {error}",
                    backup_path.display()
                )
                .into());
            }
        };
        backup.write_all(old_bytes).map_err(|error| {
            format!(
                "failed to write config backup {}; config was not saved: {error}",
                backup_path.display()
            )
        })?;
        Ok(())
    }
}

fn parse_managed_backup_name(name: &str, filename: &str) -> Option<OffsetDateTime> {
    let prefix = format!("{filename}.");
    let rest = name.strip_prefix(&prefix)?.strip_suffix(".bak")?;
    let (timestamp, hash) = rest.split_once('.')?;
    if hash.len() != 8 || !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    let format = format_description::parse("[year][month][day]T[hour][minute][second]Z").ok()?;
    PrimitiveDateTime::parse(timestamp, &format)
        .ok()
        .map(|value| value.assume_utc())
}

fn parse_compact_backup_timestamp_seconds(value: &str) -> Option<OffsetDateTime> {
    let format = format_description::parse("[year][month][day]T[hour][minute][second]Z").ok()?;
    PrimitiveDateTime::parse(value, &format)
        .ok()
        .map(|timestamp| timestamp.assume_utc())
}

fn parse_compact_backup_timestamp_minutes(value: &str) -> Option<OffsetDateTime> {
    let format = format_description::parse("[year][month][day]T[hour][minute]Z").ok()?;
    PrimitiveDateTime::parse(value, &format)
        .ok()
        .map(|timestamp| timestamp.assume_utc())
}

fn parse_compact_backup_date(value: &str) -> Option<OffsetDateTime> {
    let format = format_description::parse("[year][month][day]").ok()?;
    Date::parse(value, &format)
        .ok()
        .map(|date| date.midnight().assume_utc())
}

fn valid_legacy_backup_label_prefix(prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let Some(label) = prefix.strip_suffix('-') else {
        return false;
    };
    !label.is_empty()
        && label.bytes().any(|byte| byte.is_ascii_alphanumeric())
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn parse_root_legacy_backup_name(name: &str, filename: &str) -> Option<OffsetDateTime> {
    let prefix = format!("{filename}.bak-");
    let suffix = name.strip_prefix(&prefix)?;
    for (length, parser) in [
        (
            16,
            parse_compact_backup_timestamp_seconds as fn(&str) -> Option<OffsetDateTime>,
        ),
        (14, parse_compact_backup_timestamp_minutes),
        (8, parse_compact_backup_date),
    ] {
        if suffix.len() < length {
            continue;
        }
        let (label_prefix, timestamp) = suffix.split_at(suffix.len() - length);
        if let Some(created_at) = parser(timestamp) {
            return valid_legacy_backup_label_prefix(label_prefix).then_some(created_at);
        }
    }
    None
}

fn parse_duplicated_legacy_backup_name(name: &str, filename: &str) -> Option<OffsetDateTime> {
    let prefix = format!("config.{filename}.bak-");
    let timestamp = name.strip_prefix(&prefix)?;
    let format = format_description::parse("[year]-[month]-[day]-[hour][minute]").ok()?;
    PrimitiveDateTime::parse(timestamp, &format)
        .ok()
        .map(|value| value.assume_utc())
}

fn safe_cleanup_file_identity(metadata: &Metadata) -> Option<FileIdentity> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return None;
    }
    metadata_identity(metadata)
}

fn collect_cleanup_candidates(
    parent_dir: &ConfigParentDir,
    filename: &str,
    managed_dir: Option<&ManagedBackupDir>,
    protected_identities: &[FileIdentity],
) -> Result<Vec<BackupCandidate>> {
    revalidate_config_parent_dir(parent_dir)?;
    let mut candidates = Vec::new();
    for entry in fs::read_dir(&parent_dir.path)? {
        let entry = entry?;
        let entry_name = entry.file_name();
        let Some(entry_name) = entry_name.to_str() else {
            continue;
        };
        let Some(created_at) = parse_root_legacy_backup_name(entry_name, filename)
            .or_else(|| parse_duplicated_legacy_backup_name(entry_name, filename))
        else {
            continue;
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let Some(identity) = safe_cleanup_file_identity(&metadata) else {
            continue;
        };
        if protected_identities.contains(&identity) {
            continue;
        }
        candidates.push(BackupCandidate {
            origin: BackupOrigin::RootLegacy,
            path,
            entry_name: entry_name.to_string(),
            created_at,
            identity,
        });
    }

    if let Some(managed_dir) = managed_dir {
        revalidate_managed_backup_dir(managed_dir)?;
        for entry in fs::read_dir(&managed_dir.path)? {
            let entry = entry?;
            let entry_name = entry.file_name();
            let Some(entry_name) = entry_name.to_str() else {
                continue;
            };
            let Some(created_at) = parse_managed_backup_name(entry_name, filename) else {
                continue;
            };
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let Some(identity) = safe_cleanup_file_identity(&metadata) else {
                continue;
            };
            if protected_identities.contains(&identity) {
                continue;
            }
            candidates.push(BackupCandidate {
                origin: BackupOrigin::Managed,
                path,
                entry_name: entry_name.to_string(),
                created_at,
                identity,
            });
        }
    }
    Ok(candidates)
}

fn delete_verified_candidate<P, A>(
    candidate: &BackupCandidate,
    parent_dir: &ConfigParentDir,
    managed_dir: Option<&ManagedBackupDir>,
    protected_identities: &[FileIdentity],
    after_preflight: &mut P,
    after_check: &mut A,
) -> Result<CandidateDeletionOutcome>
where
    P: FnMut(&BackupCandidate) -> Result<()>,
    A: FnMut(&BackupCandidate) -> Result<()>,
{
    #[cfg(unix)]
    {
        let (dir_path, expected_identity) = match candidate.origin {
            BackupOrigin::RootLegacy => {
                revalidate_config_parent_dir(parent_dir)?;
                (parent_dir.path.as_path(), parent_dir.identity)
            }
            BackupOrigin::Managed => {
                revalidate_config_parent_dir(parent_dir)?;
                let managed_dir = managed_dir.ok_or_else(|| {
                    "managed backup candidate has no validated directory".to_string()
                })?;
                revalidate_managed_backup_dir(managed_dir)?;
                (managed_dir.path.as_path(), managed_dir.identity)
            }
        };
        let dir = BoundDir::open_verified(dir_path, expected_identity)?;

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if let Some(outcome) = candidate_name_state_outcome(
                fstatat_candidate_state(&dir, &candidate.entry_name)?,
                candidate.identity,
                protected_identities,
            ) {
                return Ok(outcome);
            }

            after_preflight(candidate)?;
            #[cfg(target_os = "linux")]
            let file = match openat_path_nofollow(&dir, &candidate.entry_name) {
                Ok(file) => file,
                Err(error) => return Ok(classify_candidate_path_open_error(error)?),
            };
            #[cfg(target_os = "macos")]
            let file = match openat_read_nonblocking_nofollow(&dir, &candidate.entry_name) {
                Ok(file) => file,
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                    return Ok(CandidateDeletionOutcome::Disappeared);
                }
                Err(error) if apple_candidate_open_error_needs_state_check(&error) => {
                    if let Some(outcome) = candidate_name_state_outcome(
                        fstatat_candidate_state(&dir, &candidate.entry_name)?,
                        candidate.identity,
                        protected_identities,
                    ) {
                        return Ok(outcome);
                    }
                    if matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) {
                        return Ok(CandidateDeletionOutcome::Preserved(
                            CandidatePreserveReason::Unreadable,
                        ));
                    }
                    return Err(error.into());
                }
                Err(error) => return Err(error.into()),
            };

            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Ok(CandidateDeletionOutcome::Preserved(
                    CandidatePreserveReason::NonRegular,
                ));
            }
            if metadata.nlink() != 1 {
                return Ok(CandidateDeletionOutcome::Preserved(
                    CandidatePreserveReason::MultipleLinks,
                ));
            }
            let identity = metadata_identity(&metadata)
                .ok_or_else(|| "backup candidate descriptor identity unavailable".to_string())?;
            if identity != candidate.identity {
                return Ok(CandidateDeletionOutcome::Preserved(
                    CandidatePreserveReason::IdentityChanged,
                ));
            }
            if protected_identities.contains(&identity) {
                return Ok(CandidateDeletionOutcome::Preserved(
                    CandidatePreserveReason::ProtectedIdentity,
                ));
            }
            #[cfg(target_os = "linux")]
            let _readable_descriptor = match candidate_readability(&file)? {
                CandidateReadability::Readable { descriptor } => descriptor,
                CandidateReadability::Preserved(reason) => {
                    return Ok(CandidateDeletionOutcome::Preserved(reason));
                }
            };

            after_check(candidate)?;
            if let Some(outcome) = candidate_name_state_outcome(
                fstatat_candidate_state(&dir, &candidate.entry_name)?,
                identity,
                protected_identities,
            ) {
                return Ok(outcome);
            }
            #[cfg(target_os = "linux")]
            let _final_readable_descriptor = match candidate_readability(&file)? {
                CandidateReadability::Readable { descriptor } => descriptor,
                CandidateReadability::Preserved(reason) => {
                    return Ok(CandidateDeletionOutcome::Preserved(reason));
                }
            };
            match unlinkat_name(&dir, &candidate.entry_name) {
                Ok(()) => Ok(CandidateDeletionOutcome::Deleted),
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                    Ok(CandidateDeletionOutcome::Disappeared)
                }
                Err(error) => Err(error.into()),
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (
                dir,
                candidate,
                protected_identities,
                after_preflight,
                after_check,
            );
            Ok(CandidateDeletionOutcome::Preserved(
                CandidatePreserveReason::UnsupportedPlatform,
            ))
        }
    }

    #[cfg(not(unix))]
    {
        // Same-object unlink is unavailable without directory-relative primitives.
        // Fail closed: preserve the candidate rather than unlinking by path alone.
        let _ = (
            candidate,
            parent_dir,
            managed_dir,
            protected_identities,
            after_preflight,
            after_check,
        );
        Ok(CandidateDeletionOutcome::Preserved(
            CandidatePreserveReason::UnsupportedPlatform,
        ))
    }
}

#[cfg(test)]
fn cleanup_config_backups_with<F>(
    parent: &Path,
    expected_parent_identity: Option<FileIdentity>,
    filename: &str,
    managed_dir: Option<&ManagedBackupDir>,
    protected_identities: &[FileIdentity],
    now: OffsetDateTime,
    before_delete: &mut F,
) -> Result<()>
where
    F: FnMut(&BackupCandidate) -> Result<()>,
{
    let mut after_preflight = |_: &BackupCandidate| Ok(());
    let mut after_check = |_: &BackupCandidate| Ok(());
    cleanup_config_backups_with_hooks(
        parent,
        expected_parent_identity,
        filename,
        managed_dir,
        protected_identities,
        now,
        before_delete,
        &mut after_preflight,
        &mut after_check,
    )
}

#[allow(clippy::too_many_arguments)]
fn cleanup_config_backups_with_hooks<F, P, A>(
    parent: &Path,
    expected_parent_identity: Option<FileIdentity>,
    filename: &str,
    managed_dir: Option<&ManagedBackupDir>,
    protected_identities: &[FileIdentity],
    now: OffsetDateTime,
    before_delete: &mut F,
    after_preflight: &mut P,
    after_delete_check: &mut A,
) -> Result<()>
where
    F: FnMut(&BackupCandidate) -> Result<()>,
    P: FnMut(&BackupCandidate) -> Result<()>,
    A: FnMut(&BackupCandidate) -> Result<()>,
{
    let parent_dir = validate_config_parent_dir(parent, false)?;
    if expected_parent_identity.is_some_and(|identity| parent_dir.identity != Some(identity)) {
        return Err(format!(
            "config parent directory changed during use: {}",
            parent.display()
        )
        .into());
    }
    if parent_dir.identity.is_none() {
        return Ok(());
    }
    let cutoff = now - TimeDuration::days(30);
    let mut candidates =
        collect_cleanup_candidates(&parent_dir, filename, managed_dir, protected_identities)?;
    candidates.sort_by(|left, right| {
        (
            left.created_at,
            left.origin.retention_rank(),
            left.entry_name.as_str(),
        )
            .cmp(&(
                right.created_at,
                right.origin.retention_rank(),
                right.entry_name.as_str(),
            ))
    });
    let keep_from = candidates.len().saturating_sub(10);
    for (index, candidate) in candidates.iter().enumerate() {
        if index < keep_from && candidate.created_at <= cutoff {
            before_delete(candidate)?;
            match delete_verified_candidate(
                candidate,
                &parent_dir,
                managed_dir,
                protected_identities,
                after_preflight,
                after_delete_check,
            )? {
                CandidateDeletionOutcome::Deleted | CandidateDeletionOutcome::Disappeared => {}
                CandidateDeletionOutcome::Preserved(reason) => {
                    let _ = reason;
                }
            }
        }
    }
    Ok(())
}

fn is_canonical_quickstart_route(route: &RouteRule) -> bool {
    route.event == "*"
        && route.filter.is_empty()
        && route.sink.trim() == "discord"
        && route.channel.is_none()
        && route.slack_webhook.is_none()
        && route.mention.is_none()
        && route.template.is_none()
        && !route.allow_dynamic_tokens
        && route.format.is_none()
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}: ");
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim_end().to_string())
}

fn prompt_with_default(label: &str, default: Option<&str>) -> Result<String> {
    let value = match default {
        Some(default) => prompt(&format!("{label} [{default}]"))?,
        None => prompt(label)?,
    };

    if value.trim().is_empty() {
        Ok(default.unwrap_or_default().to_string())
    } else {
        Ok(value)
    }
}

fn prompt_format(default: Option<MessageFormat>) -> Result<MessageFormat> {
    let default_value = default.unwrap_or(MessageFormat::Compact);
    let input = prompt(&format!(
        "Format [{}] (compact/alert/inline/raw)",
        default_value.as_str()
    ))?;
    if input.trim().is_empty() {
        return Ok(default_value);
    }
    MessageFormat::from_label(input.trim())
}

fn normalize_text(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn run_bounded_test_child(
        test_name: &str,
        child_env: &str,
        child_marker: &str,
        drop_privileges: bool,
    ) {
        use std::io::Read as _;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::{Duration as StdDuration, Instant};

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env(child_env, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if drop_privileges && unsafe { libc::geteuid() } == 0 {
            command.gid(65534).uid(65534);
        }
        let mut child = command.spawn().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let stdout_reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let stderr_reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let deadline = Instant::now() + StdDuration::from_secs(10);

        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let status = child.wait().unwrap();
                let stdout = stdout_reader.join().unwrap();
                let stderr = stderr_reader.join().unwrap();
                panic!(
                    "test child timed out and was killed/reaped with {status}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                );
            }
            thread::sleep(StdDuration::from_millis(10));
        };
        let stdout = stdout_reader.join().unwrap();
        let stderr = stderr_reader.join().unwrap();
        let stdout_text = String::from_utf8_lossy(&stdout);
        let stderr_text = String::from_utf8_lossy(&stderr);
        assert!(
            status.success(),
            "test child failed with {status}\nstdout:\n{stdout_text}\nstderr:\n{stderr_text}"
        );
        assert!(
            stdout_text.contains(child_marker),
            "test child did not execute the requested scenario\nstdout:\n{stdout_text}\nstderr:\n{stderr_text}"
        );
    }

    #[test]
    fn discord_token_source_prefers_env_over_config() {
        let mut config = AppConfig::default();
        config.providers.discord.bot_token = Some("config-token".into());

        assert_eq!(config.discord_token_source_with(|_| None), "config");
        assert_eq!(
            config.effective_token_with(|_| None).as_deref(),
            Some("config-token")
        );

        let token = config.effective_token_with(|name| {
            (name == "DISCORD_TOKEN").then(|| "env-token".to_string())
        });
        assert_eq!(token.as_deref(), Some("env-token"));
        assert_eq!(
            config.discord_token_source_with(|name| {
                (name == "DISCORD_TOKEN").then(|| "env-token".to_string())
            }),
            "env"
        );
    }

    #[test]
    fn discord_token_source_reports_missing_when_unset() {
        let config = AppConfig::default();

        assert_eq!(config.discord_token_source_with(|_| None), "missing");
        assert_eq!(config.effective_token_with(|_| None), None);
    }

    #[test]
    fn legacy_env_token_is_still_supported() {
        let config = AppConfig::default();

        let token = config.effective_token_with(|name| {
            (name == "CLAWHIP_DISCORD_BOT_TOKEN").then(|| "legacy-token".to_string())
        });

        assert_eq!(token.as_deref(), Some("legacy-token"));
        assert_eq!(
            config.discord_token_source_with(|name| {
                (name == "CLAWHIP_DISCORD_BOT_TOKEN").then(|| "legacy-token".to_string())
            }),
            "env"
        );
    }

    #[test]
    fn provider_discord_token_is_used_when_present() {
        let mut config = AppConfig::default();
        config.providers.discord.bot_token = Some("config-token".into());

        assert_eq!(config.discord_token_source_with(|_| None), "config");
        assert_eq!(
            config.effective_token_with(|_| None).as_deref(),
            Some("config-token")
        );
    }

    #[test]
    fn discord_token_env_shadow_detected_when_env_overrides_config() {
        let mut config = AppConfig::default();
        config.providers.discord.bot_token = Some("config-token".into());

        assert_eq!(
            config.discord_token_env_shadow_with(|name| {
                (name == "CLAWHIP_DISCORD_BOT_TOKEN").then(|| "env-token".to_string())
            }),
            Some("CLAWHIP_DISCORD_BOT_TOKEN")
        );
        assert_eq!(
            config.discord_token_env_shadow_with(|name| {
                (name == "DISCORD_TOKEN").then(|| "env-token".to_string())
            }),
            Some("DISCORD_TOKEN")
        );
    }

    #[test]
    fn discord_token_env_shadow_uses_legacy_config_token() {
        let mut config = AppConfig::default();
        config.discord.bot_token = Some("legacy-config-token".into());

        assert_eq!(
            config.discord_token_env_shadow_with(|name| {
                (name == "DISCORD_TOKEN").then(|| "env-token".to_string())
            }),
            Some("DISCORD_TOKEN")
        );
    }

    #[test]
    fn discord_token_env_shadow_none_without_conflict() {
        let mut config = AppConfig::default();

        // No config token: env wins but nothing is shadowed.
        assert_eq!(
            config.discord_token_env_shadow_with(|name| {
                (name == "DISCORD_TOKEN").then(|| "env-token".to_string())
            }),
            None
        );

        // Config token present but no env token: config wins, no shadow.
        config.providers.discord.bot_token = Some("config-token".into());
        assert_eq!(config.discord_token_env_shadow_with(|_| None), None);
    }

    #[test]
    fn load_or_default_migrates_legacy_discord_to_providers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[discord]\ntoken = \"legacy-token\"\ndefault_channel = \"123\"\n",
        )
        .unwrap();

        let config = AppConfig::load_or_default(&path).unwrap();

        assert_eq!(
            config.providers.discord.bot_token.as_deref(),
            Some("legacy-token")
        );
        assert_eq!(
            config.providers.discord.legacy_default_channel.as_deref(),
            Some("123")
        );
        assert!(config.discord.is_empty());
        assert_eq!(config.defaults.channel.as_deref(), Some("123"));
    }

    #[test]
    fn load_or_default_rejects_conflicting_legacy_and_provider_discord() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[discord]\ntoken = \"legacy-token\"\n[providers.discord]\ntoken = \"provider-token\"\n",
        )
        .unwrap();

        let error = AppConfig::load_or_default(&path).unwrap_err().to_string();

        assert!(error.contains("conflicting legacy [discord].token"));
    }

    #[test]
    fn load_or_default_parses_discord_thread_route_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[providers.discord]
token = "bot-token"

[[routes]]
event = "session.*"
sink = "discord"
thread = "123456789012345678"
"#,
        )
        .unwrap();

        let config = AppConfig::load_or_default(&path).unwrap();

        assert_eq!(
            config.routes[0].thread.as_deref(),
            Some("123456789012345678")
        );
        assert_eq!(config.routes[0].channel, None);
    }

    #[test]
    fn webhook_route_satisfies_delivery_validation_without_bot_token() {
        let config = AppConfig {
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                webhook: Some("https://discord.com/api/webhooks/123/abc".into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };

        assert!(config.validate().is_ok(), "{:?}", config.validate().err());
    }

    #[test]
    fn catch_all_webhook_with_default_channel_validates_without_bot_token() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                channel_name: None,
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "*".into(),
                webhook: Some("https://discord.com/api/webhooks/123/abc".into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn slack_webhook_route_satisfies_delivery_validation_without_bot_token() {
        let config = AppConfig {
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                slack_webhook: Some("https://hooks.slack.com/services/T/B/abc".into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };

        assert!(config.validate().is_ok());
        assert_eq!(config.webhook_route_count(), 1);
    }

    #[test]
    fn localfile_only_route_satisfies_delivery_validation_without_bot_token() {
        let config = AppConfig {
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                sink: "localfile".into(),
                local_path: Some("/tmp/clawhip/events.jsonl".into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn localfile_route_does_not_bypass_missing_token_for_discord_channel_route() {
        let config = AppConfig {
            routes: vec![
                RouteRule {
                    event: "tmux.keyword".into(),
                    sink: "localfile".into(),
                    local_path: Some("/tmp/clawhip/events.jsonl".into()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "git.commit".into(),
                    sink: "discord".into(),
                    channel: Some("ops".into()),
                    ..RouteRule::default()
                },
            ],
            ..AppConfig::default()
        };

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("missing Discord bot token"));
    }

    #[test]
    fn localfile_route_can_mix_with_discord_webhook_without_bot_token() {
        let config = AppConfig {
            routes: vec![
                RouteRule {
                    event: "tmux.keyword".into(),
                    sink: "localfile".into(),
                    local_path: Some("/tmp/clawhip/events.jsonl".into()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "git.commit".into(),
                    sink: "discord".into(),
                    webhook: Some("https://discord.com/api/webhooks/123/abc".into()),
                    ..RouteRule::default()
                },
            ],
            ..AppConfig::default()
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn discord_route_cannot_set_multiple_targets() {
        let config = AppConfig {
            providers: ProvidersConfig {
                discord: DiscordConfig {
                    bot_token: Some("token".into()),
                    legacy_default_channel: None,
                },
                slack: SlackConfig::default(),
            },
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                sink: default_sink_name(),
                channel: Some("123".into()),
                thread: Some("456".into()),
                slack_webhook: None,
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("only one Discord target"));
    }

    #[test]
    fn slack_route_cannot_set_channel() {
        let config = AppConfig {
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                sink: "slack".into(),
                channel: Some("123".into()),
                webhook: Some("https://hooks.slack.com/services/T/B/abc".into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("cannot set channel when sink = \"slack\""));
    }

    #[test]
    fn slack_route_can_use_generic_webhook_field() {
        let config = AppConfig {
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                sink: "slack".into(),
                webhook: Some("https://hooks.slack.com/services/T/B/abc".into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };

        assert!(config.validate().is_ok());
        assert_eq!(config.webhook_route_count(), 1);
    }

    #[test]
    fn setup_scaffold_adds_canonical_quickstart_route() {
        let mut config = AppConfig::default();
        config
            .scaffold_webhook_quickstart(" https://discord.com/api/webhooks/123/abc ".into())
            .unwrap();

        assert_eq!(config.routes.len(), 1);
        assert_eq!(config.routes[0].event, "*");
        assert_eq!(
            config.routes[0].webhook.as_deref(),
            Some("https://discord.com/api/webhooks/123/abc")
        );
        assert_eq!(config.routes[0].sink, "discord");
        assert_eq!(config.routes[0].channel, None);
    }

    #[test]
    fn setup_mixed_flag_edits_update_only_owned_nodes() {
        let mut config = AppConfig {
            providers: ProvidersConfig {
                discord: DiscordConfig {
                    bot_token: Some("old-token".into()),
                    legacy_default_channel: None,
                },
                slack: SlackConfig::default(),
            },
            daemon: DaemonConfig {
                base_url: "http://127.0.0.1:25294".into(),
                ..DaemonConfig::default()
            },
            defaults: DefaultsConfig {
                channel: Some("general".into()),
                channel_name: None,
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "git.commit".into(),
                channel: Some("eng".into()),
                ..RouteRule::default()
            }],
            monitors: MonitorConfig {
                github_token: Some("gh-token".into()),
                ..MonitorConfig::default()
            },
            ..AppConfig::default()
        };

        config
            .apply_setup_edits(SetupEdits {
                webhook: Some("https://discord.com/api/webhooks/123/new".into()),
                bot_token: Some("new-token".into()),
                default_channel: Some("alerts".into()),
                default_format: Some(MessageFormat::Alert),
                daemon_base_url: Some("http://127.0.0.1:9999".into()),
            })
            .unwrap();

        assert_eq!(
            config.providers.discord.bot_token.as_deref(),
            Some("new-token")
        );
        assert_eq!(config.defaults.channel.as_deref(), Some("alerts"));
        assert_eq!(config.defaults.format, MessageFormat::Alert);
        assert_eq!(config.daemon.base_url, "http://127.0.0.1:9999");
        assert_eq!(config.routes.len(), 2);
        assert_eq!(config.routes[0].event, "git.commit");
        assert_eq!(config.routes[0].channel.as_deref(), Some("eng"));
        assert_eq!(config.monitors.github_token.as_deref(), Some("gh-token"));
        assert_eq!(
            config.routes[1].webhook.as_deref(),
            Some("https://discord.com/api/webhooks/123/new")
        );
    }

    #[test]
    fn setup_non_webhook_edits_do_not_touch_routes() {
        let mut config = AppConfig {
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                webhook: Some("https://discord.com/api/webhooks/123/original".into()),
                mention: Some("<@1>".into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };

        config
            .apply_setup_edits(SetupEdits {
                bot_token: Some("discord-token".into()),
                default_channel: Some("alerts".into()),
                default_format: Some(MessageFormat::Raw),
                daemon_base_url: Some("http://127.0.0.1:4444".into()),
                ..SetupEdits::default()
            })
            .unwrap();

        assert_eq!(config.routes.len(), 1);
        assert_eq!(config.routes[0].event, "tmux.keyword");
        assert_eq!(
            config.routes[0].webhook.as_deref(),
            Some("https://discord.com/api/webhooks/123/original")
        );
        assert_eq!(config.routes[0].mention.as_deref(), Some("<@1>"));
    }

    #[test]
    fn setup_webhook_rerun_updates_only_canonical_quickstart_route() {
        let mut config = AppConfig {
            routes: vec![
                RouteRule {
                    event: "*".into(),
                    webhook: Some("https://discord.com/api/webhooks/123/old".into()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "git.commit".into(),
                    webhook: Some("https://discord.com/api/webhooks/123/other".into()),
                    mention: Some("<@1>".into()),
                    ..RouteRule::default()
                },
            ],
            ..AppConfig::default()
        };

        config
            .scaffold_webhook_quickstart("https://discord.com/api/webhooks/123/new".into())
            .unwrap();

        assert_eq!(config.routes.len(), 2);
        assert_eq!(
            config.routes[0].webhook.as_deref(),
            Some("https://discord.com/api/webhooks/123/new")
        );
        assert_eq!(
            config.routes[1].webhook.as_deref(),
            Some("https://discord.com/api/webhooks/123/other")
        );
    }

    #[test]
    fn ambiguous_quickstart_routes_fail_without_mutating_config() {
        let mut config = AppConfig {
            routes: vec![
                RouteRule {
                    event: "*".into(),
                    webhook: Some("https://discord.com/api/webhooks/123/a".into()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "*".into(),
                    webhook: Some("https://discord.com/api/webhooks/123/b".into()),
                    ..RouteRule::default()
                },
            ],
            ..AppConfig::default()
        };

        let error = config
            .scaffold_webhook_quickstart("https://discord.com/api/webhooks/123/new".into())
            .unwrap_err()
            .to_string();

        assert!(error.contains("multiple canonical quickstart routes"));
        assert_eq!(config.routes.len(), 2);
        assert_eq!(
            config.routes[0].webhook.as_deref(),
            Some("https://discord.com/api/webhooks/123/a")
        );
        assert_eq!(
            config.routes[1].webhook.as_deref(),
            Some("https://discord.com/api/webhooks/123/b")
        );
    }

    #[test]
    fn setup_edits_require_at_least_one_non_empty_value() {
        let mut config = AppConfig::default();

        let error = config
            .apply_setup_edits(SetupEdits {
                webhook: Some("   ".into()),
                bot_token: Some(" ".into()),
                default_channel: Some(" ".into()),
                daemon_base_url: Some(" ".into()),
                ..SetupEdits::default()
            })
            .unwrap_err()
            .to_string();

        assert!(error.contains("at least one non-empty setup flag"));
    }

    #[test]
    fn config_editor_menu_matches_bounded_preset_contract() {
        assert_eq!(
            CONFIG_EDITOR_MENU_ITEMS,
            [
                "Set Discord bot token",
                "Set daemon base URL",
                "Set default channel",
                "Set default format",
                "Set Discord webhook quickstart route",
                "Save and exit",
                "Exit without saving",
                "Print manual config template hint",
            ]
        );
    }

    #[test]
    fn tmux_session_monitor_defaults_keyword_window_to_thirty_seconds() {
        let session = TmuxSessionMonitor::default();
        assert_eq!(session.keyword_window_secs, 30);
    }

    #[test]
    fn dispatch_config_defaults_ci_batch_window_to_thirty_seconds() {
        let config = AppConfig::default();
        assert_eq!(config.dispatch.ci_batch_window_secs, 30);
    }

    #[test]
    fn dispatch_config_defaults_routine_batch_window_to_five_seconds() {
        let config = AppConfig::default();
        assert_eq!(config.dispatch.routine_batch_window_secs, 5);
        assert_eq!(
            config.dispatch.routine_batch_window(),
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn cron_config_defaults_are_backward_compatible() {
        let config = AppConfig::default();
        assert_eq!(config.cron.poll_interval_secs, 30);
        assert!(config.cron.jobs.is_empty());
    }

    #[test]
    fn load_or_default_parses_dispatch_ci_batch_window_secs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[providers.discord]\ntoken = \"abc\"\n[dispatch]\nci_batch_window_secs = 90\n",
        )
        .unwrap();

        let config = AppConfig::load_or_default(&path).unwrap();

        assert_eq!(config.dispatch.ci_batch_window_secs, 90);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn load_or_default_parses_dispatch_routine_batch_window_secs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[providers.discord]\ntoken = \"abc\"\n[dispatch]\nroutine_batch_window_secs = 9\n",
        )
        .unwrap();

        let config = AppConfig::load_or_default(&path).unwrap();

        assert_eq!(config.dispatch.routine_batch_window_secs, 9);
        assert_eq!(
            config.dispatch.routine_batch_window(),
            Some(Duration::from_secs(9))
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn load_or_default_defaults_dispatch_ci_batch_window_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[providers.discord]\ntoken = \"abc\"\n").unwrap();

        let config = AppConfig::load_or_default(&path).unwrap();

        assert_eq!(config.dispatch.ci_batch_window_secs, 30);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn load_or_default_defaults_routine_batch_window_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[providers.discord]\ntoken = \"abc\"\n").unwrap();

        let config = AppConfig::load_or_default(&path).unwrap();

        assert_eq!(config.dispatch.routine_batch_window_secs, 5);
        assert_eq!(
            config.dispatch.routine_batch_window(),
            Some(Duration::from_secs(5))
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn load_or_default_preserves_zero_dispatch_ci_batch_window_secs_until_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[providers.discord]\ntoken = \"abc\"\n[dispatch]\nci_batch_window_secs = 0\n",
        )
        .unwrap();

        let config = AppConfig::load_or_default(&path).unwrap();
        assert_eq!(config.dispatch.ci_batch_window_secs, 0);
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("dispatch.ci_batch_window_secs must be at least 1"));
    }

    #[test]
    fn load_or_default_allows_zero_dispatch_routine_batch_window_secs_to_disable_batching() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[providers.discord]\ntoken = \"abc\"\n[dispatch]\nroutine_batch_window_secs = 0\n",
        )
        .unwrap();

        let config = AppConfig::load_or_default(&path).unwrap();
        assert_eq!(config.dispatch.routine_batch_window_secs, 0);
        assert_eq!(config.dispatch.routine_batch_window(), None);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn load_or_default_parses_cron_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"[providers.discord]
token = "abc"

[cron]
poll_interval_secs = 15

[[cron.jobs]]
id = "dev-followup"
schedule = "*/30 * * * *"
channel = "ops"
mention = " <@1> "
kind = "custom-message"
message = " ping "
"#,
        )
        .unwrap();

        let config = AppConfig::load_or_default(&path).unwrap();

        assert_eq!(config.cron.poll_interval_secs, 15);
        assert_eq!(config.cron.jobs.len(), 1);
        let job = &config.cron.jobs[0];
        assert_eq!(job.id, "dev-followup");
        assert_eq!(job.schedule, "*/30 * * * *");
        assert_eq!(job.channel.as_deref(), Some("ops"));
        assert_eq!(job.mention.as_deref(), Some("<@1>"));
        assert_eq!(job.timezone, "UTC");
        assert_eq!(job.zero_backlog_suppression_ttl_secs, 60 * 60);
        match &job.kind {
            CronJobKind::CustomMessage { message } => assert_eq!(message, "ping"),
        }
        assert!(config.validate().is_ok());
    }

    #[test]
    fn cron_validation_rejects_duplicate_ids() {
        let config = AppConfig {
            providers: ProvidersConfig {
                discord: DiscordConfig {
                    bot_token: Some("token".into()),
                    legacy_default_channel: None,
                },
                slack: SlackConfig::default(),
            },
            cron: CronConfig {
                poll_interval_secs: 30,
                jobs: vec![
                    CronJob {
                        id: "dup".into(),
                        schedule: "*/5 * * * *".into(),
                        timezone: "UTC".into(),
                        enabled: true,
                        channel: Some("ops".into()),
                        mention: None,
                        format: None,
                        state_file: None,
                        zero_backlog_suppression_ttl_secs:
                            default_zero_backlog_suppression_ttl_secs(),
                        kind: CronJobKind::CustomMessage {
                            message: "first".into(),
                        },
                    },
                    CronJob {
                        id: "dup".into(),
                        schedule: "0 * * * *".into(),
                        timezone: "UTC".into(),
                        enabled: true,
                        channel: Some("ops".into()),
                        mention: None,
                        format: None,
                        state_file: None,
                        zero_backlog_suppression_ttl_secs:
                            default_zero_backlog_suppression_ttl_secs(),
                        kind: CronJobKind::CustomMessage {
                            message: "second".into(),
                        },
                    },
                ],
            },
            ..AppConfig::default()
        };

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("duplicate cron job id 'dup'"));
    }

    #[test]
    fn workspace_monitor_defaults_are_backward_compatible() {
        let config: AppConfig = toml::from_str(
            "
[providers.discord]
token = 'discord-token'

[[monitors.workspace]]
path = '/tmp/repo'
",
        )
        .unwrap();

        assert_eq!(config.monitors.workspace.len(), 1);
        let monitor = &config.monitors.workspace[0];
        assert_eq!(monitor.watch_dirs, default_workspace_watch_dirs());
        assert_eq!(monitor.debounce_ms, default_workspace_debounce_ms());
        assert_eq!(monitor.poll_interval_secs, None);
        assert!(!monitor.discover_worktrees);
    }

    #[test]
    fn normalize_trims_workspace_monitor_fields() {
        let mut config = AppConfig::default();
        config.monitors.workspace.push(WorkspaceMonitor {
            path: " /tmp/repo ".into(),
            watch_dirs: vec![" .omx/state ".into(), "".into(), " .omc/state ".into()],
            discover_worktrees: true,
            channel: Some(" 123 ".into()),
            mention: Some(" <@1> ".into()),
            format: Some(MessageFormat::Compact),
            events: vec!["workspace.*".into()],
            poll_interval_secs: Some(5),
            debounce_ms: 2000,
        });

        config.normalize();
        let monitor = &config.monitors.workspace[0];
        assert_eq!(monitor.path, "/tmp/repo");
        assert_eq!(monitor.watch_dirs, vec![".omx/state", ".omc/state"]);
        assert_eq!(monitor.channel.as_deref(), Some("123"));
        assert_eq!(monitor.mention.as_deref(), Some("<@1>"));
    }

    #[test]
    fn workspace_monitor_config_parses_and_normalizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"[providers.discord]
token = "abc"

[[monitors.workspace]]
path = " {} "
watch_dirs = [" .omx/state ", " .omc/state "]
channel = " ops "
mention = " <@1> "
discover_worktrees = true
events = [" workspace.skill.* "]
debounce_ms = 1500
poll_interval_secs = 9
"#,
                dir.path().display()
            ),
        )
        .unwrap();

        let config = AppConfig::load_or_default(&path).unwrap();
        let monitor = &config.monitors.workspace[0];
        assert_eq!(monitor.path, dir.path().display().to_string());
        assert_eq!(monitor.watch_dirs, vec![".omx/state", ".omc/state"]);
        assert_eq!(monitor.channel.as_deref(), Some("ops"));
        assert_eq!(monitor.mention.as_deref(), Some("<@1>"));
        assert!(monitor.discover_worktrees);
        assert_eq!(monitor.events, vec!["workspace.skill.*"]);
        assert_eq!(monitor.debounce_ms, 1500);
        assert_eq!(monitor.poll_interval_secs, Some(9));
    }

    #[test]
    fn config_without_workspace_monitor_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[providers.discord]\ntoken = \"abc\"\n").unwrap();

        let config = AppConfig::load_or_default(&path).unwrap();
        assert!(config.monitors.workspace.is_empty());
        assert!(config.validate().is_ok());
    }
    #[test]
    fn default_discord_watch_config_is_empty_and_omitted_from_pretty_toml() {
        let config = AppConfig::default();

        assert!(config.discord_watch.is_empty());
        let toml = config.to_pretty_toml().expect("serialize default config");
        assert!(
            !toml.contains("[discord_watch]"),
            "default local-only watch config should not change generated config shape"
        );
        let round_tripped: AppConfig = toml::from_str(&toml).expect("round-trip default config");
        assert!(round_tripped.discord_watch.is_empty());
    }

    #[test]
    fn discord_watch_defaults_are_backward_compatible_and_local_only() {
        let config: AppConfig =
            toml::from_str("[[routes]]\nevent = \"custom\"\nsink = \"localfile\"\nlocal_path = \"/tmp/clawhip/events.jsonl\"\n").expect("old config parses");
        assert!(!config.discord_watch.enabled);
        assert!(config.discord_watch.watched_channels.is_empty());
        assert!(config.discord_watch.gaebal_gajae_user_id.is_empty());
        assert!(config.discord_watch.nudge_target_channel_id.is_none());
        assert!(
            config
                .discord_watch
                .banned_channel_names
                .contains(&"omo".to_string())
        );
        assert!(
            config
                .discord_watch
                .banned_channel_names
                .contains(&"omo-help".to_string())
        );
        assert_eq!(config.discord_watch.pending_mentions_threshold, 5);
        assert_eq!(config.discord_watch.direct_mention_persist_ms, 180_000);
        assert_eq!(config.discord_watch.channel_message_threshold, 100);
        assert!(config.validate().is_ok(), "{:?}", config.validate().err());
    }

    #[test]
    fn discord_watch_config_parses_without_discord_delivery_requirements() {
        let config: AppConfig = toml::from_str(
            r#"
[discord_watch]
enabled = true
gaebal_gajae_user_id = "fixture-gaebal"
owner_user_ids = ["fixture-owner"]
channel_cooldown_ms = 60000
global_cooldown_ms = 60000
[[discord_watch.watched_channels]]
id = "fixture-general"
name = "general"
"#,
        )
        .expect("discord_watch config");
        assert!(config.discord_watch.enabled);
        assert_eq!(config.discord_watch.owner_user_ids, vec!["fixture-owner"]);
        assert!(
            config.validate().is_ok(),
            "local-only watch must not require a bot token or route"
        );
    }

    #[test]
    fn discord_watch_custom_tuning_is_preserved_in_pretty_toml() {
        let mut config = AppConfig::default();
        config.discord_watch.pending_mentions_threshold = 7;
        config.discord_watch.doctrine_template = "Sweep <#{channel_id}>".into();

        let toml = config.to_pretty_toml().expect("serialize config");

        assert!(toml.contains("[discord_watch]"));
        assert!(toml.contains("pending_mentions_threshold = 7"));
        assert!(toml.contains("doctrine_template = \"Sweep <#{channel_id}>\""));
    }

    #[test]
    fn repo_channel_route_binding_keeps_legacy_bind_route_only() {
        let mut config = AppConfig::default();

        config
            .apply_repo_channel_route_binding("owner/repo", "123", Some("dev"))
            .unwrap();
        config
            .apply_repo_channel_route_binding("owner/repo", "123", Some("dev"))
            .unwrap();

        assert_eq!(config.routes.len(), 1);
        assert_eq!(config.routes[0].channel.as_deref(), Some("123"));
        assert_eq!(config.routes[0].channel_name.as_deref(), Some("dev"));
        assert!(config.monitors.git.repos.is_empty());
    }

    #[test]
    fn repo_channel_binding_allows_branch_specific_manual_route() {
        let mut filter = BTreeMap::new();
        filter.insert("repo".to_string(), "owner/repo".to_string());
        filter.insert("branch".to_string(), "main".to_string());
        let mut config = AppConfig {
            routes: vec![RouteRule {
                event: "git.push".into(),
                filter,
                channel: Some("manual".into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };

        config
            .apply_repo_channel_route_binding("owner/repo", "123", Some("dev"))
            .unwrap();

        assert_eq!(config.routes.len(), 2);
        assert!(config.monitors.git.repos.is_empty());
    }

    #[test]
    fn repo_channel_binding_create_is_idempotent_and_adds_monitor() {
        let mut config = AppConfig::default();

        config
            .apply_repo_channel_binding("owner/repo", "123", Some("#dev"), "/work/repo")
            .unwrap();
        config
            .apply_repo_channel_binding("owner/repo", "123", Some("dev"), "/work/repo")
            .unwrap();

        assert_eq!(config.routes.len(), 1);
        assert_eq!(
            config.routes[0].filter.get("repo").map(String::as_str),
            Some("owner/repo")
        );
        assert_eq!(config.routes[0].channel.as_deref(), Some("123"));
        assert_eq!(config.routes[0].channel_name.as_deref(), Some("dev"));
        assert_eq!(config.monitors.git.repos.len(), 1);
        let monitor = &config.monitors.git.repos[0];
        assert_eq!(monitor.path, "/work/repo");
        assert_eq!(monitor.name.as_deref(), Some("repo"));
        assert_eq!(monitor.github_repo.as_deref(), Some("owner/repo"));
        assert_eq!(monitor.channel.as_deref(), Some("123"));
    }

    #[test]
    fn repo_channel_binding_updates_setup_owned_monitor_and_preserves_options() {
        let mut filter = BTreeMap::new();
        filter.insert("repo".to_string(), "owner/repo".to_string());
        let mut config = AppConfig {
            routes: vec![RouteRule {
                event: "*".into(),
                filter,
                channel: Some("old".into()),
                channel_name: Some("dev".into()),
                ..RouteRule::default()
            }],
            monitors: MonitorConfig {
                git: GitMonitorConfig {
                    repos: vec![GitRepoMonitor {
                        path: "/old/repo".into(),
                        name: Some("repo".into()),
                        remote: "upstream".into(),
                        github_repo: Some("owner/repo".into()),
                        emit_commits: false,
                        emit_branch_changes: false,
                        emit_issue_opened: false,
                        emit_pr_status: true,
                        channel: Some("old".into()),
                        channel_name: Some("#DEV".into()),
                        setup_owned: true,
                        mention: Some("<@1>".into()),
                        format: Some(MessageFormat::Raw),
                    }],
                },
                ..MonitorConfig::default()
            },
            ..AppConfig::default()
        };

        config
            .apply_repo_channel_binding("owner/repo", "new", Some("dev"), "/new/repo")
            .unwrap();

        assert_eq!(config.routes[0].channel.as_deref(), Some("new"));
        let monitor = &config.monitors.git.repos[0];
        assert_eq!(monitor.path, "/new/repo");
        assert_eq!(monitor.remote, "upstream");
        assert!(!monitor.emit_commits);
        assert!(!monitor.emit_branch_changes);
        assert!(!monitor.emit_issue_opened);
        assert!(monitor.emit_pr_status);
        assert_eq!(monitor.mention.as_deref(), Some("<@1>"));
        assert_eq!(monitor.format, Some(MessageFormat::Raw));
        assert_eq!(monitor.channel.as_deref(), Some("new"));
    }

    #[test]
    fn repo_channel_binding_manual_monitor_conflict_does_not_mutate() {
        let mut config = AppConfig {
            monitors: MonitorConfig {
                git: GitMonitorConfig {
                    repos: vec![GitRepoMonitor {
                        path: "/manual".into(),
                        name: Some("manual".into()),
                        channel: Some("123".into()),
                        channel_name: None,
                        ..GitRepoMonitor::default()
                    }],
                },
                ..MonitorConfig::default()
            },
            ..AppConfig::default()
        };
        let before = config.to_pretty_toml().unwrap();

        let error = config
            .apply_repo_channel_binding("owner/repo", "123", Some("dev"), "/work/repo")
            .unwrap_err()
            .to_string();

        assert!(error.contains("manual_monitor_conflict"));
        assert_eq!(config.to_pretty_toml().unwrap(), before);
    }

    #[test]
    fn save_with_backup_skips_new_and_identical_then_backs_up_changed_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = AppConfig::default();

        config.save_with_backup(&path).unwrap();
        assert!(!dir.path().join(".clawhip-config-backups").exists());
        config.save_with_backup(&path).unwrap();
        assert!(!dir.path().join(".clawhip-config-backups").exists());

        config.defaults.channel = Some("alerts".into());
        config.save_with_backup(&path).unwrap();

        let backups = fs::read_dir(dir.path().join(".clawhip-config-backups"))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(backups.len(), 1);
        let backup = fs::read_to_string(backups[0].path()).unwrap();
        assert!(!backup.contains("alerts"));
    }

    #[test]
    fn backup_filename_parsers_accept_evidenced_families_and_reject_near_misses() {
        let expected = parse_compact_backup_timestamp_seconds("20260708T072000Z").unwrap();
        assert_eq!(
            parse_root_legacy_backup_name(
                "config.toml.bak-gajae-runtime-rename-20260708T072000Z",
                "config.toml"
            ),
            Some(expected)
        );
        assert!(
            parse_root_legacy_backup_name(
                "config.toml.bak-release-radar-20260708T0720Z",
                "config.toml"
            )
            .is_some()
        );
        assert!(
            parse_root_legacy_backup_name("config.toml.bak-gp-mention-20260708", "config.toml")
                .is_some()
        );
        assert!(
            parse_root_legacy_backup_name("config.toml.bak-20260708T072000Z", "config.toml")
                .is_some()
        );
        assert!(
            parse_duplicated_legacy_backup_name(
                "config.config.toml.bak-2026-04-10-0206",
                "config.toml"
            )
            .is_some()
        );
        assert!(
            parse_root_legacy_backup_name(
                "custom.name.toml.bak-label-20260708T072000Z",
                "custom.name.toml"
            )
            .is_some()
        );
        assert!(
            parse_duplicated_legacy_backup_name(
                "config.custom.name.toml.bak-2026-04-10-0206",
                "custom.name.toml"
            )
            .is_some()
        );

        for name in [
            "config.toml.bak-label_with_underscore-20260708T072000Z",
            "config.toml.bak-label-20260708T072000",
            "config.toml.bak-20260708T07200Z",
            "config.toml.bak-20260708-extra",
            "config.config.toml.bak-2026-04-10-0206-extra",
            "other.toml.bak-20260708T072000Z",
        ] {
            assert!(
                parse_root_legacy_backup_name(name, "config.toml").is_none()
                    && parse_duplicated_legacy_backup_name(name, "config.toml").is_none(),
                "unexpected recognized backup: {name}"
            );
        }
        assert!(
            parse_managed_backup_name("config.toml.20260708T072000Z.abcdef12.bak", "config.toml")
                .is_some()
        );
        assert!(
            parse_managed_backup_name("config.toml.20260708T072000Z.abcdef1g.bak", "config.toml")
                .is_none()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn backup_retention_combines_legacy_and_managed_with_deterministic_ties() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let backups_dir = parent.join(".clawhip-config-backups");
        fs::create_dir(&backups_dir).unwrap();
        let managed_dir = validate_managed_backup_dir(parent, false).unwrap().unwrap();
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let tie = now - TimeDuration::days(50);
        let root_tie = format!("config.toml.bak-{}", utc_backup_timestamp(tie).unwrap());
        let managed_tie = format!(
            "config.toml.{}.abcdef12.bak",
            utc_backup_timestamp(tie).unwrap()
        );
        fs::write(parent.join(&root_tie), "root").unwrap();
        fs::write(backups_dir.join(&managed_tie), "managed").unwrap();
        let mut later_names = Vec::new();
        for index in 0..9 {
            let created_at = tie + TimeDuration::seconds(index + 1);
            let name = format!(
                "config.toml.{}.{:08x}.bak",
                utc_backup_timestamp(created_at).unwrap(),
                index
            );
            fs::write(backups_dir.join(&name), "later").unwrap();
            later_names.push(name);
        }

        let mut before_delete = |_: &BackupCandidate| Ok(());
        cleanup_config_backups_with(
            parent,
            None,
            "config.toml",
            Some(&managed_dir),
            &[],
            now,
            &mut before_delete,
        )
        .unwrap();
        assert!(!parent.join(&root_tie).exists());
        assert!(backups_dir.join(&managed_tie).exists());
        assert!(
            later_names
                .iter()
                .all(|name| backups_dir.join(name).exists())
        );

        cleanup_config_backups_with(
            parent,
            None,
            "config.toml",
            Some(&managed_dir),
            &[],
            now,
            &mut before_delete,
        )
        .unwrap();
        assert!(backups_dir.join(&managed_tie).exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn backup_retention_deletes_exact_cutoff_but_keeps_just_newer() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let exact_cutoff = format!(
            "config.toml.bak-exact-cutoff-{}",
            utc_backup_timestamp(now - TimeDuration::days(30)).unwrap()
        );
        let just_newer = format!(
            "config.toml.bak-just-newer-{}",
            utc_backup_timestamp(now - TimeDuration::days(30) + TimeDuration::seconds(1)).unwrap()
        );
        fs::write(parent.join(&exact_cutoff), "cutoff").unwrap();
        fs::write(parent.join(&just_newer), "recent").unwrap();
        for index in 0..10 {
            let name = format!(
                "config.toml.bak-later-{index}-{}",
                utc_backup_timestamp(now - TimeDuration::days(20 - index)).unwrap()
            );
            fs::write(parent.join(name), "later").unwrap();
        }
        let mut before_delete = |_: &BackupCandidate| Ok(());
        cleanup_config_backups_with(
            parent,
            None,
            "config.toml",
            None,
            &[],
            now,
            &mut before_delete,
        )
        .unwrap();
        assert!(!parent.join(exact_cutoff).exists());
        assert!(parent.join(just_newer).exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn backup_retention_deletes_stale_duplicated_legacy_family() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let duplicated = parent.join("config.config.toml.bak-2000-01-01-0000");
        fs::write(&duplicated, "duplicated").unwrap();
        for day in 2..=11 {
            fs::write(
                parent.join(format!("config.toml.bak-200001{day:02}")),
                "later",
            )
            .unwrap();
        }
        let mut no_delete = |_: &BackupCandidate| Ok(());

        cleanup_config_backups_with(
            parent,
            None,
            "config.toml",
            None,
            &[],
            OffsetDateTime::now_utc(),
            &mut no_delete,
        )
        .unwrap();

        assert!(!duplicated.exists());
    }

    #[cfg(not(unix))]
    #[test]
    fn backup_initial_save_succeeds_without_identity_cleanup_proof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        AppConfig::default().save_with_backup(&path).unwrap();
        assert!(path.is_file());
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    #[test]
    fn backup_cleanup_preserves_selected_candidate_without_supported_descriptor_proof() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        for day in 1..=11 {
            fs::write(
                parent.join(format!("config.toml.bak-200001{day:02}")),
                "candidate",
            )
            .unwrap();
        }
        let oldest = parent.join("config.toml.bak-20000101");
        let mut before_delete = |_: &BackupCandidate| Ok(());

        cleanup_config_backups_with(
            parent,
            None,
            "config.toml",
            None,
            &[],
            OffsetDateTime::now_utc(),
            &mut before_delete,
        )
        .unwrap();

        assert_eq!(fs::read_to_string(oldest).unwrap(), "candidate");
    }
    #[cfg(unix)]
    #[test]
    fn backup_cleanup_preserves_unknown_symlink_directory_and_hardlink_entries() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let active = parent.join("config.toml");
        fs::write(&active, "active").unwrap();
        let unknown = parent.join("config.toml.bak-not-a-date");
        fs::write(&unknown, "unknown").unwrap();
        let directory = parent.join("config.toml.bak-20200101");
        fs::create_dir(&directory).unwrap();
        let outside = parent.join("outside");
        fs::write(&outside, "outside").unwrap();
        let symlink_path = parent.join("config.toml.bak-20190101");
        symlink(&outside, &symlink_path).unwrap();
        let hardlink_path = parent.join("config.toml.bak-20180101");
        fs::hard_link(&active, &hardlink_path).unwrap();

        let protected = current_active_config_identity(&active)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>();
        let mut before_delete = |_: &BackupCandidate| Ok(());
        cleanup_config_backups_with(
            parent,
            None,
            "config.toml",
            None,
            &protected,
            OffsetDateTime::now_utc(),
            &mut before_delete,
        )
        .unwrap();

        assert!(unknown.exists());
        assert!(directory.is_dir());
        assert!(
            symlink_path
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(hardlink_path.exists());
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside");
    }

    #[cfg(unix)]
    #[test]
    fn save_with_backup_rejects_active_and_managed_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.toml");
        fs::write(&outside, "outside").unwrap();
        let active = dir.path().join("config.toml");
        symlink(&outside, &active).unwrap();
        let error = AppConfig::default()
            .save_with_backup(&active)
            .unwrap_err()
            .to_string();
        assert!(error.contains("regular non-symlink"));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");

        fs::remove_file(&active).unwrap();
        fs::create_dir(&active).unwrap();
        let directory_error = AppConfig::default()
            .save_with_backup(&active)
            .unwrap_err()
            .to_string();
        assert!(directory_error.contains("regular non-symlink"));
        fs::remove_dir(&active).unwrap();
        AppConfig::default().save_with_backup(&active).unwrap();
        let outside_dir = dir.path().join("outside-backups");
        fs::create_dir(&outside_dir).unwrap();
        let managed_path = dir.path().join(".clawhip-config-backups");
        symlink(&outside_dir, &managed_path).unwrap();
        let mut changed = AppConfig::default();
        changed.defaults.channel = Some("alerts".into());
        let error = changed.save_with_backup(&active).unwrap_err().to_string();
        assert!(error.contains("regular directory"));
        assert!(fs::read_dir(outside_dir).unwrap().next().is_none());
        assert!(!fs::read_to_string(&active).unwrap().contains("alerts"));

        fs::remove_file(&managed_path).unwrap();
        fs::write(&managed_path, "not a directory").unwrap();
        let error = changed.save_with_backup(&active).unwrap_err().to_string();
        assert!(error.contains("regular directory"));
        assert!(!fs::read_to_string(&active).unwrap().contains("alerts"));

        let outside_parent = dir.path().join("outside-parent");
        fs::create_dir(&outside_parent).unwrap();
        let linked_parent = dir.path().join("linked-parent");
        symlink(&outside_parent, &linked_parent).unwrap();
        let linked_config = linked_parent.join("config.toml");
        let error = AppConfig::default()
            .save_with_backup(&linked_config)
            .unwrap_err()
            .to_string();
        assert!(error.contains("config parent path must be a regular directory"));
        assert!(fs::read_dir(outside_parent).unwrap().next().is_none());
    }

    #[test]
    fn save_with_backup_collision_is_precommit_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let original = AppConfig::default();
        let mut no_delete = |_: &BackupCandidate| Ok(());
        original
            .save_with_backup_at(&path, now, &mut no_delete)
            .unwrap();
        let old_bytes = fs::read(&path).unwrap();
        let backups_dir = dir.path().join(".clawhip-config-backups");
        fs::create_dir(&backups_dir).unwrap();
        let collision = backups_dir.join(format!(
            "config.toml.{}.{}.bak",
            utc_backup_timestamp(now).unwrap(),
            sha256_first8(&old_bytes)
        ));
        fs::write(&collision, "sentinel").unwrap();
        let mut changed = AppConfig::default();
        changed.defaults.channel = Some("alerts".into());

        let error = changed
            .save_with_backup_at(&path, now, &mut no_delete)
            .unwrap_err()
            .to_string();
        assert!(error.contains("config backup collision"));
        assert!(error.contains("config was not saved"));
        assert_eq!(fs::read_to_string(collision).unwrap(), "sentinel");
        assert_eq!(fs::read(&path).unwrap(), old_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn save_with_backup_reports_postcommit_cleanup_failure_and_retries_on_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let original = AppConfig::default();
        let mut no_delete = |_: &BackupCandidate| Ok(());
        original
            .save_with_backup_at(&path, now, &mut no_delete)
            .unwrap();
        fs::write(dir.path().join("config.toml.bak-20100101"), "stale root").unwrap();
        for index in 0..10 {
            let name = format!("config.toml.bak-201001{:02}", index + 2);
            fs::write(dir.path().join(name), "retained").unwrap();
        }
        let mut changed = AppConfig::default();
        changed.defaults.channel = Some("alerts".into());
        let mut fail_once =
            |_: &BackupCandidate| -> Result<()> { Err("injected cleanup failure".into()) };

        let error = changed
            .save_with_backup_at(&path, now, &mut fail_once)
            .unwrap_err()
            .to_string();
        assert!(error.contains("config was saved; backup retention cleanup remains incomplete"));
        assert!(fs::read_to_string(&path).unwrap().contains("alerts"));

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            changed
                .save_with_backup_at(&path, now, &mut no_delete)
                .unwrap();
            assert!(!dir.path().join("config.toml.bak-20100101").exists());
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            changed
                .save_with_backup_at(&path, now, &mut no_delete)
                .unwrap();
            assert!(dir.path().join("config.toml.bak-20100101").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn backup_cleanup_revalidates_managed_directory_before_unlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        let backups_dir = parent.join(".clawhip-config-backups");
        fs::create_dir(&backups_dir).unwrap();
        let managed_dir = validate_managed_backup_dir(parent, false).unwrap().unwrap();
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        for index in 0..11 {
            let name = format!(
                "config.toml.{}.{:08x}.bak",
                utc_backup_timestamp(now - TimeDuration::days(60 - index)).unwrap(),
                index
            );
            fs::write(backups_dir.join(name), "managed").unwrap();
        }
        let outside = parent.join("outside");
        fs::create_dir(&outside).unwrap();
        let outside_name = format!(
            "config.toml.{}.abcdef12.bak",
            utc_backup_timestamp(now - TimeDuration::days(90)).unwrap()
        );
        fs::write(outside.join(&outside_name), "outside").unwrap();
        let moved = parent.join("moved-backups");
        let mut swapped = false;
        let mut swap_manager = |_: &BackupCandidate| -> Result<()> {
            if !swapped {
                fs::rename(&backups_dir, &moved)?;
                symlink(&outside, &backups_dir)?;
                swapped = true;
            }
            Ok(())
        };

        let error = cleanup_config_backups_with(
            parent,
            None,
            "config.toml",
            Some(&managed_dir),
            &[],
            now,
            &mut swap_manager,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("managed config backup directory changed"));
        assert_eq!(
            fs::read_to_string(outside.join(outside_name)).unwrap(),
            "outside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_with_backup_preserves_old_active_hardlink_legacy_alias() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let original = AppConfig::default();
        let mut no_delete = |_: &BackupCandidate| Ok(());
        original
            .save_with_backup_at(&path, now, &mut no_delete)
            .unwrap();
        let old_bytes = fs::read(&path).unwrap();
        let alias = dir.path().join("config.toml.bak-20000101");
        fs::hard_link(&path, &alias).unwrap();
        for day in 2..=11 {
            fs::write(
                dir.path().join(format!("config.toml.bak-200001{day:02}")),
                "later",
            )
            .unwrap();
        }
        let mut changed = AppConfig::default();
        changed.defaults.channel = Some("alerts".into());

        changed
            .save_with_backup_at(&path, now, &mut no_delete)
            .unwrap();

        assert_eq!(fs::read(alias).unwrap(), old_bytes);
        assert!(fs::read_to_string(path).unwrap().contains("alerts"));
    }

    #[cfg(unix)]
    #[test]
    fn save_with_backup_symlink_collision_does_not_follow_outside_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let original = AppConfig::default();
        let mut no_delete = |_: &BackupCandidate| Ok(());
        original
            .save_with_backup_at(&path, now, &mut no_delete)
            .unwrap();
        let old_bytes = fs::read(&path).unwrap();
        let backups_dir = dir.path().join(".clawhip-config-backups");
        fs::create_dir(&backups_dir).unwrap();
        let outside = dir.path().join("outside-backup");
        fs::write(&outside, "outside").unwrap();
        let collision = backups_dir.join(format!(
            "config.toml.{}.{}.bak",
            utc_backup_timestamp(now).unwrap(),
            sha256_first8(&old_bytes)
        ));
        symlink(&outside, &collision).unwrap();
        let mut changed = AppConfig::default();
        changed.defaults.channel = Some("alerts".into());

        let error = changed
            .save_with_backup_at(&path, now, &mut no_delete)
            .unwrap_err()
            .to_string();
        assert!(error.contains("config backup collision"));
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside");
        assert_eq!(fs::read(path).unwrap(), old_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn backup_cleanup_skips_non_utf8_names() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().unwrap();
        let mut bytes = b"config.toml.bak-label-".to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(b"-20100101");
        let path = dir.path().join(OsString::from_vec(bytes));
        fs::write(&path, "unknown").unwrap();
        let mut no_delete = |_: &BackupCandidate| Ok(());

        cleanup_config_backups_with(
            dir.path(),
            None,
            "config.toml",
            None,
            &[],
            OffsetDateTime::now_utc(),
            &mut no_delete,
        )
        .unwrap();

        assert!(fs::symlink_metadata(path).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn save_with_backup_noop_cleanup_failure_reports_current_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let config = AppConfig::default();
        let mut no_delete = |_: &BackupCandidate| Ok(());
        config
            .save_with_backup_at(&path, now, &mut no_delete)
            .unwrap();
        let original = fs::read(&path).unwrap();
        for day in 1..=11 {
            fs::write(
                dir.path().join(format!("config.toml.bak-200001{day:02}")),
                "stale",
            )
            .unwrap();
        }
        let mut fail = |_: &BackupCandidate| -> Result<()> { Err("injected no-op failure".into()) };

        let error = config
            .save_with_backup_at(&path, now, &mut fail)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(
                "config was already current; backup retention cleanup remains incomplete"
            )
        );
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn candidate_path_open_errors_preserve_only_disappearance() {
        assert_eq!(
            classify_candidate_path_open_error(io::Error::from_raw_os_error(libc::ENOENT)).unwrap(),
            CandidateDeletionOutcome::Disappeared
        );

        for code in [
            libc::EINTR,
            libc::EACCES,
            libc::EBADF,
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOMEM,
            libc::EIO,
            libc::ESTALE,
            libc::EINVAL,
        ] {
            let error =
                classify_candidate_path_open_error(io::Error::from_raw_os_error(code)).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(code));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn candidate_readability_errors_preserve_only_attributable_denials() {
        assert!(matches!(
            classify_candidate_readability_error(io::Error::from_raw_os_error(libc::EACCES))
                .unwrap(),
            CandidateReadability::Preserved(CandidatePreserveReason::Unreadable)
        ));
        for code in [
            libc::ENOSYS,
            libc::EINVAL,
            libc::EINTR,
            libc::EPERM,
            libc::EBADF,
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOMEM,
            libc::EIO,
            libc::ESTALE,
        ] {
            let error = classify_candidate_readability_error(io::Error::from_raw_os_error(code))
                .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(code));
        }

        for code in [libc::EACCES, libc::EPERM] {
            assert!(matches!(
                classify_proc_fd_reopen_error(io::Error::from_raw_os_error(code)).unwrap(),
                CandidateReadability::Preserved(CandidatePreserveReason::Unreadable)
            ));
        }
        assert!(matches!(
            classify_proc_fd_reopen_error(io::Error::from_raw_os_error(libc::EAGAIN)).unwrap(),
            CandidateReadability::Preserved(CandidatePreserveReason::ReadLeaseContended)
        ));
        for code in [
            libc::ENOENT,
            libc::EINVAL,
            libc::EINTR,
            libc::EBADF,
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOMEM,
            libc::EIO,
            libc::ESTALE,
        ] {
            let error =
                classify_proc_fd_reopen_error(io::Error::from_raw_os_error(code)).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(code));
        }

        for code in [libc::ENOENT, libc::ENOTDIR, libc::EACCES, libc::EPERM] {
            assert!(matches!(
                classify_proc_fd_dir_error(io::Error::from_raw_os_error(code)).unwrap(),
                CandidateReadability::Preserved(
                    CandidatePreserveReason::CompatibilityProbeUnavailable
                )
            ));
        }
        for code in [
            libc::EINVAL,
            libc::EINTR,
            libc::EBADF,
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOMEM,
            libc::EIO,
            libc::ESTALE,
        ] {
            let error = classify_proc_fd_dir_error(io::Error::from_raw_os_error(code)).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(code));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn faccessat2_compatibility_fallback_deletes_readable_and_preserves_unsafe_candidates() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};

        const CHILD_ENV: &str = "CLAWHIP_TEST_FACCESSAT2_COMPAT_CHILD";
        const CHILD_OK: &str = "CLAWHIP_FACCESSAT2_COMPAT_CHILD_OK";
        const TEST_NAME: &str = "config::tests::faccessat2_compatibility_fallback_deletes_readable_and_preserves_unsafe_candidates";

        if std::env::var_os(CHILD_ENV).is_some() {
            FORCE_FACCESSAT2_ENOSYS.with(|force| force.set(true));
            let root = tempfile::tempdir().unwrap();
            let parent = root.path();
            let parent_dir = validate_config_parent_dir(parent, false).unwrap();
            let outside = parent.join("outside-sentinel");
            fs::write(&outside, "outside").unwrap();
            let make_candidate = |path: &Path| {
                let metadata = fs::symlink_metadata(path).unwrap();
                BackupCandidate {
                    origin: BackupOrigin::RootLegacy,
                    path: path.to_path_buf(),
                    entry_name: path.file_name().unwrap().to_str().unwrap().to_string(),
                    created_at: OffsetDateTime::now_utc(),
                    identity: metadata_identity(&metadata).unwrap(),
                }
            };

            let readable = parent.join("config.toml.bak-20000101");
            fs::write(&readable, "readable").unwrap();
            let readable_candidate = make_candidate(&readable);
            let mut no_preflight = |_: &BackupCandidate| Ok(());
            let mut no_after_check = |_: &BackupCandidate| Ok(());
            let readable_outcome = delete_verified_candidate(
                &readable_candidate,
                &parent_dir,
                None,
                &[],
                &mut no_preflight,
                &mut no_after_check,
            )
            .unwrap();
            assert_eq!(readable_outcome, CandidateDeletionOutcome::Deleted);
            assert!(!readable.exists());

            let unreadable = parent.join("config.toml.bak-20000102");
            fs::write(&unreadable, "unreadable").unwrap();
            let unreadable_bytes = fs::read(&unreadable).unwrap();
            let unreadable_candidate = make_candidate(&unreadable);
            fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
            let mut no_preflight = |_: &BackupCandidate| Ok(());
            let mut no_after_check = |_: &BackupCandidate| Ok(());
            let unreadable_outcome = delete_verified_candidate(
                &unreadable_candidate,
                &parent_dir,
                None,
                &[],
                &mut no_preflight,
                &mut no_after_check,
            )
            .unwrap();
            assert_eq!(
                unreadable_outcome,
                CandidateDeletionOutcome::Preserved(CandidatePreserveReason::Unreadable)
            );
            assert!(unreadable.exists());
            fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(fs::read(&unreadable).unwrap(), unreadable_bytes);

            let special = parent.join("config.toml.bak-20000103");
            let moved = parent.join("moved-special-original");
            fs::write(&special, "special-original").unwrap();
            let special_candidate = make_candidate(&special);
            let mut replace_with_fifo = |_: &BackupCandidate| -> Result<()> {
                fs::rename(&special, &moved)?;
                let special_name = CString::new(special.as_os_str().as_bytes()).unwrap();
                let rc = unsafe { libc::mkfifo(special_name.as_ptr(), 0o600) };
                if rc != 0 {
                    return Err(io::Error::last_os_error().into());
                }
                Ok(())
            };
            let mut no_after_check = |_: &BackupCandidate| Ok(());
            let special_outcome = delete_verified_candidate(
                &special_candidate,
                &parent_dir,
                None,
                &[],
                &mut replace_with_fifo,
                &mut no_after_check,
            )
            .unwrap();
            assert_eq!(
                special_outcome,
                CandidateDeletionOutcome::Preserved(CandidatePreserveReason::NonRegular)
            );
            assert!(
                fs::symlink_metadata(&special)
                    .unwrap()
                    .file_type()
                    .is_fifo()
            );
            assert_eq!(fs::read_to_string(&moved).unwrap(), "special-original");
            assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");

            let no_proc = parent.join("config.toml.bak-20000104");
            fs::write(&no_proc, "no-proc").unwrap();
            let no_proc_candidate = make_candidate(&no_proc);
            FORCE_PROC_FD_UNAVAILABLE.with(|force| force.set(true));
            let mut no_preflight = |_: &BackupCandidate| Ok(());
            let mut no_after_check = |_: &BackupCandidate| Ok(());
            let no_proc_outcome = delete_verified_candidate(
                &no_proc_candidate,
                &parent_dir,
                None,
                &[],
                &mut no_preflight,
                &mut no_after_check,
            )
            .unwrap();
            assert_eq!(
                no_proc_outcome,
                CandidateDeletionOutcome::Preserved(
                    CandidatePreserveReason::CompatibilityProbeUnavailable
                )
            );
            assert!(no_proc.exists());

            let save_root = parent.join("no-proc-save");
            fs::create_dir(&save_root).unwrap();
            let config_path = save_root.join("config.toml");
            let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
            let initial = AppConfig::default();
            let mut no_delete_hook = |_: &BackupCandidate| Ok(());
            initial
                .save_with_backup_at(&config_path, now, &mut no_delete_hook)
                .unwrap();
            for day in 1..=11 {
                fs::write(
                    save_root.join(format!("config.toml.bak-200001{day:02}")),
                    format!("legacy-{day}"),
                )
                .unwrap();
            }
            let preserved = save_root.join("config.toml.bak-20000101");
            let mut changed = AppConfig::default();
            changed.defaults.channel = Some("alerts".into());
            changed
                .save_with_backup_at(&config_path, now, &mut no_delete_hook)
                .unwrap();
            let changed_bytes = fs::read(&config_path).unwrap();
            changed
                .save_with_backup_at(&config_path, now, &mut no_delete_hook)
                .unwrap();
            assert_eq!(fs::read(&config_path).unwrap(), changed_bytes);
            assert!(preserved.exists());
            FORCE_PROC_FD_UNAVAILABLE.with(|force| force.set(false));

            FORCE_FACCESSAT2_ENOSYS.with(|force| force.set(false));
            FORCE_FACCESSAT2_EPERM.with(|force| force.set(true));
            let eperm_readable = parent.join("config.toml.bak-20000105");
            fs::write(&eperm_readable, "eperm-readable").unwrap();
            let eperm_readable_candidate = make_candidate(&eperm_readable);
            let mut no_preflight = |_: &BackupCandidate| Ok(());
            let mut no_after_check = |_: &BackupCandidate| Ok(());
            let eperm_readable_outcome = delete_verified_candidate(
                &eperm_readable_candidate,
                &parent_dir,
                None,
                &[],
                &mut no_preflight,
                &mut no_after_check,
            )
            .unwrap();
            assert_eq!(eperm_readable_outcome, CandidateDeletionOutcome::Deleted);
            assert!(!eperm_readable.exists());

            let eperm_unreadable = parent.join("config.toml.bak-20000106");
            fs::write(&eperm_unreadable, "eperm-unreadable").unwrap();
            let eperm_unreadable_candidate = make_candidate(&eperm_unreadable);
            fs::set_permissions(&eperm_unreadable, fs::Permissions::from_mode(0o000)).unwrap();
            let mut no_preflight = |_: &BackupCandidate| Ok(());
            let mut no_after_check = |_: &BackupCandidate| Ok(());
            let eperm_unreadable_outcome = delete_verified_candidate(
                &eperm_unreadable_candidate,
                &parent_dir,
                None,
                &[],
                &mut no_preflight,
                &mut no_after_check,
            )
            .unwrap();
            assert_eq!(
                eperm_unreadable_outcome,
                CandidateDeletionOutcome::Preserved(CandidatePreserveReason::Unreadable)
            );
            assert!(eperm_unreadable.exists());
            fs::set_permissions(&eperm_unreadable, fs::Permissions::from_mode(0o600)).unwrap();

            let eperm_no_proc = parent.join("config.toml.bak-20000107");
            fs::write(&eperm_no_proc, "eperm-no-proc").unwrap();
            let eperm_no_proc_candidate = make_candidate(&eperm_no_proc);
            FORCE_PROC_FD_UNAVAILABLE.with(|force| force.set(true));
            let mut no_preflight = |_: &BackupCandidate| Ok(());
            let mut no_after_check = |_: &BackupCandidate| Ok(());
            let eperm_no_proc_outcome = delete_verified_candidate(
                &eperm_no_proc_candidate,
                &parent_dir,
                None,
                &[],
                &mut no_preflight,
                &mut no_after_check,
            )
            .unwrap();
            assert_eq!(
                eperm_no_proc_outcome,
                CandidateDeletionOutcome::Preserved(
                    CandidatePreserveReason::CompatibilityProbeUnavailable
                )
            );
            assert!(eperm_no_proc.exists());
            FORCE_PROC_FD_UNAVAILABLE.with(|force| force.set(false));
            FORCE_FACCESSAT2_EPERM.with(|force| force.set(false));

            let lease_root = parent.join("lease-save");
            fs::create_dir(&lease_root).unwrap();
            let lease_config_path = lease_root.join("config.toml");
            let lease_initial = AppConfig::default();
            let mut lease_delete_hook = |_: &BackupCandidate| Ok(());
            lease_initial
                .save_with_backup_at(&lease_config_path, now, &mut lease_delete_hook)
                .unwrap();
            for day in 1..=11 {
                fs::write(
                    lease_root.join(format!("config.toml.bak-200002{day:02}")),
                    format!("lease-{day}"),
                )
                .unwrap();
            }
            let lease_candidate = lease_root.join("config.toml.bak-20000201");
            FORCE_PROC_FD_REOPEN_EAGAIN.with(|force| force.set(true));
            let mut lease_changed = AppConfig::default();
            lease_changed.defaults.channel = Some("lease-alerts".into());
            lease_changed
                .save_with_backup_at(&lease_config_path, now, &mut lease_delete_hook)
                .unwrap();
            let lease_changed_bytes = fs::read(&lease_config_path).unwrap();
            lease_changed
                .save_with_backup_at(&lease_config_path, now, &mut lease_delete_hook)
                .unwrap();
            assert_eq!(fs::read(&lease_config_path).unwrap(), lease_changed_bytes);
            assert!(lease_candidate.exists());
            FORCE_PROC_FD_REOPEN_EAGAIN.with(|force| force.set(false));
            assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");

            println!("{CHILD_OK}");
            return;
        }

        run_bounded_test_child(TEST_NAME, CHILD_ENV, CHILD_OK, true);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn backup_cleanup_rechecks_readability_after_permission_change() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        const CHILD_ENV: &str = "CLAWHIP_TEST_PERMISSION_REVOCATION_CHILD";
        const CHILD_OK: &str = "CLAWHIP_PERMISSION_REVOCATION_CHILD_OK";
        const TEST_NAME: &str =
            "config::tests::backup_cleanup_rechecks_readability_after_permission_change";

        if std::env::var_os(CHILD_ENV).is_some() {
            let root = tempfile::tempdir().unwrap();
            let parent = root.path().join("config-root");
            fs::create_dir(&parent).unwrap();
            let path = parent.join("config.toml");
            let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
            let initial = AppConfig::default();
            let mut no_delete = |_: &BackupCandidate| Ok(());
            initial
                .save_with_backup_at(&path, now, &mut no_delete)
                .unwrap();
            for day in 1..=11 {
                fs::write(
                    parent.join(format!("config.toml.bak-200001{day:02}")),
                    format!("original-{day}"),
                )
                .unwrap();
            }
            let target = parent.join("config.toml.bak-20000101");
            let target_bytes = fs::read(&target).unwrap();
            let target_metadata = fs::metadata(&target).unwrap();
            let target_identity = (target_metadata.dev(), target_metadata.ino());
            let outside = root.path().join("outside-sentinel");
            fs::write(&outside, "outside").unwrap();

            let mut changed = AppConfig::default();
            changed.defaults.channel = Some("alerts".into());
            let expected_config = changed.to_pretty_toml().unwrap().into_bytes();
            let mut before_rename = || Ok(());
            let mut before_snapshot = || Ok(());
            let mut before_delete = |_: &BackupCandidate| Ok(());
            let mut after_preflight = |_: &BackupCandidate| Ok(());
            let mut revoked = false;
            let mut after_check = |candidate: &BackupCandidate| -> Result<()> {
                if candidate.path == target && !revoked {
                    fs::set_permissions(&target, fs::Permissions::from_mode(0o000))?;
                    revoked = true;
                }
                Ok(())
            };
            changed
                .save_with_backup_at_all_candidate_hooks(
                    &path,
                    now,
                    &mut before_rename,
                    &mut before_snapshot,
                    &mut before_delete,
                    &mut after_preflight,
                    &mut after_check,
                )
                .unwrap();

            assert!(revoked);
            assert_eq!(fs::read(&path).unwrap(), expected_config);
            let changed_bytes = fs::read(&path).unwrap();
            changed
                .save_with_backup_at(&path, now, &mut no_delete)
                .unwrap();
            assert_eq!(fs::read(&path).unwrap(), changed_bytes);
            let preserved_metadata = fs::metadata(&target).unwrap();
            assert_eq!(
                (preserved_metadata.dev(), preserved_metadata.ino()),
                target_identity
            );
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(fs::read(&target).unwrap(), target_bytes);
            assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
            println!("{CHILD_OK}");
            return;
        }

        run_bounded_test_child(TEST_NAME, CHILD_ENV, CHILD_OK, true);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apple_candidate_open_errors_reprobe_only_candidate_states() {
        for code in [
            libc::EACCES,
            libc::EPERM,
            libc::ELOOP,
            libc::ENXIO,
            libc::ENODEV,
            libc::EOPNOTSUPP,
            libc::EAGAIN,
            libc::EBUSY,
            libc::EISDIR,
        ] {
            assert!(apple_candidate_open_error_needs_state_check(
                &io::Error::from_raw_os_error(code)
            ));
        }
        for code in [
            libc::EINTR,
            libc::EBADF,
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOMEM,
            libc::EIO,
            libc::EINVAL,
            libc::ENOTDIR,
        ] {
            assert!(!apple_candidate_open_error_needs_state_check(
                &io::Error::from_raw_os_error(code)
            ));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn save_with_backup_preserves_fifo_replacement_after_preflight_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::FileTypeExt;

        const CHILD_ENV: &str = "CLAWHIP_TEST_FIFO_REPLACEMENT_CHILD";
        const CHILD_OK: &str = "CLAWHIP_FIFO_REPLACEMENT_CHILD_OK";
        const TEST_NAME: &str = "config::tests::save_with_backup_preserves_fifo_replacement_after_preflight_without_blocking";

        if std::env::var_os(CHILD_ENV).is_some() {
            let root = tempfile::tempdir().unwrap();
            let parent = root.path().join("config-root");
            fs::create_dir(&parent).unwrap();
            let path = parent.join("config.toml");
            let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
            let original = AppConfig::default();
            let mut no_delete = |_: &BackupCandidate| Ok(());
            original
                .save_with_backup_at(&path, now, &mut no_delete)
                .unwrap();
            for day in 1..=11 {
                fs::write(
                    parent.join(format!("config.toml.bak-200001{day:02}")),
                    format!("original-{day}"),
                )
                .unwrap();
            }
            let target = parent.join("config.toml.bak-20000101");
            let moved = parent.join("moved-original");
            let outside = root.path().join("outside-sentinel");
            fs::write(&outside, "outside").unwrap();

            let mut changed = AppConfig::default();
            changed.defaults.channel = Some("alerts".into());
            let expected_config = changed.to_pretty_toml().unwrap().into_bytes();
            let mut before_rename = || Ok(());
            let mut before_snapshot = || Ok(());
            let mut before_delete = |_: &BackupCandidate| Ok(());
            let mut after_preflight = |candidate: &BackupCandidate| -> Result<()> {
                if candidate.path == target {
                    fs::rename(&target, &moved)?;
                    let target_name = CString::new(target.as_os_str().as_bytes()).unwrap();
                    let rc = unsafe { libc::mkfifo(target_name.as_ptr(), 0o600) };
                    if rc != 0 {
                        return Err(io::Error::last_os_error().into());
                    }
                }
                Ok(())
            };

            changed
                .save_with_backup_at_candidate_hooks(
                    &path,
                    now,
                    &mut before_rename,
                    &mut before_snapshot,
                    &mut before_delete,
                    &mut after_preflight,
                )
                .unwrap();

            assert_eq!(fs::read(&path).unwrap(), expected_config);
            assert!(
                fs::symlink_metadata(&target).unwrap().file_type().is_fifo(),
                "FIFO replacement was not preserved"
            );
            assert_eq!(fs::read_to_string(&moved).unwrap(), "original-1");
            assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
            println!("{CHILD_OK}");
            return;
        }

        run_bounded_test_child(TEST_NAME, CHILD_ENV, CHILD_OK, false);
    }

    #[cfg(unix)]
    #[test]
    fn backup_cleanup_skips_candidate_replaced_after_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        for day in 1..=11 {
            fs::write(
                parent.join(format!("config.toml.bak-200001{day:02}")),
                "original",
            )
            .unwrap();
        }
        let target = parent.join("config.toml.bak-20000101");
        let moved = parent.join("replaced-original");
        let mut replaced = false;
        let mut replace_candidate = |candidate: &BackupCandidate| -> Result<()> {
            if !replaced && candidate.path == target {
                fs::rename(&target, &moved)?;
                fs::write(&target, "replacement")?;
                replaced = true;
            }
            Ok(())
        };

        cleanup_config_backups_with(
            parent,
            None,
            "config.toml",
            None,
            &[],
            OffsetDateTime::now_utc(),
            &mut replace_candidate,
        )
        .unwrap();

        assert!(replaced);
        assert_eq!(fs::read_to_string(target).unwrap(), "replacement");
        assert_eq!(fs::read_to_string(moved).unwrap(), "original");
    }

    #[cfg(unix)]
    #[test]
    fn backup_cleanup_revalidates_config_parent_before_root_unlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("config-root");
        fs::create_dir(&parent).unwrap();
        for day in 1..=11 {
            fs::write(
                parent.join(format!("config.toml.bak-200001{day:02}")),
                "root",
            )
            .unwrap();
        }
        let outside = dir.path().join("outside-root");
        fs::create_dir(&outside).unwrap();
        let oldest_name = "config.toml.bak-20000101";
        fs::write(outside.join(oldest_name), "outside").unwrap();
        let moved = dir.path().join("moved-root");
        let mut swapped = false;
        let mut swap_parent = |_: &BackupCandidate| -> Result<()> {
            if !swapped {
                fs::rename(&parent, &moved)?;
                symlink(&outside, &parent)?;
                swapped = true;
            }
            Ok(())
        };

        let error = cleanup_config_backups_with(
            &parent,
            None,
            "config.toml",
            None,
            &[],
            OffsetDateTime::now_utc(),
            &mut swap_parent,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("config parent directory changed"));
        assert_eq!(
            fs::read_to_string(outside.join(oldest_name)).unwrap(),
            "outside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_with_backup_rejects_active_replacement_before_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let moved = dir.path().join("original-config");
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let original = AppConfig::default();
        let mut no_delete = |_: &BackupCandidate| Ok(());
        original
            .save_with_backup_at(&path, now, &mut no_delete)
            .unwrap();
        let original_bytes = fs::read(&path).unwrap();
        let mut changed = AppConfig::default();
        changed.defaults.channel = Some("alerts".into());
        let mut replace_active = || -> Result<()> {
            fs::rename(&path, &moved)?;
            fs::write(&path, "replacement")?;
            Ok(())
        };
        let mut before_snapshot = || Ok(());

        let error = changed
            .save_with_backup_at_hooks(
                &path,
                now,
                &mut replace_active,
                &mut before_snapshot,
                &mut no_delete,
            )
            .unwrap_err()
            .to_string();

        assert!(error.contains("active config changed before replacement"));
        assert!(error.contains("config was not saved"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
        assert_eq!(fs::read(moved).unwrap(), original_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn save_with_backup_rejects_exact_pid_temp_preplacement_without_touching_outside() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let original = AppConfig::default();
        let mut no_delete = |_: &BackupCandidate| Ok(());
        original
            .save_with_backup_at(&path, now, &mut no_delete)
            .unwrap();
        let original_bytes = fs::read(&path).unwrap();
        let outside = dir.path().join("outside-sentinel");
        fs::write(&outside, "sentinel").unwrap();
        let temp_name = format!(".config.toml.tmp.{}", std::process::id());
        let temp_path = dir.path().join(&temp_name);
        symlink(&outside, &temp_path).unwrap();

        let mut changed = AppConfig::default();
        changed.defaults.channel = Some("alerts".into());
        let error = changed
            .save_with_backup_at(&path, now, &mut no_delete)
            .unwrap_err()
            .to_string();

        assert!(error.contains("config temp collision") || error.contains("config was not saved"));
        assert_eq!(fs::read_to_string(&outside).unwrap(), "sentinel");
        assert_eq!(fs::read(&path).unwrap(), original_bytes);
        assert!(
            temp_path
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_with_backup_rejects_post_write_temp_replacement_before_rename() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let original = AppConfig::default();
        let mut no_delete = |_: &BackupCandidate| Ok(());
        original
            .save_with_backup_at(&path, now, &mut no_delete)
            .unwrap();
        let original_bytes = fs::read(&path).unwrap();
        let outside = dir.path().join("outside-sentinel");
        fs::write(&outside, "sentinel").unwrap();
        let temp_name = format!(".config.toml.tmp.{}", std::process::id());
        let temp_path = dir.path().join(&temp_name);
        let moved_temp = dir.path().join("moved-temp");

        let mut replace_temp = || -> Result<()> {
            if temp_path.exists() {
                fs::rename(&temp_path, &moved_temp)?;
            }
            symlink(&outside, &temp_path)?;
            Ok(())
        };
        let mut before_snapshot = || Ok(());
        let mut changed = AppConfig::default();
        changed.defaults.channel = Some("alerts".into());

        let error = changed
            .save_with_backup_at_hooks(
                &path,
                now,
                &mut replace_temp,
                &mut before_snapshot,
                &mut no_delete,
            )
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("config temp path changed before commit")
                || error.contains("config was not saved")
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "sentinel");
        assert_eq!(fs::read(&path).unwrap(), original_bytes);
        assert!(!fs::read_to_string(&path).unwrap().contains("alerts"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn backup_cleanup_preserves_candidate_replaced_after_final_identity_check() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path();
        for day in 1..=11 {
            fs::write(
                parent.join(format!("config.toml.bak-200001{day:02}")),
                "original",
            )
            .unwrap();
        }
        let target = parent.join("config.toml.bak-20000101");
        let moved = parent.join("replaced-original");
        let mut replaced = false;
        let mut before_delete = |_: &BackupCandidate| Ok(());
        let mut after_preflight = |_: &BackupCandidate| Ok(());
        let mut after_check = |candidate: &BackupCandidate| -> Result<()> {
            if !replaced && candidate.path == target {
                fs::rename(&target, &moved)?;
                fs::write(&target, "replacement")?;
                replaced = true;
            }
            Ok(())
        };

        cleanup_config_backups_with_hooks(
            parent,
            None,
            "config.toml",
            None,
            &[],
            OffsetDateTime::now_utc(),
            &mut before_delete,
            &mut after_preflight,
            &mut after_check,
        )
        .unwrap();

        assert!(replaced);
        assert_eq!(fs::read_to_string(target).unwrap(), "replacement");
        assert_eq!(fs::read_to_string(moved).unwrap(), "original");
    }

    #[cfg(unix)]
    #[test]
    fn save_with_backup_managed_snapshot_stays_in_bound_dir_after_path_swap() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let now = OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let original = AppConfig::default();
        let mut no_delete = |_: &BackupCandidate| Ok(());
        original
            .save_with_backup_at(&path, now, &mut no_delete)
            .unwrap();
        let original_bytes = fs::read(&path).unwrap();
        let backups_dir = dir.path().join(".clawhip-config-backups");
        fs::create_dir(&backups_dir).unwrap();
        let outside = dir.path().join("outside-backups");
        fs::create_dir(&outside).unwrap();
        let moved = dir.path().join("moved-backups");
        let mut swapped = false;
        let mut before_snapshot = || -> Result<()> {
            if !swapped {
                fs::rename(&backups_dir, &moved)?;
                symlink(&outside, &backups_dir)?;
                swapped = true;
            }
            Ok(())
        };
        let mut before_rename = || Ok(());
        let mut changed = AppConfig::default();
        changed.defaults.channel = Some("alerts".into());
        let result = changed.save_with_backup_at_hooks(
            &path,
            now,
            &mut before_rename,
            &mut before_snapshot,
            &mut no_delete,
        );

        assert!(swapped);
        assert!(
            fs::read_dir(&outside).unwrap().next().is_none(),
            "managed snapshot must not write outside swapped directory"
        );
        let managed_entries = fs::read_dir(&moved)
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(managed_entries.len(), 1);
        assert_eq!(fs::read(managed_entries[0].path()).unwrap(), original_bytes);
        match result {
            Ok(()) => {
                assert!(fs::read_to_string(&path).unwrap().contains("alerts"));
            }
            Err(error) => {
                let message = error.to_string();
                assert!(
                    message.contains("config was saved")
                        || message.contains("config was not saved")
                        || message.contains("managed config backup directory")
                );
                // Snapshot containment is the contract under test; path-based
                // post-commit cleanup may observe the swapped managed path.
                let _ = message;
            }
        }
    }
}

#[cfg(test)]
mod subscription_config_tests {
    use super::AppConfig;

    #[test]
    fn rejects_unknown_subscription_and_endpoint_fields() {
        let config = r#"
[[subscriptions]]
name = "gjc-workflow-gate"
enabled = false
kind = "websocket"
endpoint_env = "GJC_WS_URL"
endpoint = "wss://secret.invalid"
[subscriptions.filter]
discriminator_pointer = "/type"
discriminator_equals = "workflow_gate"
[subscriptions.projection]
workflow_id = "/workflow/id"
[subscriptions.adapter]
program = "/bin/true"
"#;
        assert!(toml::from_str::<AppConfig>(config).is_err());
    }

    #[test]
    fn rejects_subscription_names_with_surrounding_whitespace() {
        let config: AppConfig = toml::from_str(
            r#"
[[subscriptions]]
name = " workflow-gate "
enabled = false
kind = "websocket"
endpoint_env = "WORKFLOW_GATE_URL"
[subscriptions.filter]
discriminator_pointer = "/type"
discriminator_equals = "workflow_gate"
[subscriptions.projection]
workflow_id = "/workflow/id"
[subscriptions.adapter]
program = "/bin/true"
"#,
        )
        .unwrap();

        assert_eq!(
            config.validate().unwrap_err().to_string(),
            "invalid_subscription_config"
        );
    }

    #[test]
    fn documented_workflow_gate_and_question_subscriptions_parse_and_validate() {
        let config: AppConfig = toml::from_str(
            r#"
[providers.discord]
token = "fixture-token"
[[subscriptions]]
name = "gjc-workflow-gate"
enabled = false
kind = "websocket"
endpoint_env = "GJC_WORKFLOW_GATE_WS"
[subscriptions.filter]
discriminator_pointer = "/type"
discriminator_equals = "workflow_gate"
[[subscriptions.filter.predicates]]
pointer = "/gate/state"
equals = "ready"
[subscriptions.projection]
workflow_id = "/workflow/id"
gate_state = "/gate/state"
[subscriptions.adapter]
program = "/bin/true"
[subscriptions.routing]
tool = "gjc"
project = "my-project"

[[subscriptions]]
name = "gjc-question"
enabled = false
kind = "websocket"
endpoint_env = "GJC_QUESTION_WS"
[subscriptions.filter]
discriminator_pointer = "/type"
discriminator_equals = "question"
[subscriptions.projection]
question_id = "/question/id"
summary = "/question/summary"
[subscriptions.adapter]
program = "/bin/true"

[[routes]]
event = "workflow.gate"
sink = "discord"
channel = "WORKFLOW_GATE_CHANNEL_ID"
format = "alert"

[[routes]]
event = "workflow.question"
sink = "discord"
channel = "QUESTIONS_CHANNEL_ID"
format = "compact"
"#,
        )
        .unwrap();

        config.validate().unwrap();
    }
}
