use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::events::IncomingEvent;

const MAX_NATIVE_OBSERVABILITY_GROUPS: usize = 256;
const MAX_NATIVE_OBSERVABILITY_SESSIONS: usize = 256;
const MAX_NATIVE_OBSERVABILITY_SAMPLES: usize = 32;
const MAX_NATIVE_MISMATCHES: usize = 32;

pub type SharedNativeHookObservability = Arc<Mutex<NativeHookObservability>>;

pub fn new_shared_native_hook_observability() -> SharedNativeHookObservability {
    Arc::new(Mutex::new(NativeHookObservability::default()))
}

#[derive(Debug, Default)]
pub struct NativeHookObservability {
    totals: Totals,
    reason_counts: HashMap<String, u64>,
    groups: HashMap<GroupKey, GroupStats>,
    sessions: HashMap<SessionKey, SessionStats>,
    samples: VecDeque<Sample>,
}

#[derive(Debug, Default, Clone)]
struct Totals {
    received: u64,
    normalized: u64,
    dropped: u64,
    deferred: u64,
    routed: u64,
    unresolved: u64,
    route_deliveries: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GroupKey {
    provider: String,
    event_kind: String,
    repo: String,
    repo_path: String,
    worktree_path: String,
    session_id: String,
}

#[derive(Debug, Default, Clone)]
struct GroupStats {
    received: u64,
    normalized: u64,
    dropped: u64,
    deferred: u64,
    routed: u64,
    unresolved: u64,
    route_deliveries: u64,
    last_seen_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionKey {
    provider: String,
    repo: String,
    repo_path: String,
    worktree_path: String,
    session_id: String,
}

#[derive(Debug, Default, Clone)]
struct SessionStats {
    tool_events: u64,
    session_events: u64,
    last_tool_event_unix: Option<u64>,
    last_session_event_unix: Option<u64>,
    first_mismatch_unix: Option<u64>,
    last_seen_unix: u64,
}

#[derive(Debug, Clone)]
struct Sample {
    outcome: &'static str,
    reason: Option<String>,
    key: GroupKey,
    delivery_count: Option<usize>,
    observed_at_unix: u64,
}

impl NativeHookObservability {
    pub fn observe_received_raw(&mut self, payload: &Value) {
        let now = now_unix();
        self.totals.received += 1;
        let key = GroupKey::from_raw(payload);
        let group = self.group_mut(key.clone(), now);
        group.received += 1;
        self.push_sample(Sample {
            outcome: "received",
            reason: None,
            key,
            delivery_count: None,
            observed_at_unix: now,
        });
    }

    pub fn observe_normalized(&mut self, event: &IncomingEvent) {
        let now = now_unix();
        self.totals.normalized += 1;
        let key = GroupKey::from_event(event);
        let group = self.group_mut(key.clone(), now);
        group.normalized += 1;
        self.observe_session_event(&key, event.canonical_kind(), now);
        self.push_sample(Sample {
            outcome: "normalized",
            reason: None,
            key,
            delivery_count: None,
            observed_at_unix: now,
        });
    }

    pub fn observe_dropped_raw(&mut self, payload: &Value, reason: impl Into<String>) {
        let reason = reason.into();
        let now = now_unix();
        self.totals.dropped += 1;
        *self.reason_counts.entry(reason.clone()).or_default() += 1;
        let key = GroupKey::from_raw(payload);
        let group = self.group_mut(key.clone(), now);
        group.dropped += 1;
        self.push_sample(Sample {
            outcome: "dropped",
            reason: Some(reason),
            key,
            delivery_count: None,
            observed_at_unix: now,
        });
    }

    pub fn observe_dropped(&mut self, event: &IncomingEvent, reason: impl Into<String>) {
        let reason = reason.into();
        let now = now_unix();
        self.totals.dropped += 1;
        *self.reason_counts.entry(reason.clone()).or_default() += 1;
        let key = GroupKey::from_event(event);
        let group = self.group_mut(key.clone(), now);
        group.dropped += 1;
        self.push_sample(Sample {
            outcome: "dropped",
            reason: Some(reason),
            key,
            delivery_count: None,
            observed_at_unix: now,
        });
    }

    pub fn observe_deferred(&mut self, event: &IncomingEvent, reason: impl Into<String>) {
        let reason = reason.into();
        let now = now_unix();
        self.totals.deferred += 1;
        *self.reason_counts.entry(reason.clone()).or_default() += 1;
        let key = GroupKey::from_event(event);
        let group = self.group_mut(key.clone(), now);
        group.deferred += 1;
        self.push_sample(Sample {
            outcome: "deferred",
            reason: Some(reason),
            key,
            delivery_count: None,
            observed_at_unix: now,
        });
    }

