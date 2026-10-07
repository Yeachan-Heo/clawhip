//! GJC SDK event bridge (#324).
//!
//! Consumes typed, authoritative GJC SDK state snapshots behind a narrow
//! interface and maps state transitions onto stable Clawhip events:
//!
//! - prompt acceptance/progression -> `session.started`, `session.prompt-submitted`
//! - blocked asks / workflow gates -> `workflow.question`, `workflow.gate`
//! - retries                       -> `session.retry-needed`
//! - completion / failure          -> `session.finished`, `session.failed`
//! - model/profile changes         -> `session.model-changed`
//! - owner-endpoint failures       -> `session.endpoint-failed`
//!
//! The bridge owns no transport, polling loop, or durable lane state: sibling
//! tracks (#322 transport, #323 control, #325 reconciler) feed snapshots in and
//! enqueue the emitted [`IncomingEvent`]s through the normal daemon pipeline
//! (router -> ledger -> sinks). Transitions are deduped per authoritative
//! session/turn/gate episode, and every emitted event carries a deterministic
//! `event_id` plus `idempotency_key` derived from that identity so the event
//! ledger suppresses replays across restarts.
//!
//! Payloads are whitelist-only and public-safe: bounded summaries, collapsed
//! control characters, and never raw prompts, tokens, or endpoint URLs.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::events::IncomingEvent;

const MAX_ID_CHARS: usize = 128;
const MAX_SUMMARY_CHARS: usize = 240;
const BRIDGE_SOURCE: &str = "gjc-sdk";
const BRIDGE_TOOL: &str = "gjc";

/// Authoritative phase of the observed GJC turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GjcSdkTurnPhase {
    Idle,
    Active,
    WaitingInput,
    Complete,
    Failed,
    Aborted,
}

/// Acceptance/progression state of the last submitted prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GjcSdkPromptStatus {
    Accepted,
    Progressing,
}

/// Kind of operator gate currently blocking the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GjcSdkGateKind {
    /// Ask-user style question that #323 answers through the control plane.
    Ask,
    /// Non-question workflow gate such as an approval barrier.
    Workflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GjcSdkGateStatus {
    Open,
    Resolved,
}

/// Health of the owner SDK endpoint as reported by authoritative evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GjcSdkEndpointHealth {
    Ok,
    Degraded,
    Failed,
}

/// Typed turn slice of an authoritative SDK state snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcSdkTurn {
    pub id: String,
    pub state: GjcSdkTurnPhase,
    #[serde(default)]
    pub prompt_accepted: bool,
    #[serde(default)]
    pub attempt: u64,
    #[serde(default)]
    pub error_summary: Option<String>,
}

/// Typed prompt slice: acceptance evidence for the submitted control command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcSdkPrompt {
    pub command_id: String,
    pub status: GjcSdkPromptStatus,
}

/// Typed gate/question slice with the stable identifiers #323 answers against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcSdkGate {
    pub id: String,
    pub kind: GjcSdkGateKind,
    pub revision: u64,
    pub status: GjcSdkGateStatus,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub workflow_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub options: Vec<String>,
}

/// Typed endpoint-health slice (no URLs, no credentials).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcSdkEndpoint {
    pub health: GjcSdkEndpointHealth,
    #[serde(default)]
    pub detail: Option<String>,
}

/// Typed authoritative GJC SDK state snapshot consumed by the bridge.
///
/// This is the narrow interface sibling tracks implement/push; unknown JSON
/// fields are ignored by deserialization so the contract stays additive.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GjcSdkStateSnapshot {
    pub session_id: String,
    pub revision: u64,
    /// Durable endpoint failure episode identity. This is not a local CAS
    /// revision; callers increment it only when authoritative endpoint health
    /// recovers and fails again so retries retain one event identity.
    #[serde(default)]
    pub endpoint_episode: u64,
    #[serde(default)]
    pub turn: Option<GjcSdkTurn>,
    #[serde(skip)]
    pub turn_present: bool,
    #[serde(default)]
    pub prompt: Option<GjcSdkPrompt>,
    #[serde(default)]
    pub gate: Option<GjcSdkGate>,
    #[serde(skip)]
    pub gate_present: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub endpoint: Option<GjcSdkEndpoint>,
    #[serde(default)]
    pub repo_name: Option<String>,
    #[serde(default)]
    pub repo_path: Option<String>,
    #[serde(default)]
    pub worktree_path: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    /// Internal mapper failure. A malformed authoritative query is carried
    /// as a rejected snapshot so callers that use the historical infallible
    /// mapper cannot accidentally advance a watermark or emit a guessed gate.
    #[serde(skip)]
    pub(crate) mapping_error: Option<String>,
}

/// Result of feeding one snapshot to the bridge.
#[derive(Debug, Clone, Default)]
pub struct BridgeOutcome {
    pub events: Vec<IncomingEvent>,
    pub duplicate: bool,
    pub stale: bool,
}

/// Public-safe bridge counters for observability surfaces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct BridgeStats {
    pub snapshots: u64,
    pub duplicates: u64,
    pub stale: u64,
    pub emitted: u64,
}

#[derive(Debug, Clone, Default)]
struct SessionTrack {
    last_revision: Option<u64>,
    turn_id: Option<String>,
    attempts: u64,
    terminal_emitted_turn: Option<String>,
    prompt_command: Option<String>,
    prompt_turn_id: Option<String>,
    last_prompt_command: Option<String>,
    gate_episode: Option<(String, u64)>,
    model: Option<String>,
    profile: Option<String>,
    endpoint_health: Option<GjcSdkEndpointHealth>,
    endpoint_alerted: bool,
    endpoint_episode: u64,
    session_started_emitted: bool,
    in_flight_turn: Option<String>,
    unexpected_idle_emitted_turn: Option<String>,
}

/// Per-session transition reducer mapping SDK snapshots onto Clawhip events.
#[derive(Debug, Clone, Default)]
pub struct GjcEventBridge {
    sessions: HashMap<String, SessionTrack>,
    stats: BridgeStats,
}

impl GjcEventBridge {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> BridgeStats {
        self.stats
    }

