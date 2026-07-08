//! Binding verification: audit Discord channel bindings against live server state.
//!
//! Walks the config to collect every channel-ID reference, then queries the
//! Discord API to confirm each channel exists and (optionally) that the live
//! name matches the operator's `channel_name` hint.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::Serialize;

use crate::config::AppConfig;
use crate::discord::DiscordClient;

// ── Channel lookup result ────────────────────────────────────────────

/// Result of resolving a single Discord channel ID against the live API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ChannelLookup {
    /// Channel exists — `name` is `None` for DM-style channels without a name.
    Found { id: String, name: Option<String> },
    /// The channel ID returned 404.
    NotFound,
    /// Bot lacks permission (403).
    Forbidden,
    /// Bot token is invalid (401).
    Unauthorized,
    /// No bot token configured — lookup skipped.
    NoToken,
    /// Network or API error.
    Transport(String),
}

// ── Binding extraction ───────────────────────────────────────────────

/// Where a channel reference was found in the config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingSource {
    DefaultChannel,
    Route { index: usize },
    GitMonitor { index: usize },
    TmuxMonitor { index: usize },
    WorkspaceMonitor { index: usize },
}

impl fmt::Display for BindingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DefaultChannel => write!(f, "defaults.channel"),
            Self::Route { index } => write!(f, "routes[{}]", index + 1),
            Self::GitMonitor { index } => write!(f, "monitors.git.repos[{}]", index + 1),
            Self::TmuxMonitor { index } => write!(f, "monitors.tmux.sessions[{}]", index + 1),
            Self::WorkspaceMonitor { index } => write!(f, "monitors.workspace[{}]", index + 1),
        }
    }
}

/// A channel reference extracted from the config, with an optional expected-name hint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChannelBinding {
    pub channel_id: String,
    pub expected_name: Option<String>,
    pub source: BindingSource,
    /// Freeform label for operator context (e.g. route event+filter, repo name).
    pub label: String,
}

/// Walk the config and collect every distinct channel-ID reference.
///
/// De-duplicates by `(channel_id, source)` so the same ID referenced from
/// both the default and a route appears twice (one per source) but the same
/// ID referenced twice in the same context does not.
pub fn collect_bindings(config: &AppConfig) -> Vec<ChannelBinding> {
    let mut bindings = Vec::new();

    // defaults.channel
    if let Some(channel) = config.defaults.channel.as_deref()
        && !channel.is_empty()
    {
        bindings.push(ChannelBinding {
            channel_id: channel.to_string(),
            expected_name: config.defaults.channel_name.clone(),
            source: BindingSource::DefaultChannel,
            label: "default channel".to_string(),
        });
    }

    // routes
    for (index, route) in config.routes.iter().enumerate() {
        if route.effective_sink() == "discord"
            && let Some(channel) = route.channel.as_deref()
            && !channel.is_empty()
        {
            let label = if route.filter.is_empty() {
                format!("event={}", route.event)
            } else {
                let filters: Vec<String> = route
                    .filter
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect();
                format!("event={} filter={{{}}}", route.event, filters.join(", "))
            };
            bindings.push(ChannelBinding {
                channel_id: channel.to_string(),
                expected_name: route.channel_name.clone(),
                source: BindingSource::Route { index },
                label,
            });
        }

        // Discord threads are intentionally excluded from channel-binding
        // verification. The public verify-bindings output is a channel audit;
        // treating thread IDs as channel IDs would expose private thread
        // identifiers and live thread names through text/JSON diagnostics.
    }

    // git monitors
    for (index, repo) in config.monitors.git.repos.iter().enumerate() {
        if let Some(channel) = repo.channel.as_deref()
            && !channel.is_empty()
        {
            let label = repo
                .name
                .clone()
                .unwrap_or_else(|| format!("git:{}", repo.path));
            bindings.push(ChannelBinding {
                channel_id: channel.to_string(),
                expected_name: repo.channel_name.clone(),
                source: BindingSource::GitMonitor { index },
                label,
            });
        }
    }

    // tmux monitors
    for (index, session) in config.monitors.tmux.sessions.iter().enumerate() {
        if let Some(channel) = session.channel.as_deref()
            && !channel.is_empty()
        {
            bindings.push(ChannelBinding {
                channel_id: channel.to_string(),
                expected_name: session.channel_name.clone(),
                source: BindingSource::TmuxMonitor { index },
                label: format!("tmux:{}", session.session),
            });
        }
    }

    // workspace monitors
    for (index, workspace) in config.monitors.workspace.iter().enumerate() {
        if let Some(channel) = workspace.channel.as_deref()
            && !channel.is_empty()
        {
            bindings.push(ChannelBinding {
                channel_id: channel.to_string(),
                expected_name: None,
                source: BindingSource::WorkspaceMonitor { index },
                label: format!("workspace:{}", workspace.path),
            });
        }
    }

    bindings
}

// ── Verification ─────────────────────────────────────────────────────