    pub fn observe_routed(
        &mut self,
        event: &IncomingEvent,
        delivery_count: usize,
        route_kind: &str,
    ) {
        let now = now_unix();
        let unresolved = route_kind == "unresolved";
        if unresolved {
            self.totals.unresolved += 1;
        } else {
            self.totals.routed += 1;
            self.totals.route_deliveries += delivery_count as u64;
        }
        *self
            .reason_counts
            .entry(route_kind.to_string())
            .or_default() += 1;
        let key = GroupKey::from_event(event);
        let group = self.group_mut(key.clone(), now);
        if unresolved {
            group.unresolved += 1;
        } else {
            group.routed += 1;
            group.route_deliveries += delivery_count as u64;
        }
        self.push_sample(Sample {
            outcome: if unresolved { "unresolved" } else { "routed" },
            reason: Some(route_kind.to_string()),
            key,
            delivery_count: Some(delivery_count),
            observed_at_unix: now,
        });
    }

    pub fn snapshot(&self) -> Value {
        let mut groups: Vec<_> = self
            .groups
            .iter()
            .map(|(key, stats)| group_snapshot(key, stats))
            .collect();
        groups.sort_by(|a, b| {
            b.get("last_seen_unix")
                .and_then(Value::as_u64)
                .cmp(&a.get("last_seen_unix").and_then(Value::as_u64))
        });

        let mut mismatches: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, stats)| stats.tool_events > 0 && stats.session_events == 0)
            .map(|(key, stats)| mismatch_snapshot(key, stats))
            .collect();
        mismatches.sort_by(|a, b| {
            b.get("last_seen_unix")
                .and_then(Value::as_u64)
                .cmp(&a.get("last_seen_unix").and_then(Value::as_u64))
        });
        mismatches.truncate(MAX_NATIVE_MISMATCHES);

        json!({
            "totals": {
                "received": self.totals.received,
                "normalized": self.totals.normalized,
                "dropped": self.totals.dropped,
                "deferred": self.totals.deferred,
                "routed": self.totals.routed,
                "unresolved": self.totals.unresolved,
                "route_deliveries": self.totals.route_deliveries,
            },
            "reasons": self.reason_counts,
            "recent_groups": groups,
            "recent_mismatches": mismatches,
            "recent_samples": self.samples.iter().map(sample_snapshot).collect::<Vec<_>>(),
        })
    }

    fn observe_session_event(&mut self, key: &GroupKey, event_kind: &str, now: u64) {
        if !matches!(
            event_kind,
            "tool.pre"
                | "tool.post"
                | "session.started"
                | "session.prompt-submitted"
                | "session.stopped"
        ) {
            return;
        }
        let session_key = SessionKey::from_group(key);
        let stats = self.sessions.entry(session_key).or_default();
        stats.last_seen_unix = now;
        if matches!(event_kind, "tool.pre" | "tool.post") {
            stats.tool_events += 1;
            stats.last_tool_event_unix = Some(now);
            if stats.session_events == 0 && stats.first_mismatch_unix.is_none() {
                stats.first_mismatch_unix = Some(now);
            }
        } else {
            stats.session_events += 1;
            stats.last_session_event_unix = Some(now);
            stats.first_mismatch_unix = None;
        }
        evict_oldest_session_if_needed(&mut self.sessions);
    }

    fn group_mut(&mut self, key: GroupKey, now: u64) -> &mut GroupStats {
        if !self.groups.contains_key(&key) && self.groups.len() >= MAX_NATIVE_OBSERVABILITY_GROUPS {
            evict_oldest_group(&mut self.groups);
        }
        let stats = self.groups.entry(key).or_default();
        stats.last_seen_unix = now;
        stats
    }

    fn push_sample(&mut self, sample: Sample) {
        if self.samples.len() == MAX_NATIVE_OBSERVABILITY_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }
}

pub fn is_native_hook_event(event: &IncomingEvent) -> bool {
    matches!(
        event.canonical_kind(),
        "session.started"
            | "session.prompt-submitted"
            | "session.stopped"
            | "tool.pre"
            | "tool.post"
    ) && event.payload.as_object().is_some_and(|payload| {
        payload.contains_key("provider")
            && (payload.contains_key("hook_event_name")
                || payload.contains_key("event_name")
                || payload.contains_key("normalized_event"))
    })
}

pub fn snapshot_shared(observability: &SharedNativeHookObservability) -> Value {
    observability
        .lock()
        .map(|guard| guard.snapshot())
        .unwrap_or_else(|_| json!({"error": "native hook observability unavailable"}))
}

