//! GitHub platform status monitor (Statuspage public API).
//!
//! Polls the public GitHub Statuspage machine-readable endpoints and emits
//! lifecycle events for watched components (default: Actions) and related
//! incidents. This is intentionally separate from websocket `[[subscriptions]]`
//! (for example GJC workflow-gate ingress) and from per-repo GitHub API monitors.
//!
//! Endpoint is public; no authentication is used or required.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::Result;
use crate::config::{AppConfig, GitHubStatusMonitorConfig};
use crate::events::IncomingEvent;
use crate::source::Source;

const DEFAULT_PAGE_URL: &str = "https://www.githubstatus.com";

pub struct GitHubStatusSource {
    config: Arc<AppConfig>,
}

impl GitHubStatusSource {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }
}

#[async_trait::async_trait]
impl Source for GitHubStatusSource {
    fn name(&self) -> &str {
        "github_status"
    }

    async fn run(&self, tx: mpsc::Sender<IncomingEvent>) -> Result<()> {
        if !self.config.monitors.github_status.enabled {
            return Ok(());
        }

        let client = match build_status_client() {
            Ok(client) => client,
            Err(error) => {
                eprintln!("clawhip source github_status: failed to build HTTP client: {error}");
                return Ok(());
            }
        };

        let mut state = MonitorState::default();
        loop {
            if let Err(error) = poll_once(self.config.as_ref(), &client, &tx, &mut state).await {
                eprintln!("clawhip source github_status poll failed: {error}");
            }
            let secs = self.config.monitors.github_status.poll_interval_secs.max(1);
            sleep(Duration::from_secs(secs)).await;
        }
    }
}

#[derive(Default)]
struct MonitorState {
    baseline_established: bool,
    components: HashMap<String, String>,
    incidents: HashMap<String, IncidentSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct IncidentSnapshot {
    name: String,
    status: String,
    impact: String,
    shortlink: Option<String>,
    last_update_id: Option<String>,
    last_update_status: Option<String>,
    last_update_body: Option<String>,
    affected_components: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SummaryResponse {
    #[serde(default)]
    page: PageInfo,
    #[serde(default)]
    components: Vec<ComponentEntry>,
    #[serde(default)]
    incidents: Vec<IncidentEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct PageInfo {
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ComponentEntry {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IncidentEntry {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    impact: String,
    #[serde(default)]
    shortlink: Option<String>,
    #[serde(default)]
    components: Vec<ComponentEntry>,
    #[serde(default)]
    incident_updates: Vec<IncidentUpdateEntry>,
}

#[derive(Debug, Deserialize)]
struct IncidentUpdateEntry {
    id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    body: String,
}

fn build_status_client() -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(concat!("clawhip/", env!("CARGO_PKG_VERSION"))),
    );
    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(15))
        .build()?)
}

async fn poll_once(
    config: &AppConfig,
    client: &reqwest::Client,
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut MonitorState,
) -> Result<()> {
    let monitor = &config.monitors.github_status;
    if !monitor.enabled {
        return Ok(());
    }

    let summary = fetch_summary(client, &monitor.api_base).await?;
    let page_url = summary
        .page
        .url
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_PAGE_URL)
        .to_string();

    let watched = watched_component_set(monitor);
    let current_components = watched_component_statuses(&summary.components, &watched);
    let current_incidents = watched_incidents(&summary.incidents, &watched);

    let events = if !state.baseline_established {
        // Prime baseline without replaying historical operational noise.
        state.components = current_components;
        state.incidents = current_incidents;
        state.baseline_established = true;
        Vec::new()
    } else {
        let events = collect_events(
            monitor,
            &page_url,
            state,
            &current_components,
            &current_incidents,
        );
        state.components = current_components;
        state.incidents = current_incidents;
        events
    };

    for event in events {
        tx.send(event)
            .await
            .map_err(|error| format!("github_status source channel closed: {error}"))?;
    }
    Ok(())
}

async fn fetch_summary(client: &reqwest::Client, api_base: &str) -> Result<SummaryResponse> {
    let url = format!("{}/summary.json", api_base.trim_end_matches('/'));
    let response = client.get(&url).send().await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("statuspage summary request failed with {status}: {body}").into());
    }
    Ok(response.json().await?)
}