/// Verdict for a single binding after comparing the live API response against
/// the expected-name hint (if one was set).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum VerdictKind {
    /// channel_name hint matches the live name.
    Match { live_name: String },
    /// channel_name hint does NOT match the live name.
    Mismatch {
        live_name: String,
        expected_name: String,
    },
    /// No channel_name hint was set — the channel resolved, here's the name.
    Resolved { live_name: Option<String> },
    /// Channel ID returned 404.
    NotFound,
    /// Bot lacks access (403).
    Forbidden,
    /// Bot token invalid (401).
    Unauthorized,
    /// No bot token configured.
    NoToken,
    /// Network or API failure.
    Transport { message: String },
}

impl VerdictKind {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Match { .. } | Self::Resolved { .. })
    }
}

/// One binding + its resolved verdict.
#[derive(Debug, Clone, Serialize)]
pub struct BindingVerdict {
    pub binding: ChannelBinding,
    pub verdict: VerdictKind,
}

/// Aggregate audit result for all bindings.
#[derive(Debug, Clone, Serialize)]
pub struct BindingAudit {
    pub verdicts: Vec<BindingVerdict>,
}

impl BindingAudit {
    pub fn all_ok(&self) -> bool {
        self.verdicts.iter().all(|entry| entry.verdict.is_ok())
    }
}

/// Aggregate route↔git-monitor drift result for setup-owned repo bindings.
#[derive(Debug, Clone, Serialize)]
pub struct BindingDriftAudit {
    pub ok: bool,
    pub findings: Vec<BindingDriftFinding>,
}

/// A public-safe route↔git-monitor drift finding.
#[derive(Debug, Clone, Serialize)]
pub struct BindingDriftFinding {
    pub severity: String,
    pub code: String,
    pub repo: String,
    pub route_indices: Vec<usize>,
    pub monitor_indices: Vec<usize>,
    pub route_channel_id: Option<String>,
    pub monitor_channel_id: Option<String>,
    pub route_channel_name: Option<String>,
    pub monitor_channel_name: Option<String>,
    pub checkout_label: Option<String>,
    pub message: String,
}

