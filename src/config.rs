use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

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
    #[serde(default, skip_serializing_if = "crate::update::UpdateConfig::is_empty")]
    pub update: crate::update::UpdateConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub discord: DiscordConfig,
    #[serde(default)]
    pub slack: SlackConfig,
    #[serde(default)]
    pub gemini: GeminiConfig,
    #[serde(default)]
    pub openrouter: OpenRouterConfig,
    #[serde(default)]
    pub openai: OpenAiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GeminiConfig {
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenRouterConfig {
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpenAiConfig {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
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
    #[serde(default)]
    pub format: MessageFormat,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            channel: None,
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
    pub webhook: Option<String>,
    pub slack_webhook: Option<String>,
    pub mention: Option<String>,
    #[serde(default)]
    pub allow_dynamic_tokens: bool,
    pub format: Option<MessageFormat>,
    pub template: Option<String>,
}

impl Default for RouteRule {
    fn default() -> Self {
        Self {
            event: String::new(),
            filter: BTreeMap::new(),
            sink: default_sink_name(),
            channel: None,
            webhook: None,
            slack_webhook: None,
            mention: None,
            allow_dynamic_tokens: false,
            format: None,
            template: None,
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

    pub fn slack_webhook_target(&self) -> Option<&str> {
        non_empty_trimmed(self.slack_webhook.as_deref()).or_else(|| {
            (self.sink.trim() == "slack").then(|| non_empty_trimmed(self.webhook.as_deref()))?
        })
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
    pub mention: Option<String>,
    pub format: Option<MessageFormat>,
    // ── Optional tmux content transformation ──
    #[serde(default)]
    pub summarize: bool,
    #[serde(default = "default_summarizer")]
    pub summarizer: String,
    #[serde(default)]
    pub heartbeat_mins: u64,
    /// Minimum number of new lines added before triggering summarization. 0 = no filter.
    #[serde(default)]
    pub min_new_lines: usize,
    /// Minimum minutes between LLM summarization calls for this session. 0 = no throttle.
    #[serde(default)]
    pub summarize_interval_mins: u64,
    /// Minutes between heartbeat events. 0 = disable. Overrides heartbeat_mins when set.
    #[serde(default)]
    pub heartbeat_interval: u64,
    /// Minimum minutes between AI summary events. 0 = use summarize_interval_mins.
    #[serde(default)]
    pub summary_interval: u64,
}

impl Default for TmuxSessionMonitor {
    fn default() -> Self {
        Self {
            session: String::new(),
            keywords: Vec::new(),
            keyword_window_secs: default_keyword_window_secs(),
            stale_minutes: default_stale_minutes(),
            channel: None,
            mention: None,
            format: None,
            summarize: false,
            summarizer: default_summarizer(),
            heartbeat_mins: 0,
            min_new_lines: 0,
            summarize_interval_mins: 0,
            heartbeat_interval: 0,
            summary_interval: 0,
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
fn default_true() -> bool {
    true
}

fn default_summarizer() -> String {
    "gemini:gemini-2.5-flash".to_string()
}

pub fn default_sink_name() -> String {
    "discord".to_string()
}

const DISCORD_TOKEN_ENV_VARS: [&str; 2] = ["DISCORD_TOKEN", "CLAWHIP_DISCORD_BOT_TOKEN"];

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

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, self.to_pretty_toml()?)?;
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

    pub fn webhook_route_count(&self) -> usize {
        self.routes
            .iter()
            .filter(|route| route.has_any_webhook_target())
            .count()
    }

    pub fn has_webhook_routes(&self) -> bool {
        self.webhook_route_count() > 0
    }

    pub fn validate(&self) -> Result<()> {
        if self.dispatch.ci_batch_window_secs == 0 {
            return Err("dispatch.ci_batch_window_secs must be at least 1".into());
        }
        if self.cron.poll_interval_secs == 0 {
            return Err("cron.poll_interval_secs must be at least 1".into());
        }

        for (index, route) in self.routes.iter().enumerate() {
            let sink = route.effective_sink();
            let has_channel = normalize_secret(route.channel.clone()).is_some();
            let has_discord_webhook = route.discord_webhook_target().is_some();
            let has_slack_webhook = route.slack_webhook_target().is_some();
            if route.sink.trim().is_empty() && !has_slack_webhook {
                return Err(
                    format!("route #{} ({}) must set a sink", index + 1, route.event).into(),
                );
            }
            if !matches!(sink, "discord" | "slack") {
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
                    if has_channel && has_discord_webhook {
                        return Err(format!(
                            "route #{} ({}) cannot set both channel and webhook",
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

        if self.effective_token().is_none() && !self.has_webhook_routes() {
            return Err(
                "missing Discord delivery config: configure [providers.discord].token (or legacy [discord].token) or at least one route webhook"
                    .into(),
            );
        }

        Ok(())
    }

    pub fn scaffold_webhook_quickstart(&mut self, webhook: String) {
        let webhook = webhook.trim().to_string();
        if webhook.is_empty() {
            return;
        }

        if let Some(route) = self.routes.iter_mut().find(|route| {
            route.event == "*"
                && route.filter.is_empty()
                && route.mention.is_none()
                && route.template.is_none()
        }) {
            route.sink = default_sink_name();
            route.channel = None;
            route.webhook = Some(webhook);
            return;
        }

        self.routes.push(RouteRule {
            event: "*".to_string(),
            filter: BTreeMap::new(),
            sink: default_sink_name(),
            channel: None,
            webhook: Some(webhook),
            slack_webhook: None,
            mention: None,
            allow_dynamic_tokens: false,
            format: None,
            template: None,
        });
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
            println!("  1) Set Discord bot token");
            println!("  2) Set daemon base URL");
            println!("  3) Set default channel");
            println!("  4) Set default format");
            println!("  5) Save and exit");
            println!("  6) Exit without saving");
            println!("  7) Print config template hint");
            match prompt("Selection")?.trim() {
                "1" => self.providers.discord.bot_token = empty_to_none(prompt("Bot token")?),
                "2" => {
                    self.daemon.base_url =
                        prompt_with_default("Daemon base URL", Some(&self.daemon.base_url))?
                }
                "3" => self.defaults.channel = empty_to_none(prompt("Default channel")?),
                "4" => self.defaults.format = prompt_format(Some(self.defaults.format.clone()))?,
                "5" => {
                    self.save(path)?;
                    println!("Saved {}", path.display());
                    break;
                }
                "6" => {
                    println!("Discarded changes.");
                    break;
                }
                "7" => self.print_template_hint(),
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
        println!("Edit the config file directly for routes and monitor definitions.");
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
            route.webhook = normalize_text(route.webhook.clone());
            route.slack_webhook = normalize_text(route.slack_webhook.clone());
            route.mention = normalize_text(route.mention.clone());
            route.template = normalize_text(route.template.clone());
        }

        for repo in &mut self.monitors.git.repos {
            repo.channel = normalize_text(repo.channel.clone());
            repo.mention = normalize_text(repo.mention.clone());
            repo.name = normalize_text(repo.name.clone());
            repo.github_repo = normalize_text(repo.github_repo.clone());
        }

        for session in &mut self.monitors.tmux.sessions {
            session.channel = normalize_text(session.channel.clone());
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

fn prompt(label: &str) -> Result<String> {
    print!("{label}: ");
    io::stdout().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim_end().to_string())
}

fn prompt_with_default(label: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(default) => prompt(&format!("{label} [{default}]")),
        None => prompt(label),
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

fn empty_to_none(value: String) -> Option<String> {
    normalize_text(Some(value))
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
    fn webhook_route_satisfies_delivery_validation_without_bot_token() {
        let config = AppConfig {
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
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
    fn route_cannot_set_channel_and_webhook() {
        let config = AppConfig {
            providers: ProvidersConfig {
                discord: DiscordConfig {
                    bot_token: Some("token".into()),
                    legacy_default_channel: None,
                },
                slack: SlackConfig::default(),
                ..Default::default()
            },
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                sink: default_sink_name(),
                channel: Some("123".into()),
                webhook: Some("https://discord.com/api/webhooks/123/abc".into()),
                slack_webhook: None,
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("cannot set both channel and webhook"));
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
    fn setup_scaffold_adds_tmux_keyword_webhook_route() {
        let mut config = AppConfig::default();
        config.scaffold_webhook_quickstart(" https://discord.com/api/webhooks/123/abc ".into());

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
                ..Default::default()
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
}