    /// Feed one authoritative snapshot; returns emitted events plus whether
    /// the snapshot was suppressed as a duplicate or stale/out-of-order input.
    pub fn observe(&mut self, snapshot: &GjcSdkStateSnapshot) -> Result<BridgeOutcome, String> {
        self.stats.snapshots += 1;
        if let Some(error) = snapshot.mapping_error.as_deref() {
            return Err(error.to_string());
        }
        crate::gjc::model::SessionId::new(snapshot.session_id.clone())
            .map_err(|_| "gjc_bridge_invalid_session_id".to_string())?;
        let session_id = sanitize(&snapshot.session_id);
        if session_id.is_empty() || session_id.chars().count() > MAX_ID_CHARS {
            return Err("gjc_bridge_invalid_session_id".to_string());
        }
        if let Some(gate) = &snapshot.gate {
            if !valid_gate_id(&gate.id) {
                return Err("gjc_bridge_invalid_gate_id".to_string());
            }
            if gate.revision == 0 {
                return Err("gjc_bridge_invalid_gate_revision".to_string());
            }
        }
        if let Some(prompt) = &snapshot.prompt {
            crate::gjc::model::CommandId::new(prompt.command_id.clone())
                .map_err(|_| "gjc_bridge_invalid_prompt_id".to_string())?;
            let prompt_id = sanitize(&prompt.command_id);
            if prompt_id.is_empty() || prompt_id.chars().count() > MAX_ID_CHARS {
                return Err("gjc_bridge_invalid_prompt_id".to_string());
            }
        }

        let track = self.sessions.entry(session_id.clone()).or_default();
        if let Some(previous) = track.last_revision {
            if snapshot.revision < previous {
                self.stats.stale += 1;
                return Ok(BridgeOutcome {
                    stale: true,
                    ..BridgeOutcome::default()
                });
            }
            if snapshot.revision == previous {
                self.stats.duplicates += 1;
                return Ok(BridgeOutcome {
                    duplicate: true,
                    ..BridgeOutcome::default()
                });
            }
        }

        let mut events = Vec::new();
        let context = SnapshotContext::new(&session_id, snapshot);

        if let Some(turn) = &snapshot.turn {
            crate::gjc::model::TurnId::new(turn.id.clone())
                .map_err(|_| "gjc_bridge_invalid_turn_id".to_string())?;
            let turn_id = sanitize(&turn.id);
            if turn_id.is_empty() || turn_id.chars().count() > MAX_ID_CHARS {
                return Err("gjc_bridge_invalid_turn_id".to_string());
            }
            if track.turn_id.as_deref() != Some(turn_id.as_str()) {
                track.turn_id = Some(turn_id.clone());
                track.attempts = 0;
                track.terminal_emitted_turn = None;
                track.prompt_command = None;
            }
            let prompt_accepted = turn.prompt_accepted || track.prompt_command.is_some();

            if prompt_accepted && turn.attempt > track.attempts {
                events.push(lifecycle_event(
                    &context,
                    "session.retry-needed",
                    "retry-needed",
                    turn.attempt,
                    Some(format!("retry attempt {}", turn.attempt)),
                    None,
                    &[],
                    &[("attempt", Value::from(turn.attempt))],
                ));
            }
            track.attempts = track.attempts.max(turn.attempt);

            let terminal_already_emitted =
                track.terminal_emitted_turn.as_deref() == Some(turn_id.as_str());
            match turn.state {
                GjcSdkTurnPhase::Active => {
                    track.in_flight_turn = prompt_accepted.then_some(turn_id.clone());
                    track.unexpected_idle_emitted_turn = None;
                    if prompt_accepted && !track.session_started_emitted {
                        events.push(lifecycle_event(
                            &context,
                            "session.started",
                            "started",
                            0,
                            snapshot.summary.clone(),
                            None,
                            &[],
                            &[],
                        ));
                        track.session_started_emitted = true;
                    }
                }
                GjcSdkTurnPhase::Complete | GjcSdkTurnPhase::Failed => {
                    if prompt_accepted && !terminal_already_emitted {
                        let failed = turn.state == GjcSdkTurnPhase::Failed;
                        let kind = if failed {
                            "session.failed"
                        } else {
                            "session.finished"
                        };
                        let status = if failed { "failed" } else { "finished" };
                        let error = failed.then(|| {
                            turn.error_summary
                                .as_deref()
                                .or(snapshot.summary.as_deref())
                                .map(public_failure_summary)
                                .filter(|value| !value.is_empty())
                                .unwrap_or_else(|| "provider turn failed".to_string())
                        });
                        let command_fields = track
                            .prompt_command
                            .as_ref()
                            .map(|command_id| vec![("command_id", Value::from(command_id.clone()))])
                            .unwrap_or_default();
                        events.push(lifecycle_event(
                            &context,
                            kind,
                            status,
                            0,
                            if failed {
                                error.clone()
                            } else {
                                snapshot.summary.clone()
                            },
                            error,
                            &[if failed { "failed" } else { "complete" }],
                            &command_fields,
                        ));
                        track.terminal_emitted_turn = Some(turn_id);
                    }
                    track.in_flight_turn = None;
                }
                GjcSdkTurnPhase::Idle | GjcSdkTurnPhase::Aborted => {
                    if track.in_flight_turn.as_deref() == Some(turn_id.as_str())
                        && track.unexpected_idle_emitted_turn.as_deref() != Some(turn_id.as_str())
                    {
                        let mut error = turn
                            .error_summary
                            .as_deref()
                            .map(public_failure_summary)
                            .unwrap_or_else(|| {
                                "accepted turn became idle without a terminal receipt".to_string()
                            });
                        if turn.state == GjcSdkTurnPhase::Aborted {
                            error =
                                "accepted turn was aborted without a successful terminal receipt"
                                    .to_string();
                        }
                        events.push(lifecycle_event(
                            &context,
                            "session.stalled",
                            "unexpected-idle",
                            0,
                            Some(error.clone()),
                            Some(error),
                            &["unexpected-idle"],
                            &[],
                        ));
                        track.unexpected_idle_emitted_turn = Some(turn_id.clone());
                    }
                    track.in_flight_turn = None;
                }
                GjcSdkTurnPhase::WaitingInput => {
                    if prompt_accepted && track.in_flight_turn.is_none() {
                        track.in_flight_turn = Some(turn_id);
                    }
                }
            }
        }

        if snapshot.turn_present
            && snapshot.turn.is_none()
            && let Some(turn_id) = track.in_flight_turn.take()
            && track.unexpected_idle_emitted_turn.as_deref() != Some(turn_id.as_str())
        {
            let message = "accepted turn disappeared without a terminal receipt".to_string();
            events.push(lifecycle_event(
                &context,
                "session.stalled",
                "unexpected-idle",
                0,
                Some(message.clone()),
                Some(message),
                &[turn_id.as_str(), "unexpected-idle"],
                &[],
            ));
            track.unexpected_idle_emitted_turn = Some(turn_id);
        }

        if let Some(prompt) = &snapshot.prompt {
            let command_id = sanitize(&prompt.command_id);
            let current_turn_id = snapshot.turn.as_ref().map(|turn| turn.id.as_str());
            if !command_id.is_empty()
                && prompt.status == GjcSdkPromptStatus::Accepted
                && snapshot.turn.as_ref().is_some_and(|turn| {
                    matches!(
                        turn.state,
                        GjcSdkTurnPhase::Active | GjcSdkTurnPhase::WaitingInput
                    )
                })
                && (track.last_prompt_command.as_deref() != Some(command_id.as_str())
                    || track.prompt_turn_id.as_deref() == current_turn_id)
                && track.prompt_command.as_deref() != Some(command_id.as_str())
            {
                events.push(lifecycle_event(
                    &context,
                    "session.prompt-submitted",
                    "prompt-submitted",
                    0,
                    snapshot.summary.clone(),
                    None,
                    &[command_id.as_str()],
                    &[("command_id", Value::from(command_id.as_str()))],
                ));
                track.prompt_command = Some(command_id);
                track.prompt_turn_id = current_turn_id.map(str::to_string);
                track.last_prompt_command = track.prompt_command.clone();
            }
        }

        if let Some(gate) = &snapshot.gate {
            let gate_id = gate.id.clone();
            match gate.status {
                GjcSdkGateStatus::Open => {
                    let new_episode = match &track.gate_episode {
                        Some((tracked_id, tracked_revision)) => {
                            gate_id != *tracked_id || gate.revision > *tracked_revision
                        }
                        None => true,
                    };
                    if new_episode {
                        let kind = match gate.kind {
                            GjcSdkGateKind::Ask => "workflow.question",
                            GjcSdkGateKind::Workflow => "workflow.gate",
                        };
                        events.push(gate_event(&context, kind, gate));
                        track.gate_episode = Some((gate_id, gate.revision));
                    }
                }
                GjcSdkGateStatus::Resolved => {
                    let same_or_newer = match &track.gate_episode {
                        Some((tracked_id, tracked_revision)) => {
                            gate_id != *tracked_id || gate.revision >= *tracked_revision
                        }
                        None => true,
                    };
                    if same_or_newer {
                        track.gate_episode = Some((gate_id, gate.revision));
                    }
                }
            }
        }

        if let Some(event) =
            model_change_event(&context, &mut track.model, &mut track.profile, snapshot)
        {
            events.push(event);
        }

        if let Some(endpoint) = &snapshot.endpoint {
            track.endpoint_episode = track.endpoint_episode.max(snapshot.endpoint_episode);
            let escalated = track.endpoint_health == Some(GjcSdkEndpointHealth::Degraded)
                && endpoint.health == GjcSdkEndpointHealth::Failed;
            match endpoint.health {
                GjcSdkEndpointHealth::Ok => track.endpoint_alerted = false,
                GjcSdkEndpointHealth::Degraded | GjcSdkEndpointHealth::Failed => {
                    if !track.endpoint_alerted || escalated {
                        if !track.endpoint_alerted
                            && (track.endpoint_health.is_none()
                                || track.endpoint_health == Some(GjcSdkEndpointHealth::Ok))
                        {
                            track.endpoint_episode =
                                track.endpoint_episode.saturating_add(1).max(1);
                        }
                        events.push(endpoint_failed_event(
                            &context,
                            endpoint,
                            track.endpoint_episode,
                        ));
                        track.endpoint_alerted = true;
                    }
                }
            }
            track.endpoint_health = Some(endpoint.health);
        }

        track.last_revision = Some(snapshot.revision);
        self.stats.emitted += events.len() as u64;
        Ok(BridgeOutcome {
            events,
            duplicate: false,
            stale: false,
        })
    }
}

/// Map an authoritative SDK response payload — the `payload` value returned by
/// the #322 typed websocket response envelope (`SdkResponse.payload`) — into
/// input. Unknown fields are ignored (additive contract); malformed state or a
/// missing session identifier fails closed.
pub fn snapshot_from_response_payload(payload: &Value) -> Result<GjcSdkStateSnapshot, String> {
    if !payload.is_object() {
        return Err("gjc_bridge_snapshot_payload_not_object".to_string());
    }
    let mut snapshot: GjcSdkStateSnapshot =
        serde_json::from_value(payload.clone()).map_err(|error| {
            format!(
                "gjc_bridge_snapshot_malformed: {}",
                sanitize(&error.to_string())
            )
        })?;
    if snapshot.session_id.trim().is_empty() {
        return Err("gjc_bridge_snapshot_missing_session_id".to_string());
    }
    crate::gjc::model::SessionId::new(snapshot.session_id.clone())
        .map_err(|_| "gjc_bridge_invalid_session_id".to_string())?;
    snapshot.turn_present = payload.get("turn").is_some();
    snapshot.gate_present = payload.get("gate").is_some();
    if snapshot
        .turn
        .as_ref()
        .is_some_and(|turn| turn.prompt_accepted)
        && snapshot
            .prompt
            .as_ref()
            .is_some_and(|prompt| prompt.status != GjcSdkPromptStatus::Accepted)
    {
        return Err("gjc_bridge_contradictory_prompt_acceptance".to_string());
    }
    Ok(snapshot)
}

/// Git routing identity attached to snapshots built from the control plane.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GjcSnapshotIdentity {
    pub repo_name: Option<String>,
    pub repo_path: Option<String>,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
}