pub fn with_native_observability(
    observability: &SharedNativeHookObservability,
    update: impl FnOnce(&mut NativeHookObservability),
) {
    if let Ok(mut guard) = observability.lock() {
        update(&mut guard);
    }
}

pub fn native_event_telemetry_fields(event: &IncomingEvent) -> String {
    let key = GroupKey::from_event(event);
    format!(
        "provider={} type={} repo={} session={}",
        key.provider, key.event_kind, key.repo, key.session_id
    )
}

fn group_snapshot(key: &GroupKey, stats: &GroupStats) -> Value {
    json!({
        "provider": key.provider,
        "event_kind": key.event_kind,
        "repo": key.repo,
        "repo_path": key.repo_path,
        "worktree_path": key.worktree_path,
        "session_id": key.session_id,
        "received": stats.received,
        "normalized": stats.normalized,
        "dropped": stats.dropped,
        "deferred": stats.deferred,
        "routed": stats.routed,
        "unresolved": stats.unresolved,
        "route_deliveries": stats.route_deliveries,
        "last_seen_unix": stats.last_seen_unix,
    })
}

fn mismatch_snapshot(key: &SessionKey, stats: &SessionStats) -> Value {
    json!({
        "provider": key.provider,
        "repo": key.repo,
        "repo_path": key.repo_path,
        "worktree_path": key.worktree_path,
        "session_id": key.session_id,
        "tool_events": stats.tool_events,
        "session_events": stats.session_events,
        "first_mismatch_unix": stats.first_mismatch_unix,
        "last_tool_event_unix": stats.last_tool_event_unix,
        "last_seen_unix": stats.last_seen_unix,
        "summary": "tool events observed without session lifecycle events",
    })
}

fn sample_snapshot(sample: &Sample) -> Value {
    json!({
        "outcome": sample.outcome,
        "reason": sample.reason,
        "provider": sample.key.provider,
        "event_kind": sample.key.event_kind,
        "repo": sample.key.repo,
        "session_id": sample.key.session_id,
        "delivery_count": sample.delivery_count,
        "observed_at_unix": sample.observed_at_unix,
    })
}

impl GroupKey {
    fn from_event(event: &IncomingEvent) -> Self {
        let payload = &event.payload;
        Self {
            provider: string_field(payload, &["/provider", "/source", "/tool"])
                .unwrap_or_else(|| "unknown".into()),
            event_kind: event.canonical_kind().to_string(),
            repo: repo_label(payload),
            repo_path: string_field(payload, &["/repo_path"]).unwrap_or_else(|| "unknown".into()),
            worktree_path: string_field(payload, &["/worktree_path"])
                .unwrap_or_else(|| "unknown".into()),
            session_id: string_field(payload, &["/session_id", "/sessionId"])
                .unwrap_or_else(|| "unknown".into()),
        }
    }

    fn from_raw(payload: &Value) -> Self {
        let event_kind = string_field(
            payload,
            &[
                "/event_name",
                "/event",
                "/hook_event_name",
                "/hookEventName",
            ],
        )
        .unwrap_or_else(|| "unknown".into());
        let repo_path = string_field(payload, &["/repo_path", "/context/repo_path"])
            .unwrap_or_else(|| "unknown".into());
        let worktree_path = string_field(
            payload,
            &[
                "/worktree_path",
                "/context/worktree_path",
                "/directory",
                "/cwd",
                "/context/directory",
                "/context/cwd",
            ],
        )
        .unwrap_or_else(|| "unknown".into());
        Self {
            provider: string_field(
                payload,
                &["/provider", "/source/provider", "/context/provider"],
            )
            .unwrap_or_else(|| "unknown".into()),
            event_kind,
            repo: raw_repo_label(payload, &repo_path, &worktree_path),
            repo_path,
            worktree_path,
            session_id: string_field(
                payload,
                &[
                    "/session_id",
                    "/sessionId",
                    "/context/session_id",
                    "/context/sessionId",
                    "/event_payload/session_id",
                    "/event_payload/sessionId",
                ],
            )
            .unwrap_or_else(|| "unknown".into()),
        }
    }
}

impl SessionKey {
    fn from_group(key: &GroupKey) -> Self {
        Self {
            provider: key.provider.clone(),
            repo: key.repo.clone(),
            repo_path: key.repo_path.clone(),
            worktree_path: key.worktree_path.clone(),
            session_id: key.session_id.clone(),
        }
    }
}

fn repo_label(payload: &Value) -> String {
    string_field(payload, &["/repo_name", "/project"])
        .or_else(|| string_field(payload, &["/repo_path"]).and_then(|path| basename(&path)))
        .or_else(|| string_field(payload, &["/worktree_path"]).and_then(|path| basename(&path)))
        .unwrap_or_else(|| "unknown".into())
}