/// Audit setup-owned repo routes and git monitors for local config drift.
pub fn audit_route_monitor_drift(config: &AppConfig) -> BindingDriftAudit {
    let mut findings = Vec::new();
    let routes = setup_repo_routes(config);
    let monitors = setup_git_monitors(config);

    for route in &routes {
        if route.indices.len() > 1 {
            findings.push(finding(FindingParams {
                severity: "error",
                code: "duplicate_setup_route",
                repo: &route.repo,
                route_indices: route.indices.clone(),
                route_channel_id: route.channel.clone(),
                route_channel_name: route.channel_name.clone(),
                message: format!("repo '{}' has multiple setup-owned routes", route.repo),
                ..FindingParams::default()
            }));
        }

        let matching: Vec<&SetupMonitor> = monitors
            .iter()
            .filter(|monitor| monitor.repo.as_deref() == Some(route.repo.as_str()))
            .collect();
        if matching.is_empty() {
            findings.push(finding(FindingParams {
                severity: "error",
                code: "missing_git_monitor",
                repo: &route.repo,
                route_indices: route.indices.clone(),
                route_channel_id: route.channel.clone(),
                route_channel_name: route.channel_name.clone(),
                message: format!(
                    "repo '{}' has a setup-owned route but no setup-owned git monitor",
                    route.repo
                ),
                ..FindingParams::default()
            }));
        }

        for monitor in matching {
            if route.channel.as_deref() != monitor.channel.as_deref() {
                findings.push(finding(FindingParams {
                    severity: "error",
                    code: "channel_mismatch",
                    repo: &route.repo,
                    route_indices: route.indices.clone(),
                    monitor_indices: vec![monitor.index],
                    route_channel_id: route.channel.clone(),
                    monitor_channel_id: monitor.channel.clone(),
                    route_channel_name: route.channel_name.clone(),
                    monitor_channel_name: monitor.channel_name.clone(),
                    checkout_label: Some(checkout_label(&monitor.path)),
                    message: format!(
                        "repo '{}' route and git monitor target different channels",
                        route.repo
                    ),
                }));
            }
            if route.channel_name != monitor.channel_name {
                findings.push(finding(FindingParams {
                    severity: "error",
                    code: "channel_name_mismatch",
                    repo: &route.repo,
                    route_indices: route.indices.clone(),
                    monitor_indices: vec![monitor.index],
                    route_channel_id: route.channel.clone(),
                    monitor_channel_id: monitor.channel.clone(),
                    route_channel_name: route.channel_name.clone(),
                    monitor_channel_name: monitor.channel_name.clone(),
                    checkout_label: Some(checkout_label(&monitor.path)),
                    message: format!(
                        "repo '{}' route and git monitor have different channel_name hints",
                        route.repo
                    ),
                }));
            }
            if !Path::new(&monitor.path).exists() {
                findings.push(finding(FindingParams {
                    severity: "error",
                    code: "checkout_missing",
                    repo: &route.repo,
                    route_indices: route.indices.clone(),
                    monitor_indices: vec![monitor.index],
                    route_channel_id: route.channel.clone(),
                    monitor_channel_id: monitor.channel.clone(),
                    route_channel_name: route.channel_name.clone(),
                    monitor_channel_name: monitor.channel_name.clone(),
                    checkout_label: Some(checkout_label(&monitor.path)),
                    message: format!("repo '{}' git monitor checkout is missing", route.repo),
                }));
            } else if !looks_like_git_worktree(&monitor.path) {
                findings.push(finding(FindingParams {
                    severity: "error",
                    code: "checkout_not_git_worktree",
                    repo: &route.repo,
                    route_indices: route.indices.clone(),
                    monitor_indices: vec![monitor.index],
                    route_channel_id: route.channel.clone(),
                    monitor_channel_id: monitor.channel.clone(),
                    route_channel_name: route.channel_name.clone(),
                    monitor_channel_name: monitor.channel_name.clone(),
                    checkout_label: Some(checkout_label(&monitor.path)),
                    message: format!(
                        "repo '{}' git monitor checkout is not a git worktree",
                        route.repo
                    ),
                }));
            }
        }

        for monitor in monitors.iter().filter(|monitor| monitor.repo.is_none()) {
            if route.channel.is_some() && route.channel == monitor.channel {
                findings.push(finding(FindingParams {
                    severity: "error",
                    code: "manual_monitor_conflict",
                    repo: &route.repo,
                    route_indices: route.indices.clone(),
                    monitor_indices: vec![monitor.index],
                    route_channel_id: route.channel.clone(),
                    monitor_channel_id: monitor.channel.clone(),
                    route_channel_name: route.channel_name.clone(),
                    monitor_channel_name: monitor.channel_name.clone(),
                    checkout_label: Some(checkout_label(&monitor.path)),
                    message: format!(
                        "repo '{}' route shares a channel with a manual channel-only git monitor",
                        route.repo
                    ),
                }));
            }
        }
    }

    for monitor in &monitors {
        if monitor.indices_len > 1 {
            findings.push(finding(FindingParams {
                severity: "error",
                code: "duplicate_setup_monitor",
                repo: monitor.repo.as_deref().unwrap_or("<manual>"),
                monitor_indices: vec![monitor.index],
                monitor_channel_id: monitor.channel.clone(),
                monitor_channel_name: monitor.channel_name.clone(),
                checkout_label: Some(checkout_label(&monitor.path)),
                message: "multiple setup-owned git monitors share the same repo identity"
                    .to_string(),
                ..FindingParams::default()
            }));
        }
    }

    for monitor in monitors.iter().filter(|monitor| monitor.repo.is_some()) {
        let repo = monitor.repo.as_ref().unwrap();
        if !routes.iter().any(|route| route.repo == *repo) {
            findings.push(finding(FindingParams {
                severity: "error",
                code: "repo_identity_mismatch",
                repo,
                monitor_indices: vec![monitor.index],
                monitor_channel_id: monitor.channel.clone(),
                monitor_channel_name: monitor.channel_name.clone(),
                checkout_label: Some(checkout_label(&monitor.path)),
                message: format!(
                    "git monitor repo '{}' has no matching setup-owned route",
                    repo
                ),
                ..FindingParams::default()
            }));
        }
    }

    for (index, route) in config.routes.iter().enumerate() {
        if !is_setup_repo_route(route)
            && route.event == "*"
            && route.effective_sink() == "discord"
            && route.channel.is_some()
            && route.filter.len() == 1
            && route.filter.contains_key("repo")
        {
            let repo = route.filter.get("repo").cloned().unwrap_or_default();
            if routes.iter().any(|setup| setup.repo == repo) {
                findings.push(finding(FindingParams {
                    severity: "error",
                    code: "manual_route_conflict",
                    repo: &repo,
                    route_indices: vec![index],
                    route_channel_id: route.channel.clone(),
                    route_channel_name: route.channel_name.clone(),
                    message: format!("repo '{}' has a manual wildcard Discord route", repo),
                    ..FindingParams::default()
                }));
            }
        }
    }

    BindingDriftAudit {
        ok: findings.is_empty(),
        findings,
    }
}

#[derive(Debug)]
struct SetupRoute {
    repo: String,
    indices: Vec<usize>,
    channel: Option<String>,
    channel_name: Option<String>,
}

#[derive(Debug)]
struct SetupMonitor {
    repo: Option<String>,
    index: usize,
    indices_len: usize,
    path: String,
    channel: Option<String>,
    channel_name: Option<String>,
}

fn setup_repo_routes(config: &AppConfig) -> Vec<SetupRoute> {
    let mut by_repo: BTreeMap<String, SetupRoute> = BTreeMap::new();
    for (index, route) in config.routes.iter().enumerate() {
        if is_setup_repo_route(route) {
            let repo = route.filter.get("repo").cloned().unwrap_or_default();
            by_repo
                .entry(repo.clone())
                .and_modify(|entry| entry.indices.push(index))
                .or_insert_with(|| SetupRoute {
                    repo,
                    indices: vec![index],
                    channel: route.channel.clone(),
                    channel_name: route.channel_name.clone(),
                });
        }
    }
    by_repo.into_values().collect()
}