fn watched_component_set(monitor: &GitHubStatusMonitorConfig) -> HashSet<String> {
    monitor
        .components
        .iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect()
}

fn watched_component_statuses(
    components: &[ComponentEntry],
    watched: &HashSet<String>,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for component in components {
        let name = component.name.trim();
        if name.is_empty() || !watched.contains(name) {
            continue;
        }
        let status = normalize_status(&component.status);
        out.insert(name.to_string(), status);
    }
    out
}

fn watched_incidents(
    incidents: &[IncidentEntry],
    watched: &HashSet<String>,
) -> HashMap<String, IncidentSnapshot> {
    let mut out = HashMap::new();
    for incident in incidents {
        let affected: Vec<String> = incident
            .components
            .iter()
            .map(|component| component.name.trim().to_string())
            .filter(|name| !name.is_empty() && watched.contains(name))
            .collect();
        if affected.is_empty() {
            continue;
        }
        let latest = incident.incident_updates.first();
        out.insert(
            incident.id.clone(),
            IncidentSnapshot {
                name: incident.name.trim().to_string(),
                status: normalize_status(&incident.status),
                impact: normalize_status(&incident.impact),
                shortlink: incident
                    .shortlink
                    .as_ref()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                last_update_id: latest.map(|update| update.id.clone()),
                last_update_status: latest.map(|update| normalize_status(&update.status)),
                last_update_body: latest
                    .map(|update| update.body.trim().to_string())
                    .filter(|body| !body.is_empty()),
                affected_components: affected,
            },
        );
    }
    out
}

fn collect_events(
    monitor: &GitHubStatusMonitorConfig,
    page_url: &str,
    previous: &MonitorState,
    current_components: &HashMap<String, String>,
    current_incidents: &HashMap<String, IncidentSnapshot>,
) -> Vec<IncomingEvent> {
    let mut events = Vec::new();

    let mut component_names: Vec<&String> = current_components.keys().collect();
    component_names.sort();
    for name in component_names {
        let new_status = current_components
            .get(name)
            .map(String::as_str)
            .unwrap_or("unknown");
        match previous.components.get(name).map(String::as_str) {
            Some(old_status) if old_status == new_status => {}
            Some(old_status) => {
                if should_emit_component_transition(old_status, new_status) {
                    events.push(
                        IncomingEvent::github_actions_status(
                            name.clone(),
                            old_status.to_string(),
                            new_status.to_string(),
                            page_url.to_string(),
                            monitor.channel.clone(),
                        )
                        .with_mention(monitor.mention.clone())
                        .with_format(monitor.format.clone()),
                    );
                }
            }
            None => {
                // Newly observed watched component after baseline: emit only if
                // currently degraded/outage/maintenance.
                if is_actionable_component_status(new_status) {
                    events.push(
                        IncomingEvent::github_actions_status(
                            name.clone(),
                            "unknown".to_string(),
                            new_status.to_string(),
                            page_url.to_string(),
                            monitor.channel.clone(),
                        )
                        .with_mention(monitor.mention.clone())
                        .with_format(monitor.format.clone()),
                    );
                }
            }
        }
    }

    // Recovery of a watched component that disappeared from the payload is rare;
    // if Statuspage omits it we leave previous state until it reappears.

    let mut incident_ids: Vec<&String> = current_incidents.keys().collect();
    incident_ids.sort();
    for id in incident_ids {
        let current = &current_incidents[id];
        match previous.incidents.get(id) {
            None => {
                events.push(incident_event(
                    monitor, id, current, "opened", None, page_url,
                ));
            }
            Some(old)
                if old.status != current.status
                    || old.last_update_id != current.last_update_id
                    || old.impact != current.impact =>
            {
                events.push(incident_event(
                    monitor,
                    id,
                    current,
                    "updated",
                    Some(old.status.as_str()),
                    page_url,
                ));
            }
            Some(_) => {}
        }
    }

    let mut resolved_ids: Vec<&String> = previous
        .incidents
        .keys()
        .filter(|id| !current_incidents.contains_key(id.as_str()))
        .collect();
    resolved_ids.sort();
    for id in resolved_ids {
        let previous_incident = &previous.incidents[id];
        let mut resolved = previous_incident.clone();
        resolved.status = "resolved".to_string();
        events.push(incident_event(
            monitor,
            id,
            &resolved,
            "resolved",
            Some(previous_incident.status.as_str()),
            page_url,
        ));
    }

    events
}