/// Map an authoritative #323 control-plane session query into the bridge's
/// typed snapshot input. `revision` is caller-supplied monotonic lane state
/// (the reconciler's store revision); git routing identity comes from lane
/// registration because the control-plane model does not carry it. Gate
/// revisions are derived from authoritative `raised_at` timestamps so
/// re-raised gates form new dedupe episodes. Queries with ambiguous ready
/// gates or malformed gate identity are carried as rejected snapshots rather
/// than projected into guessed state.
pub fn snapshot_from_session_query(
    session_id: &str,
    revision: u64,
    query: &crate::gjc::model::SessionQuery,
    identity: &GjcSnapshotIdentity,
) -> GjcSdkStateSnapshot {
    let gates = query.workflow_gates.as_deref().unwrap_or_default();
    let ready_count = gates
        .iter()
        .filter(|gate| gate.state == crate::gjc::model::WorkflowGateState::Ready)
        .count();
    if ready_count > 1 {
        return rejected_snapshot(
            session_id,
            revision,
            identity,
            "gjc_bridge_ambiguous_workflow_gates",
        );
    }
    if gates
        .iter()
        .any(|gate| !valid_gate_id(&gate.gate_id) || gate_revision(gate) == 0)
    {
        return rejected_snapshot(
            session_id,
            revision,
            identity,
            "gjc_bridge_invalid_gate_identity",
        );
    }

    let turn = query.turn.as_ref().map(|turn| GjcSdkTurn {
        id: turn.turn_id.clone(),
        state: match turn.status {
            crate::gjc::model::GjcPromptStatus::Queued => GjcSdkTurnPhase::Active,
            crate::gjc::model::GjcPromptStatus::Running => GjcSdkTurnPhase::Active,
            crate::gjc::model::GjcPromptStatus::Succeeded => GjcSdkTurnPhase::Complete,
            crate::gjc::model::GjcPromptStatus::Failed => GjcSdkTurnPhase::Failed,
            crate::gjc::model::GjcPromptStatus::Aborted => GjcSdkTurnPhase::Idle,
        },
        attempt: 0,
        prompt_accepted: turn.prompt_accepted,
        error_summary: matches!(
            turn.status,
            crate::gjc::model::GjcPromptStatus::Failed
                | crate::gjc::model::GjcPromptStatus::Aborted
        )
        .then(|| {
            turn.outcome
                .as_ref()
                .and_then(|outcome| outcome.summary.clone())
        })
        .flatten(),
    });

    let gate = gates
        .iter()
        .find(|gate| gate.state == crate::gjc::model::WorkflowGateState::Ready)
        .map(|gate| (gate, GjcSdkGateStatus::Open))
        .or_else(|| {
            gates
                .iter()
                .max_by_key(|gate| gate_revision(gate))
                .map(|gate| (gate, GjcSdkGateStatus::Resolved))
        })
        .map(|(gate, status)| GjcSdkGate {
            id: gate.gate_id.clone(),
            kind: match gate.kind.as_deref() {
                Some("ask") => GjcSdkGateKind::Ask,
                _ => GjcSdkGateKind::Workflow,
            },
            revision: gate_revision(gate),
            status,
            summary: gate.title.clone(),
            workflow_id: gate.workflow_id.clone(),
            title: gate.title.clone(),
            options: gate.options.clone(),
        });

    GjcSdkStateSnapshot {
        session_id: session_id.to_string(),
        revision,
        endpoint_episode: 0,
        turn,
        turn_present: query.turn_present,
        prompt: None,
        gate,
        gate_present: query.workflow_gates_present,
        model: query
            .model_profile
            .as_ref()
            .map(|model| model.model.clone()),
        profile: query
            .model_profile
            .as_ref()
            .and_then(|model| model.profile.clone()),
        endpoint: None,
        repo_name: identity.repo_name.clone(),
        repo_path: identity.repo_path.clone(),
        worktree_path: identity.worktree_path.clone(),
        branch: identity.branch.clone(),
        observed_at: query
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.last_active_at.clone()),
        summary: query
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.title.clone()),
        mapping_error: None,
    }
}

fn rejected_snapshot(
    session_id: &str,
    revision: u64,
    identity: &GjcSnapshotIdentity,
    error: &str,
) -> GjcSdkStateSnapshot {
    GjcSdkStateSnapshot {
        session_id: session_id.to_string(),
        revision,
        gate_present: true,
        repo_name: identity.repo_name.clone(),
        repo_path: identity.repo_path.clone(),
        worktree_path: identity.worktree_path.clone(),
        branch: identity.branch.clone(),
        mapping_error: Some(error.to_string()),
        ..GjcSdkStateSnapshot::default()
    }
}

/// Deterministic gate episode revision: nanoseconds of the authoritative
/// `raised_at` timestamp. A missing or malformed timestamp is invalid rather
/// than replaced with a locally invented revision.
pub(crate) fn gate_revision(gate: &crate::gjc::model::WorkflowGate) -> u64 {
    gate.raised_at
        .as_deref()
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .and_then(|time| u64::try_from(time.unix_timestamp_nanos()).ok())
        .filter(|revision| *revision > 0)
        .unwrap_or(0)
}

pub(crate) fn valid_gate_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_CHARS
        && value
            .chars()
            .all(|character| !character.is_control() && !character.is_whitespace())
}

struct SnapshotContext<'a> {
    session_id: &'a str,
    snapshot: &'a GjcSdkStateSnapshot,
    timestamp: String,
}