fn setup_git_monitors(config: &AppConfig) -> Vec<SetupMonitor> {
    let mut identity_counts: BTreeMap<String, usize> = BTreeMap::new();
    for monitor in &config.monitors.git.repos {
        if let Some(repo) = setup_monitor_repo_identity(monitor) {
            *identity_counts.entry(repo).or_default() += 1;
        }
    }

    config
        .monitors
        .git
        .repos
        .iter()
        .enumerate()
        .map(|(index, monitor)| {
            let repo = setup_monitor_repo_identity(monitor);
            let indices_len = repo
                .as_ref()
                .and_then(|repo| identity_counts.get(repo))
                .copied()
                .unwrap_or(1);
            SetupMonitor {
                repo,
                index,
                indices_len,
                path: monitor.path.clone(),
                channel: monitor.channel.clone(),
                channel_name: monitor.channel_name.clone(),
            }
        })
        .collect()
}

fn is_setup_repo_route(route: &crate::config::RouteRule) -> bool {
    route.event == "*"
        && route.sink.trim() == "discord"
        && route.effective_sink() == "discord"
        && route.filter.len() == 1
        && route
            .filter
            .get("repo")
            .map(|repo| !repo.trim().is_empty())
            .unwrap_or(false)
        && route
            .channel
            .as_deref()
            .map(|channel| !channel.trim().is_empty())
            .unwrap_or(false)
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

fn setup_monitor_repo_identity(monitor: &crate::config::GitRepoMonitor) -> Option<String> {
    if !monitor.setup_owned {
        return None;
    }
    if monitor
        .channel
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
        || monitor
            .channel_name
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        return None;
    }

    monitor_repo_identity(monitor)
}

fn monitor_repo_identity(monitor: &crate::config::GitRepoMonitor) -> Option<String> {
    monitor
        .github_repo
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            monitor
                .name
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
}

fn looks_like_git_worktree(path: &str) -> bool {
    let path = Path::new(path);
    path.join(".git").exists()
        || path
            .parent()
            .map(|parent| parent.join(".git").exists())
            .unwrap_or(false)
}

fn checkout_label(path: &str) -> String {
    let path = Path::new(path);
    let basename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("checkout");
    format!(
        "{}#{:08x}",
        basename,
        stable_hash32(path.to_string_lossy().as_ref())
    )
}

fn stable_hash32(value: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in value.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

#[derive(Default)]
struct FindingParams<'a> {
    severity: &'a str,
    code: &'a str,
    repo: &'a str,
    route_indices: Vec<usize>,
    monitor_indices: Vec<usize>,
    route_channel_id: Option<String>,
    monitor_channel_id: Option<String>,
    route_channel_name: Option<String>,
    monitor_channel_name: Option<String>,
    checkout_label: Option<String>,
    message: String,
}

fn finding(params: FindingParams<'_>) -> BindingDriftFinding {
    BindingDriftFinding {
        severity: params.severity.to_string(),
        code: params.code.to_string(),
        repo: params.repo.to_string(),
        route_indices: params.route_indices,
        monitor_indices: params.monitor_indices,
        route_channel_id: params.route_channel_id,
        monitor_channel_id: params.monitor_channel_id,
        route_channel_name: params.route_channel_name,
        monitor_channel_name: params.monitor_channel_name,
        checkout_label: params.checkout_label,
        message: params.message,
    }
}

/// Resolve a lookup result into a verdict given the expected-name hint.
fn resolve_verdict(lookup: ChannelLookup, expected: &Option<String>) -> VerdictKind {
    match lookup {
        ChannelLookup::Found { name, .. } => match expected {
            Some(expected_name) => {
                let live = name.as_deref().unwrap_or("");
                let expect = expected_name.trim().trim_start_matches('#');
                if live.eq_ignore_ascii_case(expect) {
                    VerdictKind::Match {
                        live_name: live.to_string(),
                    }
                } else {
                    VerdictKind::Mismatch {
                        live_name: live.to_string(),
                        expected_name: expect.to_string(),
                    }
                }
            }
            None => VerdictKind::Resolved { live_name: name },
        },
        ChannelLookup::NotFound => VerdictKind::NotFound,
        ChannelLookup::Forbidden => VerdictKind::Forbidden,
        ChannelLookup::Unauthorized => VerdictKind::Unauthorized,
        ChannelLookup::NoToken => VerdictKind::NoToken,
        ChannelLookup::Transport(message) => VerdictKind::Transport { message },
    }
}

/// Verify all extracted bindings against the live Discord API.
pub async fn verify(client: &DiscordClient, config: &AppConfig) -> BindingAudit {
    let bindings = collect_bindings(config);
    let mut verdicts = Vec::with_capacity(bindings.len());

    for binding in bindings {
        let lookup = client.lookup_channel(&binding.channel_id).await;
        let verdict = resolve_verdict(lookup, &binding.expected_name);
        verdicts.push(BindingVerdict { binding, verdict });
    }

    BindingAudit { verdicts }
}

// ── Display ──────────────────────────────────────────────────────────