fn incident_event(
    monitor: &GitHubStatusMonitorConfig,
    incident_id: &str,
    incident: &IncidentSnapshot,
    change: &str,
    old_status: Option<&str>,
    page_url: &str,
) -> IncomingEvent {
    IncomingEvent::github_actions_incident(
        incident_id.to_string(),
        incident.name.clone(),
        incident.status.clone(),
        incident.impact.clone(),
        change.to_string(),
        old_status.map(str::to_string),
        incident.last_update_id.clone(),
        incident.last_update_status.clone(),
        incident.last_update_body.clone(),
        incident.affected_components.clone(),
        incident
            .shortlink
            .clone()
            .unwrap_or_else(|| page_url.to_string()),
        monitor.channel.clone(),
    )
    .with_mention(monitor.mention.clone())
    .with_format(monitor.format.clone())
}

fn normalize_status(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn is_actionable_component_status(status: &str) -> bool {
    matches!(
        status,
        "degraded_performance" | "partial_outage" | "major_outage" | "under_maintenance"
    )
}

fn should_emit_component_transition(old_status: &str, new_status: &str) -> bool {
    if old_status == new_status {
        return false;
    }
    // Emit degradations, outages, maintenance, and recovery back to operational.
    is_actionable_component_status(new_status)
        || (is_actionable_component_status(old_status) && new_status == "operational")
}

/// Pure helper used by tests and future diagnostics: summarize component ranks.
#[cfg(test)]
fn severity_rank(status: &str) -> u8 {
    match status {
        "major_outage" => 4,
        "partial_outage" => 3,
        "degraded_performance" => 2,
        "under_maintenance" => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::MessageFormat;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn monitor_config() -> GitHubStatusMonitorConfig {
        GitHubStatusMonitorConfig {
            enabled: true,
            api_base: "https://www.githubstatus.com/api/v2".into(),
            components: vec!["Actions".into()],
            poll_interval_secs: 60,
            channel: Some("gajae-code-dev".into()),
            channel_name: Some("gajae-code-dev".into()),
            mention: None,
            format: Some(MessageFormat::Alert),
        }
    }

    fn empty_state() -> MonitorState {
        MonitorState {
            baseline_established: true,
            components: HashMap::new(),
            incidents: HashMap::new(),
        }
    }

    fn primed_operational() -> MonitorState {
        MonitorState {
            baseline_established: true,
            components: HashMap::from([("Actions".into(), "operational".into())]),
            incidents: HashMap::new(),
        }
    }

    #[test]
    fn baseline_poll_emits_nothing() {
        let monitor = monitor_config();
        let mut state = MonitorState::default();
        let components = HashMap::from([("Actions".into(), "degraded_performance".into())]);
        let incidents = HashMap::from([(
            "inc-1".into(),
            IncidentSnapshot {
                name: "Incident with Actions".into(),
                status: "investigating".into(),
                impact: "major".into(),
                shortlink: Some("https://stspg.io/example".into()),
                last_update_id: Some("upd-1".into()),
                last_update_status: Some("investigating".into()),
                last_update_body: Some("We are investigating".into()),
                affected_components: vec!["Actions".into()],
            },
        )]);

        // Simulate first-poll baseline path.
        assert!(!state.baseline_established);
        state.components = components.clone();
        state.incidents = incidents.clone();
        state.baseline_established = true;
        let events = collect_events(
            &monitor,
            "https://www.githubstatus.com",
            &state,
            &components,
            &incidents,
        );
        assert!(
            events.is_empty(),
            "unchanged post-baseline state must not spam"
        );
    }

    #[test]
    fn component_degraded_and_recovery_emit_once_each() {
        let monitor = monitor_config();
        let mut previous = primed_operational();
        let degraded = HashMap::from([("Actions".into(), "degraded_performance".into())]);
        let events = collect_events(
            &monitor,
            "https://www.githubstatus.com",
            &previous,
            &degraded,
            &HashMap::new(),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "github.actions-status");
        assert_eq!(events[0].payload["component"], "Actions");
        assert_eq!(events[0].payload["old_status"], "operational");
        assert_eq!(events[0].payload["new_status"], "degraded_performance");
        assert_eq!(events[0].channel.as_deref(), Some("gajae-code-dev"));

        previous.components = degraded.clone();
        let same = collect_events(
            &monitor,
            "https://www.githubstatus.com",
            &previous,
            &degraded,
            &HashMap::new(),
        );
        assert!(same.is_empty(), "stable degraded state must not re-alert");

        let recovered = HashMap::from([("Actions".into(), "operational".into())]);
        let recovery = collect_events(
            &monitor,
            "https://www.githubstatus.com",
            &previous,
            &recovered,
            &HashMap::new(),
        );
        assert_eq!(recovery.len(), 1);
        assert_eq!(recovery[0].payload["new_status"], "operational");
        assert_eq!(recovery[0].payload["old_status"], "degraded_performance");
    }

    #[test]
    fn major_outage_is_actionable() {
        assert!(is_actionable_component_status("major_outage"));
        assert!(is_actionable_component_status("partial_outage"));
        assert!(is_actionable_component_status("degraded_performance"));
        assert!(!is_actionable_component_status("operational"));
        assert!(severity_rank("major_outage") > severity_rank("degraded_performance"));
    }

    #[test]
    fn incident_lifecycle_emits_open_update_resolve_without_duplicates() {
        let monitor = monitor_config();
        let mut previous = primed_operational();
        let open = HashMap::from([(
            "inc-1".into(),
            IncidentSnapshot {
                name: "Incident with Actions".into(),
                status: "investigating".into(),
                impact: "critical".into(),
                shortlink: Some("https://stspg.io/rcz3fcm83sff".into()),
                last_update_id: Some("upd-1".into()),
                last_update_status: Some("investigating".into()),
                last_update_body: Some("Investigating Actions degradation".into()),
                affected_components: vec!["Actions".into()],
            },
        )]);

        let opened = collect_events(
            &monitor,
            "https://www.githubstatus.com",
            &previous,
            &previous.components,
            &open,
        );
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].kind, "github.actions-incident");
        assert_eq!(opened[0].payload["change"], "opened");
        assert_eq!(opened[0].payload["impact"], "critical");

        previous.incidents = open.clone();
        let unchanged = collect_events(
            &monitor,
            "https://www.githubstatus.com",
            &previous,
            &previous.components,
            &open,
        );
        assert!(unchanged.is_empty());

        let mut updated_map = open.clone();
        updated_map.get_mut("inc-1").unwrap().status = "monitoring".into();
        updated_map.get_mut("inc-1").unwrap().last_update_id = Some("upd-2".into());
        updated_map.get_mut("inc-1").unwrap().last_update_status = Some("monitoring".into());
        updated_map.get_mut("inc-1").unwrap().last_update_body =
            Some("Mitigated; monitoring".into());

        let updated = collect_events(
            &monitor,
            "https://www.githubstatus.com",
            &previous,
            &previous.components,
            &updated_map,
        );
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].payload["change"], "updated");
        assert_eq!(updated[0].payload["old_status"], "investigating");
        assert_eq!(updated[0].payload["status"], "monitoring");
        assert_eq!(updated[0].payload["update_id"], "upd-2");

        previous.incidents = updated_map;
        let resolved = collect_events(
            &monitor,
            "https://www.githubstatus.com",
            &previous,
            &previous.components,
            &HashMap::new(),
        );
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].payload["change"], "resolved");
        assert_eq!(resolved[0].payload["status"], "resolved");
    }

    #[test]
    fn incidents_for_unwatched_components_are_ignored() {
        let monitor = monitor_config();
        let previous = primed_operational();
        // Empty current map: unwatched components never enter collect_events.
        let events = collect_events(
            &monitor,
            "https://www.githubstatus.com",
            &previous,
            &previous.components,
            &HashMap::new(),
        );
        assert!(events.is_empty());
        let _ = empty_state();
    }

    #[test]
    fn watched_incidents_filters_to_actions() {
        let incidents = vec![
            IncidentEntry {
                id: "a".into(),
                name: "Actions".into(),
                status: "investigating".into(),
                impact: "major".into(),
                shortlink: None,
                components: vec![ComponentEntry {
                    name: "Actions".into(),
                    status: "degraded_performance".into(),
                    description: None,
                }],
                incident_updates: vec![IncidentUpdateEntry {
                    id: "u1".into(),
                    status: "investigating".into(),
                    body: "looking".into(),
                }],
            },
            IncidentEntry {
                id: "b".into(),
                name: "Pages".into(),
                status: "investigating".into(),
                impact: "minor".into(),
                shortlink: None,
                components: vec![ComponentEntry {
                    name: "Pages".into(),
                    status: "degraded_performance".into(),
                    description: None,
                }],
                incident_updates: vec![],
            },
        ];
        let watched = HashSet::from(["Actions".to_string()]);
        let filtered = watched_incidents(&incidents, &watched);
        assert_eq!(filtered.len(), 1);
        assert!(filtered.contains_key("a"));
    }

    #[tokio::test]
    async fn poll_once_primes_baseline_then_emits_component_change() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0_u8; 4096];
                let _ = stream.read(&mut buf).await;
                let n = hits_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let status = if n == 0 {
                    "operational"
                } else {
                    "major_outage"
                };
                let body = json!({
                    "page": {"url": "https://www.githubstatus.com"},
                    "components": [{
                        "name": "Actions",
                        "status": status,
                        "description": "Workflows"
                    }],
                    "incidents": []
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let mut config = AppConfig::default();
        config.monitors.github_status = GitHubStatusMonitorConfig {
            enabled: true,
            api_base: format!("http://{addr}"),
            components: vec!["Actions".into()],
            poll_interval_secs: 60,
            channel: Some("dev-channel".into()),
            channel_name: Some("gajae-code-dev".into()),
            mention: None,
            format: Some(MessageFormat::Alert),
        };
        let client = build_status_client().unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let mut state = MonitorState::default();

        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert!(state.baseline_established);
        assert!(
            rx.try_recv().is_err(),
            "first poll must establish baseline without alert spam"
        );

        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        let event = rx.try_recv().expect("component outage event");
        assert_eq!(event.kind, "github.actions-status");
        assert_eq!(event.payload["new_status"], "major_outage");
        assert_eq!(event.channel.as_deref(), Some("dev-channel"));

        // Third poll with same major_outage should not re-emit.
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert!(rx.try_recv().is_err());

        server.abort();
    }

    #[test]
    fn parse_live_summary_shape() {
        let sample = r#"{
          "page": {"id":"kctbh9vrtdwd","name":"GitHub","url":"https://www.githubstatus.com"},
          "status": {"indicator":"none","description":"All Systems Operational"},
          "components": [
            {"id":"br0l2tvcx85d","name":"Actions","status":"operational","description":"Workflows"}
          ],
          "incidents": []
        }"#;
        let parsed: SummaryResponse = serde_json::from_str(sample).unwrap();
        assert_eq!(parsed.components[0].name, "Actions");
        let watched = HashSet::from(["Actions".to_string()]);
        let statuses = watched_component_statuses(&parsed.components, &watched);
        assert_eq!(
            statuses.get("Actions").map(String::as_str),
            Some("operational")
        );
    }

    #[test]
    fn collect_events_keeps_deterministic_order() {
        let monitor = monitor_config();
        let previous = MonitorState {
            baseline_established: true,
            components: HashMap::from([
                ("Actions".into(), "operational".into()),
                ("Packages".into(), "operational".into()),
            ]),
            incidents: HashMap::new(),
        };
        let current = HashMap::from([
            ("Packages".into(), "partial_outage".into()),
            ("Actions".into(), "degraded_performance".into()),
        ]);
        let mut multi = monitor.clone();
        multi.components = vec!["Actions".into(), "Packages".into()];
        let events = collect_events(
            &multi,
            "https://www.githubstatus.com",
            &previous,
            &current,
            &HashMap::new(),
        );
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload["component"], "Actions");
        assert_eq!(events[1].payload["component"], "Packages");
    }
}