fn raw_repo_label(payload: &Value, repo_path: &str, worktree_path: &str) -> String {
    string_field(
        payload,
        &[
            "/repo_name",
            "/context/repo_name",
            "/project",
            "/project_name",
            "/projectName",
        ],
    )
    .or_else(|| basename(repo_path))
    .or_else(|| basename(worktree_path))
    .unwrap_or_else(|| "unknown".into())
}

fn string_field(payload: &Value, pointers: &[&str]) -> Option<String> {
    pointers.iter().find_map(|pointer| {
        payload
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

fn basename(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "unknown" {
        return None;
    }
    trimmed
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn evict_oldest_group(groups: &mut HashMap<GroupKey, GroupStats>) {
    if let Some(oldest) = groups
        .iter()
        .min_by_key(|(_, stats)| stats.last_seen_unix)
        .map(|(key, _)| key.clone())
    {
        groups.remove(&oldest);
    }
}

fn evict_oldest_session_if_needed(sessions: &mut HashMap<SessionKey, SessionStats>) {
    if sessions.len() <= MAX_NATIVE_OBSERVABILITY_SESSIONS {
        return;
    }
    if let Some(oldest) = sessions
        .iter()
        .min_by_key(|(_, stats)| stats.last_seen_unix)
        .map(|(key, _)| key.clone())
    {
        sessions.remove(&oldest);
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn native_event(kind: &str, session_id: &str) -> IncomingEvent {
        IncomingEvent {
            kind: kind.into(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: json!({
                "provider": "codex",
                "hook_event_name": "PostToolUse",
                "repo_name": "clawhip",
                "repo_path": "/tmp/clawhip",
                "worktree_path": "/tmp/clawhip",
                "session_id": session_id,
                "payload": { "secret": "must-not-appear" }
            }),
        }
    }

    #[test]
    fn records_totals_groups_and_samples_without_raw_payload() {
        let mut obs = NativeHookObservability::default();
        let event = native_event("tool.post", "sess-1");

        obs.observe_received_raw(&json!({
            "provider": "codex",
            "event_name": "PostToolUse",
            "repo_name": "clawhip",
            "session_id": "sess-1"
        }));
        obs.observe_normalized(&event);
        obs.observe_routed(&event, 2, "explicit_route");

        let snapshot = obs.snapshot();
        assert_eq!(snapshot["totals"]["received"], json!(1));
        assert_eq!(snapshot["totals"]["normalized"], json!(1));
        assert_eq!(snapshot["totals"]["routed"], json!(1));
        assert_eq!(snapshot["totals"]["route_deliveries"], json!(2));
        assert_eq!(snapshot["recent_groups"][0]["provider"], json!("codex"));
        assert_eq!(snapshot["recent_groups"][0]["session_id"], json!("sess-1"));
        assert!(!snapshot.to_string().contains("must-not-appear"));
    }

    #[test]
    fn mismatch_appears_for_tool_event_and_clears_after_session_event() {
        let mut obs = NativeHookObservability::default();
        obs.observe_normalized(&native_event("tool.pre", "sess-2"));
        assert_eq!(
            obs.snapshot()["recent_mismatches"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        obs.observe_normalized(&native_event("session.started", "sess-2"));
        assert!(
            obs.snapshot()["recent_mismatches"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn dropped_raw_increments_reason_counter() {
        let mut obs = NativeHookObservability::default();
        obs.observe_dropped_raw(
            &json!({"provider": "codex", "event_name": "Bogus"}),
            "normalization_failed",
        );
        let snapshot = obs.snapshot();
        assert_eq!(snapshot["totals"]["dropped"], json!(1));
        assert_eq!(snapshot["reasons"]["normalization_failed"], json!(1));
    }

    #[test]
    fn group_count_is_bounded() {
        let mut obs = NativeHookObservability::default();
        for index in 0..(MAX_NATIVE_OBSERVABILITY_GROUPS + 10) {
            obs.observe_received_raw(&json!({
                "provider": "codex",
                "event_name": "PostToolUse",
                "repo_name": format!("repo-{index}"),
            }));
        }
        assert_eq!(obs.groups.len(), MAX_NATIVE_OBSERVABILITY_GROUPS);
    }

    #[test]
    fn recognizes_only_provider_native_hook_events() {
        assert!(is_native_hook_event(&native_event(
            "session.started",
            "sess-3"
        )));
        let generic = IncomingEvent {
            kind: "session.started".into(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: json!({"session_id": "sess-3"}),
        };
        assert!(!is_native_hook_event(&generic));
    }
}