impl fmt::Display for BindingAudit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.verdicts.is_empty() {
            writeln!(f, "No channel bindings found in config.")?;
            return Ok(());
        }

        for entry in &self.verdicts {
            let tag = if entry.verdict.is_ok() { "ok" } else { "FAIL" };
            write!(
                f,
                "[{tag:>4}] {} -> {} ",
                entry.binding.source, entry.binding.channel_id
            )?;
            match &entry.verdict {
                VerdictKind::Match { live_name } => {
                    writeln!(f, "(#{live_name}) -- matches hint")?;
                }
                VerdictKind::Mismatch {
                    live_name,
                    expected_name,
                } => {
                    writeln!(f, "(#{live_name}) -- MISMATCH: expected #{expected_name}")?;
                }
                VerdictKind::Resolved {
                    live_name: Some(name),
                } => {
                    writeln!(f, "(#{name})")?;
                }
                VerdictKind::Resolved { live_name: None } => {
                    writeln!(f, "(unnamed channel)")?;
                }
                VerdictKind::NotFound => {
                    writeln!(f, "-- NOT FOUND (deleted or wrong ID)")?;
                }
                VerdictKind::Forbidden => {
                    writeln!(f, "-- FORBIDDEN (bot lacks access)")?;
                }
                VerdictKind::Unauthorized => {
                    writeln!(f, "-- UNAUTHORIZED (invalid bot token)")?;
                }
                VerdictKind::NoToken => {
                    writeln!(f, "-- SKIPPED (no bot token configured)")?;
                }
                VerdictKind::Transport { message } => {
                    writeln!(f, "-- ERROR: {message}")?;
                }
            }
        }

        let total = self.verdicts.len();
        let ok_count = self.verdicts.iter().filter(|e| e.verdict.is_ok()).count();
        let fail_count = total - ok_count;
        writeln!(f)?;
        if fail_count == 0 {
            writeln!(f, "{total} binding(s) verified, all OK.")?;
        } else {
            writeln!(
                f,
                "{total} binding(s) checked: {ok_count} OK, {fail_count} failed."
            )?;
        }

        Ok(())
    }
}

