//! Binding verification: audit Discord channel bindings against live server state.
//!
//! Walks the config to collect every channel-ID reference, then queries the
//! Discord API to confirm each channel exists and (optionally) that the live
//! name matches the operator's `channel_name` hint.

use std::fmt;

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
    fn apply_repo_binding_creates_route() {
        let mut config = AppConfig::default();
        config
            .apply_repo_binding("clawhip", "123456", Some("clawhip-dev"))
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
    fn apply_repo_binding_updates_existing() {
        let mut config = AppConfig::default();
        config
            .apply_repo_binding("clawhip", "111", Some("old-name"))
            .unwrap();
        config
            .apply_repo_binding("clawhip", "222", Some("new-name"))
            .unwrap();
        assert_eq!(config.routes.len(), 1);
        assert_eq!(config.routes[0].channel.as_deref(), Some("222"));
        assert_eq!(config.routes[0].channel_name.as_deref(), Some("new-name"));
    }
}