impl<'a> SnapshotContext<'a> {
    fn new(session_id: &'a str, snapshot: &'a GjcSdkStateSnapshot) -> Self {
        let timestamp = snapshot
            .observed_at
            .as_deref()
            .and_then(|value| OffsetDateTime::parse(value.trim(), &Rfc3339).ok())
            .and_then(|value| value.format(&Rfc3339).ok())
            .unwrap_or_else(rfc3339_now);
        Self {
            session_id,
            snapshot,
            timestamp,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn lifecycle_event(
    context: &SnapshotContext<'_>,
    kind: &str,
    status: &str,
    attempt: u64,
    summary: Option<String>,
    error_message: Option<String>,
    identity_parts: &[&str],
    extra_fields: &[(&str, Value)],
) -> IncomingEvent {
    let attempt_component = attempt.to_string();
    let mut event_identity = Vec::with_capacity(identity_parts.len() + usize::from(attempt > 0));
    if attempt > 0 {
        event_identity.push(attempt_component.as_str());
    }
    event_identity.extend_from_slice(identity_parts);
    let mut object = base_object(context, kind, status, &event_identity);
    if attempt > 0 {
        object.insert("attempt".to_string(), Value::from(attempt));
    }
    insert_summary(&mut object, summary);
    if let Some(error_message) = error_message {
        object.insert("error_message".to_string(), Value::from(error_message));
    }
    for (key, value) in extra_fields {
        object.insert((*key).to_string(), value.clone());
    }
    finish_event(kind, object)
}

fn gate_event(context: &SnapshotContext<'_>, kind: &str, gate: &GjcSdkGate) -> IncomingEvent {
    let gate_id = sanitize(&gate.id);
    let gate_kind = match gate.kind {
        GjcSdkGateKind::Ask => "ask",
        GjcSdkGateKind::Workflow => "workflow",
    };
    let summary = gate
        .summary
        .as_deref()
        .map(public_text)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| match gate.kind {
            GjcSdkGateKind::Ask => "operator input requested".to_string(),
            GjcSdkGateKind::Workflow => "workflow gate requires approval".to_string(),
        });

    // No top-level `status` field: normalization derives `normalized_event`
    // from it and a "blocked" status would remap `workflow.question` /
    // `workflow.gate` onto `session.blocked` on the second normalize pass.
    let mut object = base_object(context, kind, "", &[]);
    object.insert(
        "question".to_string(),
        json!({
            "id": gate_id,
            "kind": gate_kind,
            "revision": gate.revision,
            "summary": summary.clone(),
            "workflow_id": gate.workflow_id,
            "title": gate.title,
            "options": gate.options.iter().map(|option| public_text(option)).collect::<Vec<_>>(),
        }),
    );
    object.insert("question_id".to_string(), Value::from(gate_id.clone()));
    object.insert("question_summary".to_string(), Value::from(summary.clone()));
    object.insert("gate_kind".to_string(), Value::from(gate_kind));
    object.insert("gate_revision".to_string(), Value::from(gate.revision));
    if let Some(turn) = &context.snapshot.turn {
        let turn_id = sanitize(&turn.id);
        if !turn_id.is_empty() {
            object.insert("turn_id".to_string(), Value::from(turn_id));
        }
    }
    if let Some(prompt) = &context.snapshot.prompt {
        let command_id = sanitize(&prompt.command_id);
        if !command_id.is_empty() {
            object.insert("command_id".to_string(), Value::from(command_id));
        }
    }
    object.insert("summary".to_string(), Value::from(summary));
    finish_event(kind, object)
}

fn endpoint_failed_event(
    context: &SnapshotContext<'_>,
    endpoint: &GjcSdkEndpoint,
    endpoint_episode: u64,
) -> IncomingEvent {
    let endpoint_episode = endpoint_episode
        .max(context.snapshot.endpoint_episode)
        .max(1);
    let health = match endpoint.health {
        GjcSdkEndpointHealth::Ok => "ok",
        GjcSdkEndpointHealth::Degraded => "degraded",
        GjcSdkEndpointHealth::Failed => "failed",
    };
    let detail = endpoint
        .detail
        .as_deref()
        .map(public_endpoint_detail)
        .unwrap_or_else(|| "sdk endpoint unavailable".to_string());
    let mut object = base_object(
        context,
        "session.endpoint-failed",
        "endpoint-failed",
        &[health, &format!("episode-{endpoint_episode}")],
    );
    object.insert("endpoint_health".to_string(), Value::from(health));
    object.insert("error_message".to_string(), Value::from(detail.clone()));
    object.insert(
        "summary".to_string(),
        Value::from(format!("endpoint {health}: {detail}")),
    );
    finish_event("session.endpoint-failed", object)
}

fn model_change_event(
    context: &SnapshotContext<'_>,
    tracked_model: &mut Option<String>,
    tracked_profile: &mut Option<String>,
    snapshot: &GjcSdkStateSnapshot,
) -> Option<IncomingEvent> {
    let model = snapshot
        .model
        .as_deref()
        .map(sanitize)
        .filter(|value| !value.is_empty());
    let profile = snapshot
        .profile
        .as_deref()
        .map(sanitize)
        .filter(|value| !value.is_empty());

    if tracked_model.is_none() && tracked_profile.is_none() {
        *tracked_model = model.clone();
        *tracked_profile = profile.clone();
        return None;
    }
    if model == *tracked_model && profile == *tracked_profile {
        return None;
    }

    let previous_model = tracked_model.as_deref().unwrap_or("unset").to_string();
    let previous_profile = tracked_profile.as_deref().unwrap_or("unset").to_string();
    let current_model = model.as_deref().unwrap_or("unset").to_string();
    let current_profile = profile.as_deref().unwrap_or("unset").to_string();
    let mut parts = Vec::new();
    if model != *tracked_model {
        parts.push(format!(
            "model {} -> {}",
            tracked_model.clone().unwrap_or_else(|| "unset".to_string()),
            model.clone().unwrap_or_else(|| "unset".to_string())
        ));
    }
    if profile != *tracked_profile {
        parts.push(format!(
            "profile {} -> {}",
            tracked_profile
                .clone()
                .unwrap_or_else(|| "unset".to_string()),
            profile.clone().unwrap_or_else(|| "unset".to_string())
        ));
    }
    let mut object = base_object(
        context,
        "session.model-changed",
        "model-changed",
        &[
            previous_model.as_str(),
            current_model.as_str(),
            previous_profile.as_str(),
            current_profile.as_str(),
        ],
    );
    if let Some(model) = &model {
        object.insert("model".to_string(), Value::from(model.clone()));
    }
    if let Some(profile) = &profile {
        object.insert("profile".to_string(), Value::from(profile.clone()));
    }
    object.insert("summary".to_string(), Value::from(parts.join(", ")));
    *tracked_model = model;
    *tracked_profile = profile;
    Some(finish_event("session.model-changed", object))
}

fn base_object(
    context: &SnapshotContext<'_>,
    kind: &str,
    status: &str,
    identity_parts: &[&str],
) -> Map<String, Value> {
    let mut object = Map::new();
    object.insert("tool".to_string(), Value::from(BRIDGE_TOOL));
    object.insert("provider".to_string(), Value::from(BRIDGE_TOOL));
    object.insert("source".to_string(), Value::from(BRIDGE_SOURCE));
    object.insert("agent_name".to_string(), Value::from(BRIDGE_TOOL));
    if !status.is_empty() {
        object.insert("status".to_string(), Value::from(status));
    }
    object.insert("session_id".to_string(), Value::from(context.session_id));
    object.insert(
        "sdk_revision".to_string(),
        Value::from(context.snapshot.revision),
    );
    for (key, value) in [
        ("repo_name", &context.snapshot.repo_name),
        ("repo_path", &context.snapshot.repo_path),
        ("worktree_path", &context.snapshot.worktree_path),
        ("branch", &context.snapshot.branch),
    ] {
        if let Some(value) = value
            .as_deref()
            .map(sanitize)
            .filter(|value| !value.is_empty())
        {
            object.insert(key.to_string(), Value::from(value));
        }
    }
    object.insert(
        "event_timestamp".to_string(),
        Value::from(context.timestamp.clone()),
    );

    let turn = context
        .snapshot
        .turn
        .as_ref()
        .map(|turn| sanitize(&turn.id))
        .unwrap_or_default();
    if !turn.is_empty() {
        object.insert("turn_id".to_string(), Value::from(turn.clone()));
    }
    let command = context
        .snapshot
        .prompt
        .as_ref()
        .map(|prompt| sanitize(&prompt.command_id))
        .unwrap_or_default();
    if !command.is_empty() {
        object.insert("command_id".to_string(), Value::from(command.clone()));
    }
    for (key, value) in [
        ("model", context.snapshot.model.as_deref()),
        ("profile", context.snapshot.profile.as_deref()),
    ] {
        if let Some(value) = value.map(public_text).filter(|value| !value.is_empty()) {
            object.insert(key.to_string(), Value::from(value));
        }
    }
    let gate = context.snapshot.gate.as_ref();
    let gate_component = gate
        .map(|gate| format!("{}|{}", sanitize(&gate.id), gate.revision))
        .unwrap_or_default();
    let routing_identity = [
        context.snapshot.repo_name.as_deref().unwrap_or_default(),
        context.snapshot.repo_path.as_deref().unwrap_or_default(),
        context
            .snapshot
            .worktree_path
            .as_deref()
            .unwrap_or_default(),
        context.snapshot.branch.as_deref().unwrap_or_default(),
    ]
    .into_iter()
    .map(sanitize)
    .collect::<Vec<_>>()
    .join("|");
    let identity = format!(
        "{}|{}|{}|{}|{}|{}",
        kind,
        context.session_id,
        turn,
        gate_component,
        routing_identity,
        identity_parts.join("|"),
    );
    let event_id = format!(
        "gjc-{}-{:016x}",
        kind.replace('.', "-"),
        fnv1a64(identity.as_bytes())
    );
    object.insert("event_id".to_string(), Value::from(event_id.clone()));
    object.insert(
        "correlation_id".to_string(),
        Value::from(format!("gjc-{}", context.session_id)),
    );
    object.insert("idempotency_key".to_string(), Value::from(event_id));
    object
}

fn finish_event(kind: &str, object: Map<String, Value>) -> IncomingEvent {
    IncomingEvent {
        kind: kind.to_string(),
        channel: None,
        mention: None,
        format: None,
        template: None,
        payload: Value::Object(object),
    }
}

fn insert_summary(object: &mut Map<String, Value>, summary: Option<String>) {
    if let Some(summary) = summary
        .as_deref()
        .map(public_text)
        .filter(|value| !value.is_empty())
    {
        object.insert("summary".to_string(), Value::from(summary));
    }
}

fn sanitize(value: &str) -> String {
    let mut collapsed = String::with_capacity(value.len());
    let mut previous_space = false;
    for character in value.chars() {
        let mapped = if character.is_control() || character.is_whitespace() {
            ' '
        } else {
            character
        };
        if mapped == ' ' && previous_space {
            continue;
        }
        previous_space = mapped == ' ';
        collapsed.push(mapped);
    }
    collapsed.trim().chars().take(MAX_SUMMARY_CHARS).collect()
}

fn public_text(value: &str) -> String {
    let sanitized = sanitize(value);
    let lower = sanitized.to_ascii_lowercase();
    let compact: String = lower.chars().filter(char::is_ascii_alphanumeric).collect();
    if lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("api_key")
        || lower.contains("api-key")
        || lower.contains("authorization")
        || lower.contains("apikey")
        || lower.contains("credential")
        || lower.contains("passwd")
        || lower.contains("auth:")
        || compact.contains("privatekey")
        || lower.contains("auth=")
        || lower.contains("bearer")
        || lower.contains("://")
    {
        "details redacted".to_string()
    } else {
        sanitized
    }
}

fn public_failure_summary(value: &str) -> String {
    let normalized = sanitize(value).to_ascii_lowercase();
    if normalized.contains("401") || normalized.contains("unauthorized") {
        return "provider authorization failed (401)".to_string();
    }
    if normalized.contains("402") && !normalized.contains("403")
        || (normalized.contains("payment")
            || normalized.contains("billing")
            || normalized.contains("credit"))
            && !normalized.contains("403")
    {
        return "provider credits or billing unavailable (402)".to_string();
    }
    if normalized.contains("403") || normalized.contains("forbidden") {
        return "provider access denied (403)".to_string();
    }
    if normalized.contains("429")
        || normalized.contains("rate limit")
        || normalized.contains("quota")
    {
        return "provider rate limit or quota exceeded (429)".to_string();
    }
    "provider turn failed".to_string()
}

fn public_endpoint_detail(_value: &str) -> String {
    "sdk endpoint unavailable".to_string()
}

fn rfc3339_now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LedgerConfig;
    use crate::event::compat::from_incoming_event;
    use crate::events::MessageFormat;
    use crate::ledger::{AppendOutcome, EventLedger};
    use crate::render::DefaultRenderer;

    fn base_snapshot(session: &str, revision: u64) -> GjcSdkStateSnapshot {
        GjcSdkStateSnapshot {
            session_id: session.to_string(),
            revision,
            repo_name: Some("Yeachan-Heo/clawhip".to_string()),
            worktree_path: Some("/wt/issue-324".to_string()),
            branch: Some("feat/issue-324".to_string()),
            observed_at: Some("2026-08-23T00:00:00Z".to_string()),
            summary: Some("lane progressing".to_string()),
            ..GjcSdkStateSnapshot::default()
        }
    }

    fn turn(id: &str, state: GjcSdkTurnPhase, attempt: u64) -> GjcSdkTurn {
        GjcSdkTurn {
            id: id.to_string(),
            state,
            prompt_accepted: true,
            attempt,
            error_summary: None,
        }
    }

    fn kinds(events: &[IncomingEvent]) -> Vec<String> {
        events.iter().map(|event| event.kind.clone()).collect()
    }

    #[test]
    fn full_lifecycle_progression_emits_stable_kinds() {
        let mut bridge = GjcEventBridge::new();

        let mut snapshot = base_snapshot("sess-1", 1);
        snapshot.turn = Some(turn("t1", GjcSdkTurnPhase::Active, 0));
        snapshot.prompt = Some(GjcSdkPrompt {
            command_id: "c1".to_string(),
            status: GjcSdkPromptStatus::Accepted,
        });
        let outcome = bridge.observe(&snapshot).unwrap();
        let first = kinds(&outcome.events);
        assert!(first.contains(&"session.started".to_string()), "{first:?}");
        assert!(
            first.contains(&"session.prompt-submitted".to_string()),
            "{first:?}"
        );

        let outcome = bridge.observe(&snapshot).unwrap();
        assert!(outcome.duplicate);
        assert!(outcome.events.is_empty());

        snapshot.revision = 2;
        snapshot.turn = Some(turn("t1", GjcSdkTurnPhase::Complete, 0));
        let outcome = bridge.observe(&snapshot).unwrap();
        assert_eq!(kinds(&outcome.events), vec!["session.finished".to_string()]);

        snapshot.revision = 3;
        snapshot.turn = Some(GjcSdkTurn {
            id: "t2".to_string(),
            state: GjcSdkTurnPhase::Failed,
            prompt_accepted: true,
            attempt: 0,
            error_summary: Some("boom".to_string()),
        });
        let outcome = bridge.observe(&snapshot).unwrap();
        assert_eq!(kinds(&outcome.events), vec!["session.failed".to_string()]);
        assert_eq!(
            outcome.events[0].payload["error_message"],
            Value::from("provider turn failed")
        );
        assert_eq!(bridge.stats().emitted, 4);
    }

    #[test]
    fn accepted_turn_becoming_idle_emits_one_unexpected_idle_alert() {
        let mut bridge = GjcEventBridge::new();
        let mut snapshot = base_snapshot("sess-idle", 1);
        snapshot.turn = Some(turn("turn-1", GjcSdkTurnPhase::Active, 0));
        bridge.observe(&snapshot).unwrap();

        snapshot.revision = 2;
        snapshot.turn = Some(turn("turn-1", GjcSdkTurnPhase::Idle, 0));
        let outcome = bridge.observe(&snapshot).unwrap();
        assert_eq!(kinds(&outcome.events), vec!["session.stalled"]);
        assert_eq!(outcome.events[0].payload["status"], "unexpected-idle");

        snapshot.revision = 3;
        assert!(bridge.observe(&snapshot).unwrap().events.is_empty());
    }

    #[test]
    fn provider_credit_and_auth_failures_are_public_safe() {
        let mut bridge = GjcEventBridge::new();
        for (revision, detail, expected) in [
            (
                1,
                "provider HTTP 403: credit card token=secret",
                "provider access denied (403)",
            ),
            (
                2,
                "provider quota exceeded",
                "provider rate limit or quota exceeded (429)",
            ),
        ] {
            let mut snapshot = base_snapshot("sess-provider", revision);
            snapshot.turn = Some(GjcSdkTurn {
                id: format!("turn-{revision}"),
                state: GjcSdkTurnPhase::Failed,
                prompt_accepted: true,
                attempt: 0,
                error_summary: Some(detail.to_string()),
            });
            let events = bridge.observe(&snapshot).unwrap().events;
            assert_eq!(events[0].payload["error_message"], expected);
            assert!(!events[0].payload.to_string().contains("secret"));
        }
    }

    #[test]
    fn lower_and_equal_revisions_are_suppressed() {
        let mut bridge = GjcEventBridge::new();

        let mut snapshot = base_snapshot("sess-1", 5);
        snapshot.turn = Some(turn("t1", GjcSdkTurnPhase::Active, 0));
        bridge.observe(&snapshot).unwrap();

        snapshot.revision = 4;
        snapshot.turn = Some(turn("t1", GjcSdkTurnPhase::Complete, 0));
        let outcome = bridge.observe(&snapshot).unwrap();
        assert!(outcome.stale);
        assert!(outcome.events.is_empty());

        snapshot.revision = 6;
        snapshot.model = Some("model-b".to_string());
        bridge.observe(&snapshot).unwrap();

        // Out-of-order replay of a different content at an already-seen
        // revision is treated as duplicate, never re-emitted.
        snapshot.revision = 6;
        snapshot.model = Some("model-a".to_string());
        let outcome = bridge.observe(&snapshot).unwrap();
        assert!(outcome.duplicate);
        assert!(outcome.events.is_empty());
        assert_eq!(bridge.stats().stale, 1);
        assert_eq!(bridge.stats().duplicates, 1);
    }

    #[test]
    fn retries_emit_once_per_attempt_value() {
        let mut bridge = GjcEventBridge::new();
        let mut snapshot = base_snapshot("sess-1", 1);
        snapshot.turn = Some(turn("t1", GjcSdkTurnPhase::Active, 0));
        bridge.observe(&snapshot).unwrap();

        snapshot.revision = 2;
        snapshot.turn = Some(turn("t1", GjcSdkTurnPhase::Active, 2));
        let outcome = bridge.observe(&snapshot).unwrap();
        assert_eq!(
            kinds(&outcome.events),
            vec!["session.retry-needed".to_string()]
        );
        assert_eq!(outcome.events[0].payload["attempt"], Value::from(2));

        snapshot.revision = 3;
        snapshot.turn = Some(turn("t1", GjcSdkTurnPhase::Active, 2));
        let outcome = bridge.observe(&snapshot).unwrap();
        assert!(outcome.events.is_empty());
    }

    #[test]
    fn question_event_carries_answer_identifiers_for_control_plane() {
        let mut bridge = GjcEventBridge::new();
        let mut snapshot = base_snapshot("sess-1", 10);
        snapshot.turn = Some(turn("t1", GjcSdkTurnPhase::WaitingInput, 0));
        snapshot.prompt = Some(GjcSdkPrompt {
            command_id: "c9".to_string(),
            status: GjcSdkPromptStatus::Accepted,
        });
        snapshot.gate = Some(GjcSdkGate {
            id: "q-1".to_string(),
            kind: GjcSdkGateKind::Ask,
            revision: 3,
            status: GjcSdkGateStatus::Open,
            summary: Some("Approve the deploy?\nsecond line".to_string()),
            workflow_id: None,
            title: None,
            options: Vec::new(),
        });

        let outcome = bridge.observe(&snapshot).unwrap();
        let emitted = kinds(&outcome.events);
        assert!(
            emitted.contains(&"workflow.question".to_string()),
            "{emitted:?}"
        );
        assert!(
            emitted.contains(&"session.prompt-submitted".to_string()),
            "{emitted:?}"
        );
        let question = outcome
            .events
            .iter()
            .find(|event| event.kind == "workflow.question")
            .unwrap();
        let payload = &question.payload;
        assert_eq!(payload["question"]["id"], Value::from("q-1"));
        assert_eq!(payload["question"]["kind"], Value::from("ask"));
        assert_eq!(payload["question"]["revision"], Value::from(3));
        assert_eq!(
            payload["question_summary"],
            Value::from("Approve the deploy? second line")
        );
        assert_eq!(payload["turn_id"], Value::from("t1"));
        assert_eq!(payload["command_id"], Value::from("c9"));
        assert_eq!(payload["gate_revision"], Value::from(3));
        assert_eq!(payload.get("status"), None);
        assert_eq!(payload["repo_name"], Value::from("Yeachan-Heo/clawhip"));
        assert_eq!(payload["correlation_id"], Value::from("gjc-sess-1"));
        let event_id = payload["event_id"].as_str().unwrap().to_string();
        assert_eq!(payload["idempotency_key"].as_str(), Some(event_id.as_str()));

        // Same gate revision observed again at a higher session revision stays quiet.
        snapshot.revision = 11;
        let outcome = bridge.observe(&snapshot).unwrap();
        assert!(outcome.events.is_empty());

        // Resolution clears the episode; reopening at a higher gate revision re-alerts.
        snapshot.revision = 12;
        snapshot.gate.as_mut().unwrap().status = GjcSdkGateStatus::Resolved;
        let outcome = bridge.observe(&snapshot).unwrap();
        assert!(outcome.events.is_empty());

        snapshot.revision = 13;
        snapshot.gate = Some(GjcSdkGate {
            id: "q-2".to_string(),
            kind: GjcSdkGateKind::Ask,
            revision: 7,
            status: GjcSdkGateStatus::Open,
            summary: None,
            workflow_id: None,
            title: None,
            options: Vec::new(),
        });
        let outcome = bridge.observe(&snapshot).unwrap();
        assert_eq!(
            kinds(&outcome.events),
            vec!["workflow.question".to_string()]
        );
        assert_eq!(
            outcome.events[0].payload["question_summary"],
            Value::from("operator input requested")
        );
    }

    #[test]
    fn workflow_gates_emit_the_gate_route_key() {
        let mut bridge = GjcEventBridge::new();
        let mut snapshot = base_snapshot("sess-1", 20);
        snapshot.gate = Some(GjcSdkGate {
            id: "gate-7".to_string(),
            kind: GjcSdkGateKind::Workflow,
            revision: 2,
            status: GjcSdkGateStatus::Open,
            summary: None,
            workflow_id: None,
            title: None,
            options: Vec::new(),
        });
        let outcome = bridge.observe(&snapshot).unwrap();
        assert_eq!(kinds(&outcome.events), vec!["workflow.gate".to_string()]);
        assert_eq!(
            outcome.events[0].payload["gate_kind"],
            Value::from("workflow")
        );
    }

    #[test]
    fn model_changes_baseline_silently_then_announce_transitions() {
        let mut bridge = GjcEventBridge::new();
        let mut snapshot = base_snapshot("sess-1", 30);
        snapshot.model = Some("model-a".to_string());
        snapshot.profile = Some("profile-x".to_string());
        let outcome = bridge.observe(&snapshot).unwrap();
        assert!(outcome.events.is_empty());

        snapshot.revision = 31;
        snapshot.model = Some("model-b".to_string());
        let outcome = bridge.observe(&snapshot).unwrap();
        assert_eq!(
            kinds(&outcome.events),
            vec!["session.model-changed".to_string()]
        );
        assert_eq!(outcome.events[0].payload["model"], Value::from("model-b"));
        assert_eq!(
            outcome.events[0].payload["summary"],
            Value::from("model model-a -> model-b")
        );

        snapshot.revision = 32;
        snapshot.profile = Some("profile-y".to_string());
        let outcome = bridge.observe(&snapshot).unwrap();
        assert_eq!(
            kinds(&outcome.events),
            vec!["session.model-changed".to_string()]
        );
        assert!(
            outcome.events[0].payload["summary"]
                .as_str()
                .unwrap()
                .contains("profile profile-x -> profile-y")
        );

        snapshot.revision = 33;
        let outcome = bridge.observe(&snapshot).unwrap();
        assert!(outcome.events.is_empty());
    }

    #[test]
    fn endpoint_episodes_alert_once_until_recovery() {
        let mut bridge = GjcEventBridge::new();
        let endpoint = |health| GjcSdkEndpoint {
            health,
            detail: Some("connect refused".to_string()),
        };

        let mut snapshot = base_snapshot("sess-1", 40);
        snapshot.endpoint = Some(endpoint(GjcSdkEndpointHealth::Ok));
        assert!(bridge.observe(&snapshot).unwrap().events.is_empty());

        snapshot.revision = 41;
        snapshot.endpoint = Some(endpoint(GjcSdkEndpointHealth::Degraded));
        let outcome = bridge.observe(&snapshot).unwrap();
        assert_eq!(
            kinds(&outcome.events),
            vec!["session.endpoint-failed".to_string()]
        );
        assert_eq!(
            outcome.events[0].payload["endpoint_health"],
            Value::from("degraded")
        );
        assert_eq!(
            outcome.events[0].payload["error_message"],
            Value::from("sdk endpoint unavailable")
        );

        snapshot.revision = 42;
        snapshot.endpoint = Some(endpoint(GjcSdkEndpointHealth::Degraded));
        assert!(bridge.observe(&snapshot).unwrap().events.is_empty());

        snapshot.revision = 43;
        snapshot.endpoint = Some(endpoint(GjcSdkEndpointHealth::Failed));
        let outcome = bridge.observe(&snapshot).unwrap();
        assert_eq!(
            kinds(&outcome.events),
            vec!["session.endpoint-failed".to_string()]
        );

        snapshot.revision = 44;
        snapshot.endpoint = Some(endpoint(GjcSdkEndpointHealth::Failed));
        assert!(bridge.observe(&snapshot).unwrap().events.is_empty());

        snapshot.revision = 45;
        snapshot.endpoint = Some(endpoint(GjcSdkEndpointHealth::Ok));
        assert!(bridge.observe(&snapshot).unwrap().events.is_empty());

        snapshot.revision = 46;
        snapshot.endpoint = Some(endpoint(GjcSdkEndpointHealth::Failed));
        let outcome = bridge.observe(&snapshot).unwrap();
        assert_eq!(
            kinds(&outcome.events),
            vec!["session.endpoint-failed".to_string()]
        );
    }

    #[test]
    fn alert_event_ids_ignore_local_snapshot_revisions() {
        let mut first = base_snapshot("stable-id", 10);
        first.turn = Some(turn("turn-1", GjcSdkTurnPhase::Failed, 0));
        let mut second = first.clone();
        second.revision = 11_000;
        let left = GjcEventBridge::new().observe(&first).unwrap().events;
        let right = GjcEventBridge::new().observe(&second).unwrap().events;
        assert_eq!(left.len(), 1);
        assert_eq!(right.len(), 1);
        assert_eq!(left[0].payload["event_id"], right[0].payload["event_id"]);

        let mut annotated = second.clone();
        annotated.prompt = Some(GjcSdkPrompt {
            command_id: "command-correlated".to_string(),
            status: GjcSdkPromptStatus::Accepted,
        });
        let annotated = GjcEventBridge::new().observe(&annotated).unwrap().events;
        let annotated_failure = annotated
            .iter()
            .find(|event| event.kind == "session.failed")
            .expect("failure event");
        assert_eq!(
            left[0].payload["event_id"],
            annotated_failure.payload["event_id"]
        );
        assert_eq!(
            annotated_failure.payload["command_id"],
            "command-correlated"
        );

        let mut prompt_a = base_snapshot("stable-prompt", 1);
        prompt_a.turn = Some(turn("turn-1", GjcSdkTurnPhase::Active, 0));
        prompt_a.prompt = Some(GjcSdkPrompt {
            command_id: "command-1".to_string(),
            status: GjcSdkPromptStatus::Accepted,
        });
        let mut prompt_b = prompt_a.clone();
        prompt_b.revision = 99;
        let left = GjcEventBridge::new().observe(&prompt_a).unwrap().events;
        let right = GjcEventBridge::new().observe(&prompt_b).unwrap().events;
        let left = left
            .iter()
            .find(|event| event.kind == "session.prompt-submitted")
            .unwrap();
        let right = right
            .iter()
            .find(|event| event.kind == "session.prompt-submitted")
            .unwrap();
        assert_eq!(left.payload["event_id"], right.payload["event_id"]);
    }

    #[test]
    fn restart_produces_identical_event_ids_for_ledger_dedupe() {
        let feed = || {
            let mut snapshots = Vec::new();
            let mut snapshot = base_snapshot("sess-1", 1);
            snapshot.turn = Some(turn("t1", GjcSdkTurnPhase::Active, 0));
            snapshot.prompt = Some(GjcSdkPrompt {
                command_id: "c1".to_string(),
                status: GjcSdkPromptStatus::Accepted,
            });
            snapshots.push(snapshot.clone());
            snapshot.revision = 2;
            snapshot.prompt = None;
            snapshot.gate = Some(GjcSdkGate {
                id: "q-1".to_string(),
                kind: GjcSdkGateKind::Ask,
                revision: 4,
                status: GjcSdkGateStatus::Open,
                summary: Some("Proceed?".to_string()),
                workflow_id: None,
                title: None,
                options: Vec::new(),
            });
            snapshots.push(snapshot.clone());
            snapshot.revision = 3;
            snapshot.gate.as_mut().unwrap().status = GjcSdkGateStatus::Resolved;
            snapshot.turn = Some(turn("t1", GjcSdkTurnPhase::Complete, 0));
            snapshots.push(snapshot);
            snapshots
        };

        let run = || {
            let mut bridge = GjcEventBridge::new();
            let mut ids = Vec::new();
            for snapshot in feed() {
                for event in bridge.observe(&snapshot).unwrap().events {
                    ids.push(event.payload["event_id"].as_str().unwrap().to_string());
                }
            }
            ids
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn payloads_are_whitelist_only_and_public_safe() {
        const ALLOWED: &[&str] = &[
            "tool",
            "provider",
            "source",
            "agent_name",
            "status",
            "session_id",
            "repo_name",
            "repo_path",
            "worktree_path",
            "branch",
            "event_timestamp",
            "event_id",
            "correlation_id",
            "idempotency_key",
            "summary",
            "error_message",
            "attempt",
            "command_id",
            "turn_id",
            "sdk_revision",
            "question",
            "question_id",
            "question_summary",
            "gate_kind",
            "gate_revision",
            "model",
            "profile",
            "endpoint_health",
        ];

        let mut bridge = GjcEventBridge::new();
        let mut snapshot = base_snapshot("sess-1", 1);
        snapshot.summary = Some(format!(
            "{} secret-token-value",
            "x".repeat(MAX_SUMMARY_CHARS + 80)
        ));
        snapshot.turn = Some(GjcSdkTurn {
            id: "t1".to_string(),
            state: GjcSdkTurnPhase::Failed,
            prompt_accepted: true,
            attempt: 0,
            error_summary: Some("raw\r\nerror\twith control chars".to_string()),
        });
        let outcome = bridge.observe(&snapshot).unwrap();
        assert!(!outcome.events.is_empty());
        for event in &outcome.events {
            let object = event.payload.as_object().unwrap();
            for key in object.keys() {
                assert!(
                    ALLOWED.contains(&key.as_str()),
                    "unexpected payload key {key}"
                );
            }
            if let Some(summary) = object.get("summary").and_then(Value::as_str) {
                assert!(summary.chars().count() <= MAX_SUMMARY_CHARS);
                assert!(!summary.contains('\n'));
                assert!(!summary.contains("secret-token-value"));
            }
            if let Some(error) = object.get("error_message").and_then(Value::as_str) {
                assert_eq!(error, "provider turn failed");
            }
        }
    }

    #[test]
    fn ledger_accepts_bridge_events_and_dedupes_restart_replays() {
        let temp = tempfile::TempDir::new().unwrap();
        let config = LedgerConfig {
            enabled: true,
            path: Some(temp.path().join("ledger")),
            ..LedgerConfig::default()
        };
        let mut ledger = EventLedger::open(config, temp.path()).unwrap();

        let mut bridge = GjcEventBridge::new();
        let mut snapshots = Vec::new();
        let mut snapshot = base_snapshot("sess-ledger", 1);
        snapshot.turn = Some(turn("t1", GjcSdkTurnPhase::Active, 0));
        snapshots.push(snapshot.clone());
        snapshot.revision = 2;
        snapshot.gate = Some(GjcSdkGate {
            id: "q-1".to_string(),
            kind: GjcSdkGateKind::Ask,
            revision: 1,
            status: GjcSdkGateStatus::Open,
            summary: Some("Answer me?".to_string()),
            workflow_id: None,
            title: None,
            options: Vec::new(),
        });
        snapshots.push(snapshot);

        let mut events = Vec::new();
        for item in &snapshots {
            events.extend(bridge.observe(item).unwrap().events);
        }
        assert_eq!(events.len(), 2);
        for event in &events {
            assert!(matches!(ledger.append(event), Ok(AppendOutcome::Appended)));
        }

        // A restarted daemon replays the same authoritative snapshots through a
        // fresh bridge; deterministic identity makes every append a duplicate.
        let mut restarted = GjcEventBridge::new();
        for item in &snapshots {
            for event in restarted.observe(item).unwrap().events {
                assert!(matches!(
                    ledger.append(&event),
                    Ok(AppendOutcome::Duplicate)
                ));
            }
        }
        assert_eq!(ledger.status().records, 2);
    }

    #[test]
    fn emitted_events_convert_through_typed_envelope_contract() {
        let mut bridge = GjcEventBridge::new();
        let mut snapshot = base_snapshot("sess-1", 1);
        snapshot.turn = Some(turn("t1", GjcSdkTurnPhase::Active, 1));
        snapshot.prompt = Some(GjcSdkPrompt {
            command_id: "c1".to_string(),
            status: GjcSdkPromptStatus::Accepted,
        });
        snapshot.gate = Some(GjcSdkGate {
            id: "q-1".to_string(),
            kind: GjcSdkGateKind::Workflow,
            revision: 1,
            status: GjcSdkGateStatus::Open,
            summary: None,
            workflow_id: None,
            title: None,
            options: Vec::new(),
        });
        snapshot.endpoint = Some(GjcSdkEndpoint {
            health: GjcSdkEndpointHealth::Failed,
            detail: None,
        });
        snapshot.model = Some("model-b".to_string());
        snapshot.profile = Some("profile-y".to_string());
        let outcome = bridge.observe(&snapshot).unwrap();
        assert!(outcome.events.len() >= 5);
        for event in &outcome.events {
            from_incoming_event(event).unwrap_or_else(|error| {
                panic!("envelope conversion failed for {}: {error}", event.kind)
            });
        }
    }

    #[test]
    fn invalid_snapshots_fail_closed() {
        let mut bridge = GjcEventBridge::new();
        let mut snapshot = base_snapshot("   ", 1);
        assert_eq!(
            bridge.observe(&snapshot).unwrap_err(),
            "gjc_bridge_invalid_session_id"
        );
        snapshot.session_id = "sess-1".to_string();
        snapshot.turn = Some(turn("", GjcSdkTurnPhase::Active, 0));
        assert_eq!(
            bridge.observe(&snapshot).unwrap_err(),
            "gjc_bridge_invalid_turn_id"
        );
    }

    #[test]
    fn gate_replays_and_reopens_use_gate_revision_not_snapshot_revision() {
        let mut bridge = GjcEventBridge::new();
        let mut snapshot = base_snapshot("gate-replay", 1);
        snapshot.gate = Some(GjcSdkGate {
            id: "gate-1".to_string(),
            kind: GjcSdkGateKind::Workflow,
            revision: 7,
            status: GjcSdkGateStatus::Open,
            summary: None,
            workflow_id: None,
            title: None,
            options: Vec::new(),
        });
        assert_eq!(bridge.observe(&snapshot).unwrap().events.len(), 1);

        // A replay at a newer session revision is still the same gate episode.
        snapshot.revision = 2;
        assert!(bridge.observe(&snapshot).unwrap().events.is_empty());

        // A lower gate revision cannot regress or create a second episode.
        snapshot.revision = 3;
        snapshot.gate.as_mut().unwrap().revision = 6;
        assert!(bridge.observe(&snapshot).unwrap().events.is_empty());

        // Resolution followed by a higher revision on the same id reopens it.
        snapshot.revision = 4;
        snapshot.gate.as_mut().unwrap().revision = 7;
        snapshot.gate.as_mut().unwrap().status = GjcSdkGateStatus::Resolved;
        assert!(bridge.observe(&snapshot).unwrap().events.is_empty());
        snapshot.revision = 5;
        snapshot.gate.as_mut().unwrap().revision = 8;
        snapshot.gate.as_mut().unwrap().status = GjcSdkGateStatus::Open;
        let reopened = bridge.observe(&snapshot).unwrap().events;
        assert_eq!(kinds(&reopened), vec!["workflow.gate"]);
        assert_eq!(reopened[0].payload["gate_revision"], Value::from(8));
    }

    #[test]
    fn plural_ready_gates_are_rejected_before_event_mapping() {
        let gate = |id: &str| crate::gjc::model::WorkflowGate {
            gate_id: id.to_string(),
            kind: None,
            workflow_id: None,
            state: crate::gjc::model::WorkflowGateState::Ready,
            title: Some("Approve merge".to_string()),
            options: Vec::new(),
            raised_at: Some("2026-08-23T10:00:00Z".to_string()),
        };
        let mut query = control_query(crate::gjc::model::GjcPromptStatus::Running);
        query.workflow_gates = Some(vec![gate("gate-1"), gate("gate-2")]);
        let snapshot = snapshot_from_session_query("sess-9", 1, &query, &identity());
        assert_eq!(
            GjcEventBridge::new().observe(&snapshot).unwrap_err(),
            "gjc_bridge_ambiguous_workflow_gates"
        );
    }

    #[test]
    fn malformed_gate_identity_or_revision_is_rejected() {
        let mut bridge = GjcEventBridge::new();
        let mut snapshot = base_snapshot("gate-invalid", 1);
        snapshot.gate = Some(GjcSdkGate {
            id: "gate invalid".to_string(),
            kind: GjcSdkGateKind::Workflow,
            revision: 1,
            status: GjcSdkGateStatus::Open,
            summary: None,
            workflow_id: None,
            title: None,
            options: Vec::new(),
        });
        assert_eq!(
            bridge.observe(&snapshot).unwrap_err(),
            "gjc_bridge_invalid_gate_id"
        );
        snapshot.gate.as_mut().unwrap().id = "gate-1".to_string();
        snapshot.gate.as_mut().unwrap().revision = 0;
        assert_eq!(
            bridge.observe(&snapshot).unwrap_err(),
            "gjc_bridge_invalid_gate_revision"
        );
    }

    #[test]
    fn gate_summaries_redact_credential_aliases() {
        for value in [
            "authorization=secret",
            "apikey: secret",
            "api-key secret",
            "private_key=secret",
            "credential secret",
            "passwd secret",
            "auth: secret",
        ] {
            assert_eq!(public_text(value), "details redacted");
        }
    }

    #[test]
    fn contradictory_prompt_acceptance_fails_closed() {
        let error = snapshot_from_response_payload(&json!({
            "session_id": "sess-1",
            "revision": 1,
            "turn": {
                "id": "turn-1",
                "state": "active",
                "prompt_accepted": true
            },
            "prompt": {"command_id": "turn-1", "status": "progressing"}
        }))
        .unwrap_err();
        assert_eq!(error, "gjc_bridge_contradictory_prompt_acceptance");
    }

    fn control_query(
        status: crate::gjc::model::GjcPromptStatus,
    ) -> crate::gjc::model::SessionQuery {
        crate::gjc::model::SessionQuery {
            revision: None,
            metadata: Some(crate::gjc::model::SessionMetadata {
                session_id: "sess-9".to_string(),
                title: Some("issue 324 lane".to_string()),
                project: None,
                created_at: None,
                last_active_at: Some("2026-08-23T12:00:00Z".to_string()),
                lane: None,
                provider_metadata: Default::default(),
            }),
            stats: None,
            workflow_gates_present: false,
            turn_present: true,
            model_profile: Some(crate::gjc::model::SessionModelProfile {
                model: "model-a".to_string(),
                profile: Some("profile-x".to_string()),
                updated_at: None,
            }),
            turn: Some(crate::gjc::model::SessionTurn {
                turn_id: "t1".to_string(),
                status,
                prompt_accepted: true,
                started_at: None,
                finished_at: None,
                outcome: Some(crate::gjc::model::SessionTurnOutcome {
                    status,
                    summary: Some("terminal detail".to_string()),
                    finished_at: None,
                }),
            }),
            queue: None,
            workflow_gates: None,
            goal_todo: None,
        }
    }

    fn identity() -> GjcSnapshotIdentity {
        GjcSnapshotIdentity {
            repo_name: Some("Yeachan-Heo/clawhip".to_string()),
            worktree_path: Some("/wt/issue-324".to_string()),
            ..GjcSnapshotIdentity::default()
        }
    }

    #[test]
    fn control_plane_queries_map_turn_statuses_and_model_profile() {
        let running = snapshot_from_session_query(
            "sess-9",
            4,
            &control_query(crate::gjc::model::GjcPromptStatus::Running),
            &identity(),
        );
        assert_eq!(running.revision, 4);
        let turn = running.turn.as_ref().unwrap();
        assert_eq!(turn.state, GjcSdkTurnPhase::Active);
        assert_eq!(turn.id, "t1");
        assert_eq!(running.model.as_deref(), Some("model-a"));
        assert_eq!(running.profile.as_deref(), Some("profile-x"));
        assert_eq!(running.summary.as_deref(), Some("issue 324 lane"));
        assert_eq!(running.observed_at.as_deref(), Some("2026-08-23T12:00:00Z"));
        assert_eq!(running.repo_name.as_deref(), Some("Yeachan-Heo/clawhip"));

        for (status, expected) in [
            (
                crate::gjc::model::GjcPromptStatus::Queued,
                GjcSdkTurnPhase::Active,
            ),
            (
                crate::gjc::model::GjcPromptStatus::Succeeded,
                GjcSdkTurnPhase::Complete,
            ),
            (
                crate::gjc::model::GjcPromptStatus::Failed,
                GjcSdkTurnPhase::Failed,
            ),
            (
                crate::gjc::model::GjcPromptStatus::Aborted,
                GjcSdkTurnPhase::Idle,
            ),
        ] {
            let snapshot =
                snapshot_from_session_query("sess-9", 5, &control_query(status), &identity());
            assert_eq!(snapshot.turn.unwrap().state, expected, "{status:?}");
        }

        let failed = snapshot_from_session_query(
            "sess-9",
            6,
            &control_query(crate::gjc::model::GjcPromptStatus::Failed),
            &identity(),
        );
        assert_eq!(
            failed.turn.unwrap().error_summary.as_deref(),
            Some("terminal detail")
        );
    }

    #[test]
    fn control_plane_gate_episodes_open_resolve_and_reopen() {
        let gate = |state: crate::gjc::model::WorkflowGateState,
                    raised_at: &str|
         -> crate::gjc::model::WorkflowGate {
            crate::gjc::model::WorkflowGate {
                gate_id: "gate-1".to_string(),
                kind: Some("ask".to_string()),
                workflow_id: Some("workflow-1".to_string()),
                state,
                title: Some("Approve merge".to_string()),
                options: vec!["approve".to_string(), "reject".to_string()],
                raised_at: Some(raised_at.to_string()),
            }
        };

        // Open episode.
        let mut query = control_query(crate::gjc::model::GjcPromptStatus::Queued);
        query.workflow_gates = Some(vec![gate(
            crate::gjc::model::WorkflowGateState::Ready,
            "2026-08-23T10:00:00Z",
        )]);
        let mut bridge = GjcEventBridge::new();
        let open = snapshot_from_session_query("sess-9", 10, &query, &identity());
        let gate_state = open.gate.as_ref().unwrap();
        assert_eq!(gate_state.status, GjcSdkGateStatus::Open);
        assert_eq!(gate_state.revision, 1_787_479_200_000_000_000);
        let events = bridge.observe(&open).unwrap().events;
        assert!(
            kinds(&events)
                .iter()
                .any(|kind| kind == "workflow.question"),
            "{:?}",
            kinds(&events)
        );
        let gate_event = events
            .iter()
            .find(|event| event.kind == "workflow.question")
            .unwrap();
        // The adapter maps the #323 WorkflowGate concept onto the workflow gate
        // route key; ask-style questions arrive through the ask surface.
        assert_eq!(gate_event.payload["question"]["kind"], Value::from("ask"));
        assert_eq!(gate_event.payload["question"]["workflow_id"], "workflow-1");
        assert_eq!(gate_event.payload["question"]["options"][0], "approve");

        // Same revision again: quiet.
        assert!(bridge.observe(&open).unwrap().events.is_empty());

        // Answered at the same raised_at resolves the episode without an event.
        *query.workflow_gates.as_mut().unwrap() = vec![gate(
            crate::gjc::model::WorkflowGateState::Answered,
            "2026-08-23T10:00:00Z",
        )];
        let resolved = snapshot_from_session_query("sess-9", 11, &query, &identity());
        assert_eq!(
            resolved.gate.as_ref().unwrap().status,
            GjcSdkGateStatus::Resolved
        );
        assert!(bridge.observe(&resolved).unwrap().events.is_empty());

        // Re-raised at a later timestamp forms a new dedupe episode and re-alerts.
        *query.workflow_gates.as_mut().unwrap() = vec![gate(
            crate::gjc::model::WorkflowGateState::Ready,
            "2026-08-23T11:30:00Z",
        )];
        let reopened = snapshot_from_session_query("sess-9", 12, &query, &identity());
        let events = bridge.observe(&reopened).unwrap().events;
        assert!(
            kinds(&events)
                .iter()
                .any(|kind| kind == "workflow.question"),
            "{:?}",
            kinds(&events)
        );
        assert_eq!(
            events[0].payload["gate_revision"],
            Value::from(1_787_484_600_000_000_000u64)
        );
    }

    #[test]
    fn control_plane_queries_round_trip_through_the_bridge_lifecycle() {
        let mut bridge = GjcEventBridge::new();

        let mut query = control_query(crate::gjc::model::GjcPromptStatus::Running);
        let first = snapshot_from_session_query("sess-9", 1, &query, &identity());
        let kinds_first = kinds(&bridge.observe(&first).unwrap().events);
        assert!(
            kinds_first.contains(&"session.started".to_string()),
            "{kinds_first:?}"
        );

        query.turn.as_mut().unwrap().status = crate::gjc::model::GjcPromptStatus::Succeeded;
        let second = snapshot_from_session_query("sess-9", 2, &query, &identity());
        assert_eq!(
            kinds(&bridge.observe(&second).unwrap().events),
            vec!["session.finished".to_string()]
        );

        // Identical replay across fresh bridges produces identical event ids.
        let replay_ids = |bridge: &mut GjcEventBridge| {
            bridge
                .observe(&snapshot_from_session_query(
                    "sess-9",
                    1,
                    &control_query(crate::gjc::model::GjcPromptStatus::Running),
                    &identity(),
                ))
                .unwrap()
                .events
                .iter()
                .map(|event| event.payload["event_id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let left = replay_ids(&mut GjcEventBridge::new());
        let right = replay_ids(&mut GjcEventBridge::new());
        assert_eq!(left, right);
    }

    #[test]
    fn response_payloads_map_into_typed_snapshots() {
        let payload = json!({
            "session_id": "sess-9",
            "revision": 7,
            "turn": {"id": "t1", "state": "active", "attempt": 0},
            "unknown_future_field": {"nested": true},
        });
        let snapshot = snapshot_from_response_payload(&payload).unwrap();
        assert_eq!(snapshot.session_id, "sess-9");
        assert_eq!(snapshot.revision, 7);
        assert_eq!(snapshot.turn.unwrap().state, GjcSdkTurnPhase::Active);

        assert_eq!(
            snapshot_from_response_payload(&Value::Null).unwrap_err(),
            "gjc_bridge_snapshot_payload_not_object"
        );
        assert!(
            snapshot_from_response_payload(&json!({"revision": 1}))
                .unwrap_err()
                .starts_with("gjc_bridge_snapshot_malformed")
        );
        assert!(
            snapshot_from_response_payload(&json!({"session_id": "  ", "revision": 1}))
                .unwrap_err()
                .starts_with("gjc_bridge_snapshot_missing_session_id")
        );
        assert!(
            snapshot_from_response_payload(&json!({
                "session_id": "sess-9",
                "revision": 1,
                "turn": {"id": "t1", "state": "bogus"},
            }))
            .unwrap_err()
            .starts_with("gjc_bridge_snapshot_malformed")
        );
    }

    #[test]
    fn sanitize_bounds_and_collapses_input() {
        assert_eq!(sanitize("a\r\nb\t c"), "a b c");
        let long = "x".repeat(MAX_SUMMARY_CHARS + 50);
        assert_eq!(sanitize(&long).chars().count(), MAX_SUMMARY_CHARS);
        assert_eq!(sanitize("   "), "");
    }

    #[tokio::test]
    async fn question_events_match_setup_owned_route_and_render_for_discord() {
        use crate::config::{AppConfig, RouteRule};
        use crate::render::Renderer as _;
        use crate::router::Router;
        use crate::sink::SinkTarget;
        use std::sync::Arc;

        let mut filter = std::collections::BTreeMap::new();
        filter.insert("repo_name".to_string(), "Yeachan-Heo/clawhip".to_string());
        let config = AppConfig {
            routes: vec![RouteRule {
                event: "workflow.question".into(),
                filter,
                sink: "discord".into(),
                channel: Some("questions".into()),
                format: Some(MessageFormat::Alert),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };

        let mut bridge = GjcEventBridge::new();
        let mut snapshot = base_snapshot("sess-1", 5);
        snapshot.gate = Some(GjcSdkGate {
            id: "q-1".to_string(),
            kind: GjcSdkGateKind::Ask,
            revision: 2,
            status: GjcSdkGateStatus::Open,
            summary: Some("Ship it?".to_string()),
            workflow_id: None,
            title: None,
            options: Vec::new(),
        });
        let outcome = bridge.observe(&snapshot).unwrap();
        let event = &outcome.events[0];
        assert_eq!(event.kind, "workflow.question");

        let router = Router::new(Arc::new(config));
        let delivery = router.preview_delivery(event).await.unwrap();
        assert_eq!(
            delivery.target,
            SinkTarget::DiscordChannel("questions".into())
        );

        let content = DefaultRenderer
            .render(event, &MessageFormat::Alert)
            .unwrap();
        assert!(content.contains("GJC question q-1"), "{content}");
        assert!(content.contains("rev 2"), "{content}");
        assert!(content.contains("Ship it?"), "{content}");
        assert!(
            !content.trim_start().starts_with('{'),
            "raw JSON leaked: {content}"
        );

        let gate_snapshot_revision = snapshot.revision + 1;
        snapshot.revision = gate_snapshot_revision;
        snapshot.gate = Some(GjcSdkGate {
            id: "gate-1".to_string(),
            kind: GjcSdkGateKind::Workflow,
            revision: 1,
            status: GjcSdkGateStatus::Open,
            summary: None,
            workflow_id: None,
            title: None,
            options: Vec::new(),
        });
        let outcome = bridge.observe(&snapshot).unwrap();
        assert_eq!(kinds(&outcome.events), vec!["workflow.gate".to_string()]);
        let content = DefaultRenderer
            .render(&outcome.events[0], &MessageFormat::Compact)
            .unwrap();
        assert!(content.contains("GJC gate gate-1 blocked"), "{content}");
        assert!(
            content.contains("workflow gate requires approval"),
            "{content}"
        );
    }

    #[test]
    fn lifecycle_events_render_through_session_renderer() {
        use crate::render::Renderer as _;

        let mut bridge = GjcEventBridge::new();
        let mut snapshot = base_snapshot("sess-1", 1);
        snapshot.turn = Some(turn("t1", GjcSdkTurnPhase::Active, 0));
        snapshot.prompt = Some(GjcSdkPrompt {
            command_id: "c1".to_string(),
            status: GjcSdkPromptStatus::Accepted,
        });
        let events = bridge.observe(&snapshot).unwrap().events;

        let started = events
            .iter()
            .find(|event| event.kind == "session.started")
            .unwrap();
        let content = DefaultRenderer
            .render(started, &MessageFormat::Compact)
            .unwrap();
        assert!(content.starts_with("gjc sess-1 started"), "{content}");
        assert!(content.contains("repo=Yeachan-Heo/clawhip"), "{content}");

        let submitted = events
            .iter()
            .find(|event| event.kind == "session.prompt-submitted")
            .unwrap();
        let content = DefaultRenderer
            .render(submitted, &MessageFormat::Inline)
            .unwrap();
        assert!(content.contains("prompt-submitted"), "{content}");

        snapshot.revision = 2;
        snapshot.turn = Some(GjcSdkTurn {
            id: "t1".to_string(),
            state: GjcSdkTurnPhase::Failed,
            attempt: 0,
            prompt_accepted: true,
            error_summary: Some("boom".to_string()),
        });
        let failed = &bridge.observe(&snapshot).unwrap().events[0];
        assert_eq!(failed.kind, "session.failed");
        let content = DefaultRenderer
            .render(failed, &MessageFormat::Alert)
            .unwrap();
        assert!(content.contains("gjc sess-1 failed"), "{content}");
        assert!(content.contains("error=provider turn failed"), "{content}");
    }
}
