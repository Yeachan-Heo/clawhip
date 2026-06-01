//! Gateway allowlist coverage diagnostics.
//!
//! This is a local, read-only preflight for the clawhip -> Clawdbot gateway
//! boundary. It compares clawhip's configured Discord channel destinations
//! against the public-safe channel allowlist shape used by Clawdbot:
//!
//! `channels.discord.guilds[*].channels.<channel_id>.allow = true`
//!
//! The report intentionally carries only channel IDs plus clawhip source labels;
//! it never serializes the gateway config, tokens, webhooks, or payload fields.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::binding_verify::{BindingSource, collect_bindings};
use crate::config::AppConfig;

#[derive(Debug, Clone, Serialize)]
pub struct GatewayAllowlistVerdict {
    pub channel_id: String,
    pub source: BindingSource,
    pub label: String,
    pub gateway_status: GatewayAllowlistStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GatewayAllowlistStatus {
    Allowed,
    NotAllowed,
}

impl GatewayAllowlistStatus {
    fn is_ok(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GatewayAllowlistReport {
    pub checked_count: usize,
    pub allowed_count: usize,
    pub missing_count: usize,
    pub verdicts: Vec<GatewayAllowlistVerdict>,
}

impl GatewayAllowlistReport {
    pub fn all_ok(&self) -> bool {
        self.verdicts
            .iter()
            .all(|entry| entry.gateway_status.is_ok())
    }
}

#[derive(Debug, Clone)]
pub struct GatewayAllowlist {
    allowed_channels: BTreeSet<String>,
}

impl GatewayAllowlist {
    pub fn from_json_str(contents: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(contents)
            .map_err(|error| format!("failed to parse gateway config JSON: {error}"))?;
        Ok(Self::from_json_value(&value))
    }

    fn from_json_value(value: &Value) -> Self {
        let mut allowed_channels = BTreeSet::new();
        let Some(guilds) = value
            .get("channels")
            .and_then(|channels| channels.get("discord"))
            .and_then(|discord| discord.get("guilds"))
            .and_then(Value::as_object)
        else {
            return Self { allowed_channels };
        };

        for guild in guilds.values() {
            let Some(channels) = guild.get("channels").and_then(Value::as_object) else {
                continue;
            };
            for (channel_id, channel_config) in channels {
                if channel_config
                    .get("allow")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    allowed_channels.insert(channel_id.clone());
                }
            }
        }

        Self { allowed_channels }
    }

    fn contains(&self, channel_id: &str) -> bool {
        self.allowed_channels.contains(channel_id)
    }
}

pub fn default_gateway_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".clawdbot").join("clawdbot.json"))
}

pub fn verify(config: &AppConfig, gateway: &GatewayAllowlist) -> GatewayAllowlistReport {
    let bindings = collect_bindings(config);
    let mut verdicts = Vec::with_capacity(bindings.len());

    for binding in bindings {
        let gateway_status = if gateway.contains(&binding.channel_id) {
            GatewayAllowlistStatus::Allowed
        } else {
            GatewayAllowlistStatus::NotAllowed
        };
        verdicts.push(GatewayAllowlistVerdict {
            channel_id: binding.channel_id,
            source: binding.source,
            label: binding.label,
            gateway_status,
        });
    }

    let checked_count = verdicts.len();
    let allowed_count = verdicts
        .iter()
        .filter(|entry| entry.gateway_status.is_ok())
        .count();
    let missing_count = checked_count - allowed_count;

    GatewayAllowlistReport {
        checked_count,
        allowed_count,
        missing_count,
        verdicts,
    }
}

pub fn verify_from_path(
    config: &AppConfig,
    gateway_config_path: &Path,
) -> Result<GatewayAllowlistReport, String> {
    let contents = fs::read_to_string(gateway_config_path).map_err(|error| {
        format!(
            "failed to read gateway config {}: {error}. Pass --gateway-config <path> to use a different local Clawdbot config.",
            gateway_config_path.display()
        )
    })?;
    let gateway = GatewayAllowlist::from_json_str(&contents)?;
    Ok(verify(config, &gateway))
}