impl fmt::Display for BindingDriftAudit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Route/monitor drift:")?;
        if self.findings.is_empty() {
            writeln!(f, "[  ok] no route/monitor drift found")?;
            return Ok(());
        }
        for finding in &self.findings {
            let route = finding
                .route_indices
                .first()
                .map(|index| (index + 1).to_string())
                .unwrap_or_else(|| "-".to_string());
            let monitor = finding
                .monitor_indices
                .first()
                .map(|index| (index + 1).to_string())
                .unwrap_or_else(|| "-".to_string());
            let checkout = finding.checkout_label.as_deref().unwrap_or("-");
            writeln!(
                f,
                "[FAIL] {} repo={} route={} monitor={} checkout={} {}",
                finding.code, finding.repo, route, monitor, checkout, finding.message
            )?;
        }
        Ok(())
    }
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::config::{
        DefaultsConfig, GitMonitorConfig, GitRepoMonitor, MonitorConfig, RouteRule,
        TmuxMonitorConfig, TmuxSessionMonitor, WorkspaceMonitor,
    };

    fn config_with_routes(routes: Vec<RouteRule>) -> AppConfig {
        AppConfig {
            routes,
            ..AppConfig::default()
        }
    }

    #[test]
    fn collects_default_channel_binding() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("111".into()),
                channel_name: Some("alerts".into()),
                ..DefaultsConfig::default()
            },
            ..AppConfig::default()
        };
        let bindings = collect_bindings(&config);
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].channel_id, "111");
        assert_eq!(bindings[0].expected_name.as_deref(), Some("alerts"));
        assert_eq!(bindings[0].source, BindingSource::DefaultChannel);
    }

    #[test]
    fn collects_route_binding_with_filter() {
        let mut filter = BTreeMap::new();
        filter.insert("repo".into(), "clawhip".into());
        let config = config_with_routes(vec![RouteRule {
            event: "*".into(),
            filter,
            channel: Some("222".into()),
            thread: None,
            channel_name: Some("clawhip-dev".into()),
            ..RouteRule::default()
        }]);
        let bindings = collect_bindings(&config);
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].channel_id, "222");
        assert_eq!(bindings[0].expected_name.as_deref(), Some("clawhip-dev"));
        assert!(bindings[0].label.contains("repo=clawhip"));
    }

    #[test]
    fn skips_route_thread_binding_to_keep_diagnostics_public_safe() {
        let config = config_with_routes(vec![RouteRule {
            event: "session.*".into(),
            thread: Some("123456789012345678".into()),
            channel_name: Some("private-thread-name".into()),
            ..RouteRule::default()
        }]);

        let bindings = collect_bindings(&config);

        assert!(bindings.is_empty());
    }

    #[test]
    fn audit_text_and_json_do_not_expose_thread_id_or_name() {
        let config = config_with_routes(vec![RouteRule {
            event: "session.*".into(),
            thread: Some("123456789012345678".into()),
            channel_name: Some("private-thread-name".into()),
            ..RouteRule::default()
        }]);
        let audit = BindingAudit {
            verdicts: collect_bindings(&config)
                .into_iter()
                .map(|binding| BindingVerdict {
                    binding,
                    verdict: VerdictKind::NoToken,
                })
                .collect(),
        };

        let text = audit.to_string();
        let json = serde_json::to_string(&audit).unwrap();

        for rendered in [text, json] {
            assert!(!rendered.contains("123456789012345678"));
            assert!(!rendered.contains("private-thread-name"));
        }
    }

    #[test]
    fn collects_git_monitor_binding() {
        let config = AppConfig {
            monitors: MonitorConfig {
                git: GitMonitorConfig {
                    repos: vec![GitRepoMonitor {
                        path: "/repo".into(),
                        name: Some("my-repo".into()),
                        channel: Some("333".into()),
                        channel_name: Some("my-repo-dev".into()),
                        ..GitRepoMonitor::default()
                    }],
                },
                ..MonitorConfig::default()
            },
            ..AppConfig::default()
        };
        let bindings = collect_bindings(&config);
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].label, "my-repo");
    }

    #[test]
    fn collects_tmux_monitor_binding() {
        let config = AppConfig {
            monitors: MonitorConfig {
                tmux: TmuxMonitorConfig {
                    sessions: vec![TmuxSessionMonitor {
                        session: "issue-42".into(),
                        channel: Some("444".into()),
                        channel_name: Some("dev".into()),
                        ..TmuxSessionMonitor::default()
                    }],
                },
                ..MonitorConfig::default()
            },
            ..AppConfig::default()
        };
        let bindings = collect_bindings(&config);
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].label, "tmux:issue-42");
    }

    #[test]
    fn collects_workspace_monitor_binding() {
        let config = AppConfig {
            monitors: MonitorConfig {
                workspace: vec![WorkspaceMonitor {
                    path: "/workspace".into(),
                    channel: Some("555".into()),
                    ..WorkspaceMonitor::default()
                }],
                ..MonitorConfig::default()
            },
            ..AppConfig::default()
        };
        let bindings = collect_bindings(&config);
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].channel_id, "555");
        assert_eq!(
            bindings[0].source,
            BindingSource::WorkspaceMonitor { index: 0 }
        );
        assert_eq!(bindings[0].label, "workspace:/workspace");
    }

    #[test]
    fn skips_empty_channel_fields() {
        let config = config_with_routes(vec![RouteRule {
            event: "*".into(),
            channel: None,
            ..RouteRule::default()
        }]);
        assert!(collect_bindings(&config).is_empty());
    }

    #[test]
    fn verdict_match_when_hint_matches() {
        let lookup = ChannelLookup::Found {
            id: "1".into(),
            name: Some("clawhip-dev".into()),
        };
        let verdict = resolve_verdict(lookup, &Some("clawhip-dev".into()));
        assert!(matches!(verdict, VerdictKind::Match { .. }));
    }

    #[test]
    fn verdict_match_case_insensitive() {
        let lookup = ChannelLookup::Found {
            id: "1".into(),
            name: Some("Clawhip-Dev".into()),
        };
        let verdict = resolve_verdict(lookup, &Some("clawhip-dev".into()));
        assert!(matches!(verdict, VerdictKind::Match { .. }));
    }

    #[test]
    fn verdict_match_strips_hash_prefix() {
        let lookup = ChannelLookup::Found {
            id: "1".into(),
            name: Some("omc-dev".into()),
        };
        let verdict = resolve_verdict(lookup, &Some("#omc-dev".into()));
        assert!(matches!(verdict, VerdictKind::Match { .. }));
    }

    #[test]
    fn verdict_mismatch_on_different_name() {
        let lookup = ChannelLookup::Found {
            id: "1".into(),
            name: Some("omx-dev".into()),
        };
        let verdict = resolve_verdict(lookup, &Some("omc-dev".into()));
        assert!(matches!(verdict, VerdictKind::Mismatch { .. }));
    }

    #[test]
    fn verdict_resolved_without_hint() {
        let lookup = ChannelLookup::Found {
            id: "1".into(),
            name: Some("omc-dev".into()),
        };
        let verdict = resolve_verdict(lookup, &None);
        assert!(matches!(verdict, VerdictKind::Resolved { .. }));
    }

    #[test]
    fn verdict_not_found() {
        assert!(matches!(
            resolve_verdict(ChannelLookup::NotFound, &None),
            VerdictKind::NotFound
        ));
    }

    #[test]
    fn verdict_no_token() {
        assert!(matches!(
            resolve_verdict(ChannelLookup::NoToken, &None),
            VerdictKind::NoToken
        ));
    }

    #[test]
    fn audit_display_shows_summary() {
        let audit = BindingAudit {
            verdicts: vec![
                BindingVerdict {
                    binding: ChannelBinding {
                        channel_id: "111".into(),
                        expected_name: Some("omc-dev".into()),
                        source: BindingSource::Route { index: 0 },
                        label: "event=* filter={repo=omc}".into(),
                    },
                    verdict: VerdictKind::Match {
                        live_name: "omc-dev".into(),
                    },
                },
                BindingVerdict {
                    binding: ChannelBinding {
                        channel_id: "222".into(),
                        expected_name: Some("omc-dev".into()),
                        source: BindingSource::Route { index: 1 },
                        label: "event=* filter={repo=omx}".into(),
                    },
                    verdict: VerdictKind::Mismatch {
                        live_name: "omx-dev".into(),
                        expected_name: "omc-dev".into(),
                    },
                },
            ],
        };
        let text = audit.to_string();
        assert!(text.contains("[  ok]"));
        assert!(text.contains("[FAIL]"));
        assert!(text.contains("MISMATCH"));
        assert!(text.contains("1 OK, 1 failed"));
    }

    #[test]
    fn audit_display_empty() {
        let audit = BindingAudit {
            verdicts: Vec::new(),
        };
        assert!(audit.to_string().contains("No channel bindings"));
    }

    #[test]
    fn apply_repo_channel_binding_creates_route_and_monitor() {
        let mut config = AppConfig::default();
        config
            .apply_repo_channel_binding("clawhip", "123456", Some("clawhip-dev"), "/work/clawhip")
            .unwrap();
        assert_eq!(config.routes.len(), 1);
        assert_eq!(config.routes[0].channel.as_deref(), Some("123456"));
        assert_eq!(
            config.routes[0].channel_name.as_deref(),
            Some("clawhip-dev")
        );
        assert_eq!(config.routes[0].filter.get("repo").unwrap(), "clawhip");
    }

    #[test]
    fn apply_repo_channel_binding_updates_existing() {
        let mut config = AppConfig::default();
        config
            .apply_repo_channel_binding("clawhip", "111", Some("old-name"), "/work/clawhip")
            .unwrap();
        config
            .apply_repo_channel_binding("clawhip", "222", Some("new-name"), "/work/clawhip")
            .unwrap();
        assert_eq!(config.routes.len(), 1);
        assert_eq!(config.routes[0].channel.as_deref(), Some("222"));
        assert_eq!(config.routes[0].channel_name.as_deref(), Some("new-name"));
    }

    #[test]
    fn apply_owner_qualified_binding_does_not_steal_different_github_repo_by_basename() {
        let mut config = AppConfig::default();
        config.monitors.git.repos.push(GitRepoMonitor {
            path: "/work/owner1-repo".into(),
            name: Some("repo".into()),
            github_repo: Some("owner1/repo".into()),
            channel: Some("old".into()),
            channel_name: Some("old-dev".into()),
            setup_owned: true,
            ..GitRepoMonitor::default()
        });

        config
            .apply_repo_channel_binding("owner2/repo", "new", Some("new-dev"), "/work/owner2-repo")
            .unwrap();

        assert_eq!(config.monitors.git.repos.len(), 2);
        let old = &config.monitors.git.repos[0];
        assert_eq!(old.path, "/work/owner1-repo");
        assert_eq!(old.github_repo.as_deref(), Some("owner1/repo"));
        assert_eq!(old.channel.as_deref(), Some("old"));
        let new = &config.monitors.git.repos[1];
        assert_eq!(new.path, "/work/owner2-repo");
        assert_eq!(new.name.as_deref(), Some("repo"));
        assert_eq!(new.github_repo.as_deref(), Some("owner2/repo"));
        assert_eq!(new.channel.as_deref(), Some("new"));
        assert!(new.setup_owned);
    }

    fn setup_route(repo: &str, channel: &str, channel_name: Option<&str>) -> RouteRule {
        let mut filter = BTreeMap::new();
        filter.insert("repo".to_string(), repo.to_string());
        RouteRule {
            event: "*".to_string(),
            filter,
            channel: Some(channel.to_string()),
            channel_name: channel_name.map(ToOwned::to_owned),
            ..RouteRule::default()
        }
    }

    fn git_monitor(
        path: &str,
        name: Option<&str>,
        channel: Option<&str>,
        channel_name: Option<&str>,
    ) -> GitRepoMonitor {
        GitRepoMonitor {
            path: path.to_string(),
            name: name.map(ToOwned::to_owned),
            channel: channel.map(ToOwned::to_owned),
            channel_name: channel_name.map(ToOwned::to_owned),
            setup_owned: true,
            ..GitRepoMonitor::default()
        }
    }

    fn config_with_route_and_monitors(route: RouteRule, repos: Vec<GitRepoMonitor>) -> AppConfig {
        AppConfig {
            routes: vec![route],
            monitors: MonitorConfig {
                git: GitMonitorConfig { repos },
                ..MonitorConfig::default()
            },
            ..AppConfig::default()
        }
    }

    #[test]
    fn drift_audit_detects_missing_monitor() {
        let config = config_with_routes(vec![setup_route("clawhip", "123", Some("dev"))]);

        let audit = audit_route_monitor_drift(&config);

        assert!(!audit.ok);
        assert_eq!(audit.findings[0].code, "missing_git_monitor");
        assert_eq!(audit.findings[0].repo, "clawhip");
        assert_eq!(audit.findings[0].route_channel_id.as_deref(), Some("123"));
    }

    #[test]
    fn drift_audit_ignores_manual_git_monitor_without_route_binding() {
        let config = AppConfig {
            monitors: MonitorConfig {
                git: GitMonitorConfig {
                    repos: vec![git_monitor("/tmp", Some("manual"), None, None)],
                },
                ..MonitorConfig::default()
            },
            ..AppConfig::default()
        };

        let audit = audit_route_monitor_drift(&config);

        assert!(audit.ok);
        assert!(audit.findings.is_empty());
    }

    #[test]
    fn drift_audit_detects_channel_mismatch() {
        let tempdir = tempfile::tempdir().unwrap();
        std::fs::create_dir(tempdir.path().join(".git")).unwrap();
        let config = config_with_route_and_monitors(
            setup_route("clawhip", "123", Some("dev")),
            vec![git_monitor(
                tempdir.path().to_str().unwrap(),
                Some("clawhip"),
                Some("456"),
                Some("dev"),
            )],
        );

        let audit = audit_route_monitor_drift(&config);

        assert!(
            audit
                .findings
                .iter()
                .any(|finding| finding.code == "channel_mismatch")
        );
    }

    #[test]
    fn drift_audit_detects_manual_channel_only_monitor_conflict() {
        let tempdir = tempfile::tempdir().unwrap();
        std::fs::create_dir(tempdir.path().join(".git")).unwrap();
        let config = config_with_route_and_monitors(
            setup_route("clawhip", "123", Some("dev")),
            vec![git_monitor(
                tempdir.path().to_str().unwrap(),
                None,
                Some("123"),
                Some("dev"),
            )],
        );

        let audit = audit_route_monitor_drift(&config);

        assert!(
            audit
                .findings
                .iter()
                .any(|finding| finding.code == "manual_monitor_conflict")
        );
    }

    #[test]
    fn drift_audit_ignores_manual_github_monitor_without_setup_route() {
        let config = AppConfig {
            monitors: MonitorConfig {
                git: GitMonitorConfig {
                    repos: vec![GitRepoMonitor {
                        path: "/work/repo".to_string(),
                        name: Some("repo".to_string()),
                        github_repo: Some("owner/repo".to_string()),
                        ..GitRepoMonitor::default()
                    }],
                },
                ..MonitorConfig::default()
            },
            ..AppConfig::default()
        };

        let audit = audit_route_monitor_drift(&config);

        assert!(audit.ok);
        assert!(audit.findings.is_empty());
    }

    #[test]
    fn drift_audit_ignores_manual_github_monitor_with_route_like_metadata() {
        let config = config_with_route_and_monitors(
            setup_route("owner/repo", "456", Some("ops")),
            vec![GitRepoMonitor {
                path: "/manual/repo".to_string(),
                name: Some("repo".to_string()),
                github_repo: Some("owner/repo".to_string()),
                channel: Some("123".to_string()),
                channel_name: Some("dev".to_string()),
                ..GitRepoMonitor::default()
            }],
        );

        let audit = audit_route_monitor_drift(&config);

        assert!(
            !audit
                .findings
                .iter()
                .any(|finding| finding.code == "repo_identity_mismatch"
                    || finding.monitor_indices == vec![0]),
            "manual monitor must not be audited as setup-owned: {audit:?}"
        );
    }

    #[test]
    fn drift_audit_allows_branch_specific_manual_route_override() {
        let mut manual_filter = BTreeMap::new();
        manual_filter.insert("repo".to_string(), "clawhip".to_string());
        manual_filter.insert("branch".to_string(), "main".to_string());
        let config = config_with_routes(vec![
            setup_route("clawhip", "123", Some("dev")),
            RouteRule {
                event: "git.push".to_string(),
                filter: manual_filter,
                channel: Some("456".to_string()),
                ..RouteRule::default()
            },
        ]);

        let audit = audit_route_monitor_drift(&config);

        assert!(
            !audit
                .findings
                .iter()
                .any(|finding| finding.code == "manual_route_conflict")
        );
    }

    #[test]
    fn drift_audit_redacts_checkout_label_and_json_shape() {
        let tempdir = tempfile::tempdir().unwrap();
        let checkout = tempdir.path().join("secret-checkout");
        std::fs::create_dir(&checkout).unwrap();
        let raw_path = checkout.to_str().unwrap();
        let config = config_with_route_and_monitors(
            setup_route("clawhip", "123", Some("dev")),
            vec![git_monitor(
                raw_path,
                Some("clawhip"),
                Some("123"),
                Some("dev"),
            )],
        );

        let audit = audit_route_monitor_drift(&config);
        let finding = audit
            .findings
            .iter()
            .find(|finding| finding.code == "checkout_not_git_worktree")
            .unwrap();
        let label = finding.checkout_label.as_ref().unwrap();
        let json = serde_json::to_string(&audit).unwrap();
        let text = audit.to_string();

        assert!(label.starts_with("secret-checkout#"));
        assert_eq!(label.len(), "secret-checkout#".len() + 8);
        assert!(json.contains("\"ok\":"));
        assert!(json.contains("\"findings\":"));
        assert!(json.contains("\"checkout_label\":"));
        assert!(!json.contains(raw_path));
        assert!(!text.contains(raw_path));
        assert!(text.contains("Route/monitor drift:"));
        assert!(text.contains("[FAIL]"));
    }
}