impl fmt::Display for GatewayAllowlistReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.verdicts.is_empty() {
            writeln!(
                f,
                "No Discord channel destinations found in clawhip config."
            )?;
            return Ok(());
        }

        for entry in &self.verdicts {
            let tag = if entry.gateway_status.is_ok() {
                "ok"
            } else {
                "FAIL"
            };
            let status = match entry.gateway_status {
                GatewayAllowlistStatus::Allowed => "allowed by gateway",
                GatewayAllowlistStatus::NotAllowed => "NOT ALLOWED by gateway",
            };
            writeln!(
                f,
                "[{tag:>4}] {} -> {} ({}) -- {status}",
                entry.source, entry.channel_id, entry.label
            )?;
        }

        writeln!(f)?;
        if self.missing_count == 0 {
            writeln!(
                f,
                "{} gateway allowlist binding(s) checked, all allowed.",
                self.checked_count
            )?;
        } else {
            writeln!(
                f,
                "{} gateway allowlist binding(s) checked: {} allowed, {} missing.",
                self.checked_count, self.allowed_count, self.missing_count
            )?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::config::{
        AppConfig, DefaultsConfig, GitMonitorConfig, GitRepoMonitor, MonitorConfig, RouteRule,
        TmuxMonitorConfig, TmuxSessionMonitor, WorkspaceMonitor,
    };

    fn gateway_json(channel_entries: &str) -> String {
        format!(
            r#"{{
  "token": "gateway-secret-token",
  "privatePayload": {{ "doNotLeak": "private-value" }},
  "channels": {{
    "discord": {{
      "groupPolicy": "allowlist",
      "guilds": {{
        "*": {{
          "channels": {{
            {channel_entries}
          }}
        }}
      }}
    }}
  }}
}}"#
        )
    }

    fn config_with_route(channel: &str) -> AppConfig {
        let mut filter = BTreeMap::new();
        filter.insert("repo".to_string(), "clawhip".to_string());
        AppConfig {
            routes: vec![RouteRule {
                event: "github.*".into(),
                filter,
                channel: Some(channel.into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        }
    }

    #[test]
    fn allowed_channel_reports_ok() {
        let config = config_with_route("111");
        let gateway =
            GatewayAllowlist::from_json_str(&gateway_json(r#""111": { "allow": true }"#)).unwrap();

        let report = verify(&config, &gateway);

        assert!(report.all_ok());
        assert_eq!(report.checked_count, 1);
        assert_eq!(report.allowed_count, 1);
        assert_eq!(report.missing_count, 0);
        assert_eq!(
            report.verdicts[0].gateway_status,
            GatewayAllowlistStatus::Allowed
        );
    }

    #[test]
    fn missing_allowlist_channel_fails() {
        let config = config_with_route("222");
        let gateway =
            GatewayAllowlist::from_json_str(&gateway_json(r#""111": { "allow": true }"#)).unwrap();

        let report = verify(&config, &gateway);

        assert!(!report.all_ok());
        assert_eq!(report.checked_count, 1);
        assert_eq!(report.allowed_count, 0);
        assert_eq!(report.missing_count, 1);
        assert_eq!(
            report.verdicts[0].gateway_status,
            GatewayAllowlistStatus::NotAllowed
        );
    }

    #[test]
    fn allow_false_and_missing_allow_are_not_allowed() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("111".into()),
                ..DefaultsConfig::default()
            },
            routes: vec![RouteRule {
                event: "github.*".into(),
                channel: Some("222".into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };
        let gateway = GatewayAllowlist::from_json_str(&gateway_json(
            r#""111": { "allow": false }, "222": { "name": "missing-allow" }"#,
        ))
        .unwrap();

        let report = verify(&config, &gateway);

        assert!(!report.all_ok());
        assert_eq!(report.missing_count, 2);
    }

    #[test]
    fn text_and_json_do_not_dump_gateway_secrets_or_config_payloads() {
        let mut config = config_with_route("333");
        config.routes[0].channel_name = Some("do-not-render-channel-name-hint".into());
        let gateway = GatewayAllowlist::from_json_str(&gateway_json(
            r#""333": { "allow": true, "token": "nested-secret" }"#,
        ))
        .unwrap();
        let report = verify(&config, &gateway);

        let text = report.to_string();
        let json = serde_json::to_string(&report).unwrap();

        for rendered in [text, json] {
            assert!(rendered.contains("333"));
            assert!(rendered.contains("github.*"));
            assert!(!rendered.contains("gateway-secret-token"));
            assert!(!rendered.contains("nested-secret"));
            assert!(!rendered.contains("private-value"));
            assert!(!rendered.contains("privatePayload"));
            assert!(!rendered.contains("groupPolicy"));
            assert!(!rendered.contains("do-not-render-channel-name-hint"));
            assert!(!rendered.contains("expected_name"));
        }
    }

    #[test]
    fn absent_gateway_config_returns_friendly_error() {
        let config = AppConfig::default();
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-clawdbot.json");

        let error = verify_from_path(&config, &missing).unwrap_err();

        assert!(error.contains("failed to read gateway config"));
        assert!(error.contains("--gateway-config <path>"));
    }

    #[test]
    fn route_default_and_monitor_sources_are_reported() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("100".into()),
                ..DefaultsConfig::default()
            },
            routes: vec![RouteRule {
                event: "github.*".into(),
                channel: Some("200".into()),
                ..RouteRule::default()
            }],
            monitors: MonitorConfig {
                git: GitMonitorConfig {
                    repos: vec![GitRepoMonitor {
                        path: "/repo".into(),
                        name: Some("repo-label".into()),
                        channel: Some("300".into()),
                        ..GitRepoMonitor::default()
                    }],
                },
                tmux: TmuxMonitorConfig {
                    sessions: vec![TmuxSessionMonitor {
                        session: "session-label".into(),
                        channel: Some("400".into()),
                        ..TmuxSessionMonitor::default()
                    }],
                },
                workspace: vec![WorkspaceMonitor {
                    path: "/workspace".into(),
                    channel: Some("500".into()),
                    ..WorkspaceMonitor::default()
                }],
                ..MonitorConfig::default()
            },
            ..AppConfig::default()
        };
        let gateway = GatewayAllowlist::from_json_str(&gateway_json(
            r#""100": { "allow": true }, "200": { "allow": true }, "300": { "allow": true }, "400": { "allow": true }, "500": { "allow": true }"#,
        ))
        .unwrap();

        let report = verify(&config, &gateway);
        let text = report.to_string();

        assert!(report.all_ok());
        assert!(text.contains("defaults.channel -> 100"));
        assert!(text.contains("routes[1] -> 200"));
        assert!(text.contains("monitors.git.repos[1] -> 300"));
        assert!(text.contains("monitors.tmux.sessions[1] -> 400"));
        assert!(text.contains("monitors.workspace[1] -> 500"));
        assert!(text.contains("repo-label"));
        assert!(text.contains("tmux:session-label"));
        assert!(text.contains("workspace:/workspace"));
    }

    #[test]
    fn non_discord_targets_and_threads_are_skipped() {
        let config = AppConfig {
            routes: vec![
                RouteRule {
                    event: "discord-webhook".into(),
                    webhook: Some("https://discord.com/api/webhooks/secret".into()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "slack".into(),
                    sink: "slack".into(),
                    slack_webhook: Some("https://hooks.slack.test/secret".into()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "local".into(),
                    sink: "localfile".into(),
                    local_path: Some("/tmp/out".into()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "thread".into(),
                    thread: Some("999".into()),
                    ..RouteRule::default()
                },
            ],
            ..AppConfig::default()
        };
        let gateway = GatewayAllowlist::from_json_str(&gateway_json(""))
            .expect("gateway shape parses even without allowed channels");

        let report = verify(&config, &gateway);

        assert!(report.verdicts.is_empty());
        assert!(report.all_ok());
    }
}
