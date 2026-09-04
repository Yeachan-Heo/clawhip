//! Durable GJC SDK lane ownership and restart reconciliation (#325).
//!
//! Clawhip retains durable ownership records for GJC SDK-backed lanes so that a
//! daemon restart never loses session identity, worktree binding, endpoint
//! generation, last observed revision/turn/gate evidence, ownership state, or
//! terminal disposition. A background reconciler re-queries the authoritative
//! GJC SDK control plane with bounded polling and exponential backoff,
//! classifies each lane strictly from SDK evidence (never tmux pane text),
//! removes terminal ghost watches while preserving audit entries, and
//! reconciles current PR head/base so pushed heads invalidate stale review and
//! owner evidence.
//!
//! Session mutations (prompt, steer, abort, gate answers) belong to the #323
//! control-plane track. This module defines the narrow [`GjcSdkControlPlane`]
//! seam those tracks implement; it performs read-only queries itself.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::mpsc;

use crate::dispatch::{
    AlertDeliveryJournal, AlertDeliveryState, cancel_alert_acceptance,
    clear_alert_delivery_journal, register_alert_acceptance, register_alert_delivery_journal,
};
use crate::events::IncomingEvent;
use crate::gjc_sdk_events::{
    GjcEventBridge, GjcSdkEndpoint, GjcSdkEndpointHealth, GjcSdkGate, GjcSdkGateKind,
    GjcSdkGateStatus, GjcSdkPrompt, GjcSdkPromptStatus, GjcSdkStateSnapshot, GjcSdkTurn,
    GjcSdkTurnPhase, gate_revision, valid_gate_id,
};

pub const GJC_LANE_STATE_SCHEMA: &str = "clawhip.gjc-lane-state.v1";
pub const GJC_LANE_STATUS_EVENT: &str = "gjc.lane.status";
pub const GJC_LANE_STATUS_SCHEMA: &str = "clawhip.gjc-lane-status.v1";
pub const GJC_LANE_HEALTH_SCHEMA: &str = "clawhip.gjc-health.v1";

/// Audit trail is bounded: oldest entries are dropped once this many exist.
pub const MAX_AUDIT_ENTRIES: usize = 512;
/// Pending alerts are retained until the reconciler's send succeeds. Refuse
/// to grow beyond this bound rather than dropping an operator-visible alert.
const MAX_PENDING_ALERTS: usize = 64;
const MAX_PENDING_ALERT_DELIVERIES: usize = 64;
const MAX_LANES: usize = 1024;
const ALERT_ACCEPTANCE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SESSION_LEN: usize = 128;
const MAX_WORKTREE_LEN: usize = 4096;
const MAX_OWNER_LEN: usize = 64;
const MAX_EVIDENCE_LEN: usize = 400;

/// Lifecycle phase classified strictly from authoritative SDK evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GjcLanePhase {
    /// Control plane could not be queried (transport-level, non-authoritative
    /// about the session itself).
    Unavailable,
    /// Session reachable with no running turn and no pending gate.
    Idle,
    /// A turn is actively running.
    Active,
    /// A workflow gate or ask input is awaiting an answer.
    Blocked,
    /// SDK reported the session completed successfully.
    Complete,
    /// SDK reported the session failed.
    Failed,
    /// SDK reported the session retired, or the lane was retired locally.
    Retired,
    /// The SDK authoritatively reports the session/runtime no longer exists.
    RuntimeGone,
}

impl GjcLanePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Idle => "idle",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::Complete => "complete",
            Self::Failed => "failed",
            Self::Retired => "retired",
            Self::RuntimeGone => "runtime-gone",
        }
    }
}

impl fmt::Display for GjcLanePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Turn-level state reported by the SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GjcTurnState {
    Idle,
    Running,
    AwaitingInput,
    Complete,
    Failed,
    Aborted,
}

/// Workflow-gate state reported by the SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GjcGateState {
    Closed,
    Ready,
    Answered,
}

/// Session-level disposition reported by the SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GjcSessionDisposition {
    Live,
    Complete,
    Failed,
    Retired,
}

/// Authoritative observation returned by the GJC SDK control plane (#323 seam).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GjcSdkObservation {
    pub session_id: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub branch_observed: bool,
    pub endpoint_generation: u64,
    pub revision: u64,
    pub turn_state: GjcTurnState,
    pub turn_id: Option<String>,
    pub command_id: Option<String>,
    pub prompt_accepted: bool,
    pub model: Option<String>,
    pub profile: Option<String>,
    pub gate_state: Option<GjcGateState>,
    pub gate_section_present: bool,
    pub gate_id: Option<String>,
    pub gate_revision: u64,
    pub gate_kind: Option<GjcSdkGateKind>,
    pub gate_workflow_id: Option<String>,
    pub gate_title: Option<String>,
    pub gate_options: Vec<String>,
    pub disposition: GjcSessionDisposition,
    pub error_summary: Option<String>,
}

/// Query failure kinds. Only `SessionNotFound` is authoritative about the
/// session's existence; transport errors classify as `unavailable`.
///
/// Variants are constructed by the control-plane implementation (tests today;
/// the repaired-transport track in production), so the non-test binary build
/// legitimately sees them unconstructed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum GjcSdkQueryError {
    EndpointUnavailable(String),
    Timeout(String),
    SessionNotFound(String),
    #[allow(dead_code)] // surfaced by generation-aware control planes; store logic covers it today
    StaleEndpointGeneration {
        observed: u64,
    },
    Ambiguous(String),
    InvalidState(String),
}

impl fmt::Display for GjcSdkQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EndpointUnavailable(detail) => write!(f, "endpoint unavailable: {detail}"),
            Self::Timeout(detail) => write!(f, "control-plane timeout: {detail}"),
            Self::SessionNotFound(detail) => write!(f, "session not found: {detail}"),
            Self::StaleEndpointGeneration { observed } => {
                write!(f, "stale endpoint generation; observed {observed}")
            }
            Self::Ambiguous(detail) => write!(f, "ambiguous response: {detail}"),
            Self::InvalidState(detail) => write!(f, "invalid control-plane state: {detail}"),
        }
    }
}

/// Narrow read-only interface to the GJC SDK control plane.
///
/// #323 owns the authoritative implementation (and all mutation verbs); this
/// seam lets #325's reconciler observe sessions without duplicating sibling
/// transport or control surfaces.
#[async_trait]
pub trait GjcSdkControlPlane: Send + Sync {
    async fn query_lane(
        &self,
        query: &GjcLaneQuery,
    ) -> std::result::Result<GjcSdkObservation, GjcSdkQueryError>;
}

/// Adapter that makes the authoritative #323 control plane usable by the
/// durable #325 reconciler. Keeping this adapter at the seam prevents the
/// reconciler from learning transport or websocket details.
pub struct GjcControlPlaneAdapter {
    registry: crate::gjc::control::SharedGjcCommandRegistry,
}

impl GjcControlPlaneAdapter {
    pub fn new(registry: crate::gjc::control::SharedGjcCommandRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl GjcSdkControlPlane for GjcControlPlaneAdapter {
    async fn query_lane(
        &self,
        query: &GjcLaneQuery,
    ) -> std::result::Result<GjcSdkObservation, GjcSdkQueryError> {
        let session = crate::gjc::model::SessionId::new(query.sdk_session_id.clone())
            .map_err(|_| GjcSdkQueryError::SessionNotFound("invalid session identity".into()))?;
        let worktree =
            query.worktree.as_deref().map(Path::new).ok_or_else(|| {
                GjcSdkQueryError::EndpointUnavailable("worktree unavailable".into())
            })?;
        let endpoint_generation =
            crate::gjc::transport::discover_endpoint_for_session(worktree, &query.sdk_session_id)
                .map_err(map_control_error)?
                .endpoint_generation();
        if query.known_endpoint_generation != 0
            && query.known_endpoint_generation != endpoint_generation
        {
            return Err(GjcSdkQueryError::StaleEndpointGeneration {
                observed: endpoint_generation,
            });
        }
        let control =
            crate::gjc::control::GjcControlPlane::for_worktree(worktree, self.registry.clone());
        let session_query = control
            .query_session(
                &session,
                &[
                    "metadata",
                    "stats",
                    "model_profile",
                    "turn",
                    "workflow_gates",
                ],
            )
            .await
            .map_err(map_control_error)?;
        let confirmed_generation =
            crate::gjc::transport::discover_endpoint_for_session(worktree, &query.sdk_session_id)
                .map_err(map_control_error)?
                .endpoint_generation();
        if confirmed_generation != endpoint_generation {
            return Err(GjcSdkQueryError::StaleEndpointGeneration {
                observed: confirmed_generation,
            });
        }
        if !session_query.turn_present {
            return Err(GjcSdkQueryError::InvalidState(
                "authoritative session query omitted turn".to_string(),
            ));
        }
        let turn = session_query.turn.as_ref();
        let (turn_state, turn_id, disposition, error_summary, prompt_accepted) = match turn {
            Some(turn) => match turn.status {
                crate::gjc::model::GjcPromptStatus::Queued => (
                    GjcTurnState::Running,
                    Some(turn.turn_id.clone()),
                    GjcSessionDisposition::Live,
                    None,
                    turn.prompt_accepted,
                ),
                crate::gjc::model::GjcPromptStatus::Running => (
                    GjcTurnState::Running,
                    Some(turn.turn_id.clone()),
                    GjcSessionDisposition::Live,
                    None,
                    turn.prompt_accepted,
                ),
                crate::gjc::model::GjcPromptStatus::Succeeded => (
                    GjcTurnState::Complete,
                    Some(turn.turn_id.clone()),
                    GjcSessionDisposition::Live,
                    turn.outcome
                        .as_ref()
                        .and_then(|outcome| outcome.summary.clone()),
                    turn.prompt_accepted,
                ),
                crate::gjc::model::GjcPromptStatus::Failed => (
                    GjcTurnState::Failed,
                    Some(turn.turn_id.clone()),
                    GjcSessionDisposition::Live,
                    turn.outcome
                        .as_ref()
                        .and_then(|outcome| outcome.summary.clone()),
                    turn.prompt_accepted,
                ),
                crate::gjc::model::GjcPromptStatus::Aborted => (
                    GjcTurnState::Aborted,
                    Some(turn.turn_id.clone()),
                    GjcSessionDisposition::Live,
                    turn.outcome
                        .as_ref()
                        .and_then(|outcome| outcome.summary.clone()),
                    turn.prompt_accepted,
                ),
            },
            None => (
                GjcTurnState::Idle,
                None,
                GjcSessionDisposition::Live,
                None,
                false,
            ),
        };
        let command_id = if let Some(turn_id) = turn_id.as_deref() {
            let registry = self.registry.read().await;
            registry
                .values()
                .filter(|receipt| {
                    receipt.session_id == query.sdk_session_id
                        && receipt.turn_id.as_deref() == Some(turn_id)
                })
                .max_by(|left, right| {
                    left.created_at
                        .cmp(&right.created_at)
                        .then_with(|| left.command_id.cmp(&right.command_id))
                })
                .map(|receipt| receipt.command_id.clone())
        } else {
            None
        };
        let gates = session_query.workflow_gates.as_deref().unwrap_or_default();
        if gates
            .iter()
            .filter(|gate| gate.state == crate::gjc::model::WorkflowGateState::Ready)
            .count()
            > 1
        {
            return Err(GjcSdkQueryError::InvalidState(
                "multiple ready workflow gates".to_string(),
            ));
        }
        if gates
            .iter()
            .any(|gate| !valid_gate_id(&gate.gate_id) || gate_revision(gate) == 0)
        {
            return Err(GjcSdkQueryError::InvalidState(
                "workflow gate identity or revision is malformed".to_string(),
            ));
        }
        let gate = gates
            .iter()
            .find(|gate| gate.state == crate::gjc::model::WorkflowGateState::Ready)
            .or_else(|| {
                session_query
                    .workflow_gates
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .max_by_key(|gate| crate::gjc_sdk_events::gate_revision(gate))
            });
        let (branch_observed, branch) = match query.worktree.as_deref() {
            Some(worktree) => match current_git_branch(worktree) {
                Ok(branch) => (true, branch),
                Err(_) => (false, None),
            },
            None => (false, None),
        };
        Ok(GjcSdkObservation {
            session_id: query.sdk_session_id.clone(),
            worktree: query.worktree.clone(),
            branch_observed,
            branch,
            endpoint_generation,
            revision: session_query.revision.ok_or_else(|| {
                GjcSdkQueryError::InvalidState(
                    "authoritative session query omitted revision".to_string(),
                )
            })?,
            turn_state: if gate
                .is_some_and(|gate| gate.state == crate::gjc::model::WorkflowGateState::Ready)
            {
                GjcTurnState::AwaitingInput
            } else {
                turn_state
            },
            turn_id,
            command_id,
            gate_state: gate.map(|gate| {
                if gate.state == crate::gjc::model::WorkflowGateState::Ready {
                    GjcGateState::Ready
                } else {
                    GjcGateState::Answered
                }
            }),
            gate_section_present: session_query.workflow_gates_present,
            gate_id: gate.map(|gate| gate.gate_id.clone()),
            gate_revision: gate.map(crate::gjc_sdk_events::gate_revision).unwrap_or(0),
            gate_kind: gate.and_then(|gate| match gate.kind.as_deref() {
                Some("ask") => Some(GjcSdkGateKind::Ask),
                Some("workflow") => Some(GjcSdkGateKind::Workflow),
                _ => None,
            }),
            gate_workflow_id: gate.and_then(|gate| gate.workflow_id.clone()),
            gate_title: gate.and_then(|gate| gate.title.clone()),
            gate_options: gate.map(|gate| gate.options.clone()).unwrap_or_default(),
            disposition,
            error_summary,
            prompt_accepted,
            model: session_query
                .model_profile
                .as_ref()
                .map(|selection| selection.model.clone()),
            profile: session_query
                .model_profile
                .as_ref()
                .and_then(|selection| selection.profile.clone()),
        })
    }
}

fn map_control_error(error: crate::gjc::model::GjcError) -> GjcSdkQueryError {
    use crate::gjc::model::GjcError;
    match error {
        GjcError::SessionNotFound { .. } => {
            GjcSdkQueryError::SessionNotFound("session not found".into())
        }
        GjcError::Timeout { .. } => GjcSdkQueryError::Timeout("control-plane timeout".into()),
        GjcError::TransportUnavailable | GjcError::StaleEndpoint { .. } => {
            GjcSdkQueryError::EndpointUnavailable("sdk endpoint unavailable".into())
        }
        GjcError::AmbiguousAck { .. }
        | GjcError::InvalidPeerReply { .. }
        | GjcError::MissingCapability { .. }
        | GjcError::SessionMismatch { .. }
        | GjcError::InvalidRequest { .. } => {
            GjcSdkQueryError::Ambiguous("control-plane response was not authoritative".into())
        }
    }
}

/// Inputs the control plane needs to locate one SDK session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GjcLaneQuery {
    pub sdk_session_id: String,
    pub worktree: Option<String>,
    pub known_endpoint_generation: u64,
}

/// Current PR head/base state used to invalidate stale lane evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcPrState {
    pub number: u64,
    pub head_sha: String,
    pub base_branch: String,
}

/// PR resolution seam. Returns `Ok(None)` when the PR is not open anymore.
#[async_trait]
pub trait GjcLanePrResolver: Send + Sync {
    async fn resolve_pr(&self, repo: &str, number: u64) -> Result<Option<GjcPrState>>;
}

type Result<T> = anyhow::Result<T>;

/// Ownership lifecycle for a retained lane watch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GjcOwnershipState {
    #[default]
    Unclaimed,
    Claimed,
    Relinquished,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcLaneOwnership {
    pub state: GjcOwnershipState,
    pub owner_id: Option<String>,
    pub claimed_at: Option<String>,
    pub generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GjcTerminalKind {
    Complete,
    Failed,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcTerminalDisposition {
    pub kind: GjcTerminalKind,
    pub observed_at: String,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcPrBinding {
    pub repo: String,
    pub number: u64,
    pub head_sha: String,
    pub base_branch: String,
    pub bound_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcTurnSnapshot {
    pub state: GjcTurnState,
    pub turn_id: Option<String>,
    #[serde(default)]
    pub command_id: Option<String>,
    #[serde(default)]
    pub prompt_accepted: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    pub observed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcGateSnapshot {
    pub state: GjcGateState,
    pub gate_id: Option<String>,
    #[serde(default)]
    pub revision: u64,
    pub observed_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GjcAuditKind {
    LaneRegistered,
    OwnershipChanged,
    ObservationApplied,
    PhaseChanged,
    TerminalDispositionSet,
    GhostWatchRemoved,
    EvidenceInvalidated,
    PrHeadChanged,
    PrBaseChanged,
    RestartReconciled,
    ReconcileSkippedRevisionConflict,
    PollingSuspended,
    PollingResumed,
    HistoricalFailureSuppressed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcReconcileAuditEntry {
    pub at: String,
    pub lane_id: String,
    pub kind: GjcAuditKind,
    pub detail: String,
}

/// One alert durably staged before attempting delivery. The event payload is
/// already public-safe and carries its deterministic `event_id`; keeping the
/// complete payload here makes crash recovery independent of a fresh SDK
/// observation or bridge process-local reducer state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcPendingAlert {
    pub event_id: String,
    pub kind: String,
    pub payload: Value,
    #[serde(default)]
    pub queued_at: String,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_failure: Option<GjcTurnFailureCausality>,
    /// Per-destination ownership and terminal outcome. The map is durable so
    /// a replay can skip destinations already claimed or delivered while
    /// independently retrying destinations whose sink returned an error.
    #[serde(default)]
    pub deliveries: BTreeMap<String, GjcPendingAlertDelivery>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcTurnFailureCausality {
    pub session_id: String,
    pub sdk_revision: u64,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GjcPendingAlertDeliveryState {
    Claimed,
    Delivered,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcPendingAlertDelivery {
    pub state: GjcPendingAlertDeliveryState,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub updated_at: String,
}

impl GjcPendingAlert {
    fn incoming_event(&self) -> IncomingEvent {
        IncomingEvent {
            kind: self.kind.clone(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: self.payload.clone(),
        }
    }
}

/// Durable record for one GJC SDK-backed lane watch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcLaneRecord {
    pub lane_id: String,
    pub sdk_session_id: String,
    pub worktree: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    /// Last observed SDK endpoint generation; a bump invalidates stale
    /// review/owner evidence captured under the previous generation.
    pub endpoint_generation: u64,
    /// Monotonic durable endpoint failure episode. It changes only after a
    /// successful endpoint observation followed by a new failure, never on a
    /// local CAS mutation.
    #[serde(default)]
    pub endpoint_episode: u64,
    #[serde(default)]
    pub endpoint_alerted: bool,
    /// Local durable-record revision for optimistic concurrency (CAS).
    pub revision: u64,
    /// Last observed SDK-side session revision.
    pub sdk_revision: u64,
    pub last_turn: Option<GjcTurnSnapshot>,
    pub last_gate: Option<GjcGateSnapshot>,
    pub ownership: GjcLaneOwnership,
    pub terminal_disposition: Option<GjcTerminalDisposition>,
    pub phase: Option<GjcLanePhase>,
    pub pr: Option<GjcPrBinding>,
    pub evidence_revision: u64,
    #[serde(default = "default_true")]
    pub review_evidence_valid: bool,
    #[serde(default = "default_true")]
    pub owner_evidence_valid: bool,
    pub consecutive_unavailable_polls: u32,
    pub polling_suspended: bool,
    pub last_query_at: Option<String>,
    pub watch_removed_at: Option<String>,
    pub registered_at: String,
    pub updated_at: String,
    /// Alerts staged before delivery; bounded and replayed until acknowledged.
    #[serde(default)]
    pub pending_alerts: Vec<GjcPendingAlert>,
}

/// Registration input accepted by the store and daemon API.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GjcLaneRegistrationRequest {
    pub sdk_session_id: String,
    pub worktree: Option<String>,
    pub endpoint_generation: Option<u64>,
    pub owner_id: Option<String>,
    pub pr: Option<GjcPrBindingInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcPrBindingInput {
    pub repo: String,
    pub number: u64,
    pub head_sha: String,
    pub base_branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GjcApplyOutcome {
    pub phase_changed: Option<GjcLanePhase>,
    pub terminal_set: Option<GjcTerminalKind>,
    pub evidence_invalidated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GjcPrReconcileOutcome {
    pub head_changed: bool,
    pub base_changed: bool,
    pub evidence_invalidated: bool,
    pub unresolved: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
struct GjcLaneStateFile {
    schema: String,
    generation: u64,
    lanes: BTreeMap<String, GjcLaneRecord>,
    #[serde(default)]
    lane_id_aliases: BTreeMap<String, String>,
    audit: Vec<GjcReconcileAuditEntry>,
    audit_entries_dropped: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum GjcLaneStoreStatus {
    Missing,
    Loaded { lanes: usize },
    IgnoredInvalid { error: String },
}

/// Durable, atomically-persisted store of GJC SDK lane watches.
pub struct GjcLaneStore {
    path: PathBuf,
    state: Mutex<GjcLaneStateFile>,
    status: GjcLaneStoreStatus,
}

pub type SharedGjcLaneStore = Arc<GjcLaneStore>;

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Stable, filesystem-safe lane id derived from the SDK session identity.
pub fn gjc_lane_id(sdk_session_id: &str) -> String {
    let digest = Sha256::digest(sdk_session_id.as_bytes());
    let hex: String = digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("gjc-{hex}")
}

fn fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let hex: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    hex
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_OWNER_LEN
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_session_id(value: &str) -> bool {
    value.len() <= MAX_SESSION_LEN && crate::gjc::model::SessionId::new(value).is_ok()
}

fn valid_worktree(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_WORKTREE_LEN || value.chars().any(char::is_control) {
        return false;
    }
    let path = std::path::Path::new(value);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return false;
    }
    path.canonicalize()
        .map(|canonical| canonical == path)
        .unwrap_or(true)
}

fn valid_hex_sha(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn bounded_text(value: &str) -> String {
    value.chars().take(MAX_EVIDENCE_LEN).collect()
}

fn default_true() -> bool {
    true
}

fn migrate_lane_ids(state: &mut GjcLaneStateFile) -> Result<bool> {
    for (alias, target) in &state.lane_id_aliases {
        if alias == target || state.lanes.contains_key(alias) {
            bail!("lane id migration alias collides with canonical lane");
        }
        if state.lane_id_aliases.contains_key(target) {
            bail!("lane id migration alias chain is not supported");
        }
        if !state.lanes.contains_key(target) {
            bail!("lane id migration alias target is missing");
        }
    }
    let canonical_ids = state
        .lanes
        .values()
        .map(|record| gjc_lane_id(&record.sdk_session_id))
        .collect::<std::collections::BTreeSet<_>>();
    let mut migrated = false;
    let mut lanes = BTreeMap::new();
    for (old_id, mut record) in std::mem::take(&mut state.lanes) {
        let new_id = gjc_lane_id(&record.sdk_session_id);
        if old_id != new_id && canonical_ids.contains(&old_id) {
            bail!("lane id migration alias collision for canonical lane");
        }
        if old_id != new_id || record.lane_id != new_id {
            migrated = true;
            record.lane_id = new_id.clone();
        }
        if lanes.insert(new_id.clone(), record).is_some() {
            bail!("lane id migration collision for session identity");
        }
        if old_id != new_id {
            state.lane_id_aliases.insert(old_id, new_id);
        }
    }
    state.lanes = lanes;
    for target in state.lane_id_aliases.values() {
        if state.lane_id_aliases.contains_key(target) {
            bail!("lane id migration alias chain is not supported");
        }
        if !state.lanes.contains_key(target) {
            bail!("lane id migration alias target is missing");
        }
    }
    Ok(migrated)
}

fn migrate_failure_causality(state: &mut GjcLaneStateFile) -> bool {
    let mut migrated = false;
    for record in state.lanes.values_mut() {
        let legacy_count = record
            .pending_alerts
            .iter()
            .filter(|alert| alert.kind == "session.failed" && alert.turn_failure.is_none())
            .count();
        let fallback = (legacy_count == 1)
            .then_some(record.last_turn.as_ref())
            .flatten()
            .filter(|turn| turn.state == GjcTurnState::Failed)
            .and_then(|turn| {
                Some(GjcTurnFailureCausality {
                    session_id: record.sdk_session_id.clone(),
                    sdk_revision: record.sdk_revision,
                    turn_id: turn.turn_id.clone()?,
                    command_id: turn.command_id.clone(),
                    model: turn.model.clone(),
                    profile: turn.profile.clone(),
                })
            });
        for alert in &mut record.pending_alerts {
            if alert.kind == "session.failed" && alert.turn_failure.is_none() {
                let payload = &alert.payload;
                let derived = payload["session_id"]
                    .as_str()
                    .zip(payload["turn_id"].as_str())
                    .zip(payload["sdk_revision"].as_u64())
                    .map(
                        |((session_id, turn_id), sdk_revision)| GjcTurnFailureCausality {
                            session_id: session_id.to_string(),
                            sdk_revision,
                            turn_id: turn_id.to_string(),
                            command_id: payload["command_id"].as_str().map(str::to_string),
                            model: payload["model"].as_str().map(public_selection_label),
                            profile: payload["profile"].as_str().map(public_selection_label),
                        },
                    )
                    .or_else(|| fallback.clone());
                if derived.is_some() {
                    alert.turn_failure = derived;
                    migrated = true;
                }
            }
        }
    }
    migrated
}

fn current_git_branch(worktree: &str) -> Result<Option<String>> {
    let output = std::process::Command::new("git")
        .args(["-C", worktree, "branch", "--show-current"])
        .output()
        .context("git branch probe failed")?;
    if !output.status.success() {
        bail!("git branch probe returned non-success");
    }
    let branch = String::from_utf8(output.stdout)
        .context("git branch probe returned invalid output")?
        .trim()
        .to_string();
    Ok((!branch.is_empty() && branch.len() <= 128).then_some(branch))
}

fn canonical_lane_id(state: &GjcLaneStateFile, lane_id: &str) -> String {
    state
        .lane_id_aliases
        .get(lane_id)
        .cloned()
        .unwrap_or_else(|| lane_id.to_string())
}

fn observation_snapshot(
    record: &GjcLaneRecord,
    previous: Option<&GjcLaneRecord>,
    observation: &GjcSdkObservation,
    now: &str,
    endpoint: Option<GjcSdkEndpoint>,
) -> GjcSdkStateSnapshot {
    let turn_id = observation.turn_id.clone().or_else(|| {
        previous
            .and_then(|record| record.last_turn.as_ref())
            .and_then(|turn| turn.turn_id.clone())
    });
    let turn = turn_id.map(|id| GjcSdkTurn {
        id,
        state: match observation.turn_state {
            GjcTurnState::Running => {
                if observation.prompt_accepted {
                    GjcSdkTurnPhase::Active
                } else {
                    GjcSdkTurnPhase::Idle
                }
            }
            GjcTurnState::AwaitingInput => GjcSdkTurnPhase::WaitingInput,
            GjcTurnState::Complete => GjcSdkTurnPhase::Complete,
            GjcTurnState::Failed => GjcSdkTurnPhase::Failed,
            GjcTurnState::Idle => GjcSdkTurnPhase::Idle,
            GjcTurnState::Aborted => GjcSdkTurnPhase::Aborted,
        },
        prompt_accepted: observation.prompt_accepted,
        attempt: 0,
        error_summary: observation.error_summary.clone(),
    });
    let prompt = observation
        .command_id
        .clone()
        .map(|command_id| GjcSdkPrompt {
            command_id,
            status: if matches!(
                observation.turn_state,
                GjcTurnState::Running | GjcTurnState::AwaitingInput
            ) {
                GjcSdkPromptStatus::Progressing
            } else {
                GjcSdkPromptStatus::Accepted
            },
        });
    let gate = observation.gate_id.as_ref().map(|gate_id| GjcSdkGate {
        id: gate_id.clone(),
        kind: observation.gate_kind.unwrap_or(GjcSdkGateKind::Workflow),
        revision: observation.gate_revision,
        status: if observation.gate_state == Some(GjcGateState::Ready) {
            GjcSdkGateStatus::Open
        } else {
            GjcSdkGateStatus::Resolved
        },
        summary: observation.gate_title.clone(),
        workflow_id: observation.gate_workflow_id.clone(),
        title: observation.gate_title.clone(),
        options: observation.gate_options.clone(),
    });
    GjcSdkStateSnapshot {
        session_id: observation.session_id.clone(),
        revision: observation.revision,
        endpoint_episode: record.endpoint_episode,
        turn,
        prompt,
        gate,
        model: observation.model.clone(),
        profile: observation.profile.clone(),
        endpoint,
        repo_name: record.pr.as_ref().map(|pr| pr.repo.clone()),
        repo_path: record.worktree.clone(),
        worktree_path: record.worktree.clone(),
        branch: record.branch.clone(),
        observed_at: Some(now.to_string()),
        summary: observation.error_summary.clone(),
        ..GjcSdkStateSnapshot::default()
    }
}

fn pending_from_event(event: IncomingEvent, now: &str) -> Result<GjcPendingAlert> {
    let event_id = event
        .payload
        .get("event_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("gjc alert missing deterministic event id"))?
        .to_string();
    let turn_failure = turn_failure_causality(&event)?;
    Ok(GjcPendingAlert {
        event_id,
        kind: event.kind,
        payload: event.payload,
        queued_at: now.to_string(),
        attempts: 0,
        turn_failure,
        deliveries: BTreeMap::new(),
    })
}

fn turn_failure_causality(event: &IncomingEvent) -> Result<Option<GjcTurnFailureCausality>> {
    if event.kind != "session.failed" {
        return Ok(None);
    }
    let session_id = event.payload["session_id"]
        .as_str()
        .ok_or_else(|| anyhow!("gjc turn failure missing session identity"))?;
    let turn_id = event.payload["turn_id"]
        .as_str()
        .ok_or_else(|| anyhow!("gjc turn failure missing turn identity"))?;
    crate::gjc::model::SessionId::new(session_id.to_string())
        .map_err(|_| anyhow!("gjc turn failure session identity malformed"))?;
    crate::gjc::model::TurnId::new(turn_id.to_string())
        .map_err(|_| anyhow!("gjc turn failure turn identity malformed"))?;
    let command_id = event.payload["command_id"].as_str().map(str::to_string);
    if let Some(command_id) = command_id.as_deref() {
        crate::gjc::model::CommandId::new(command_id.to_string())
            .map_err(|_| anyhow!("gjc turn failure command identity malformed"))?;
    }
    Ok(Some(GjcTurnFailureCausality {
        session_id: session_id.to_string(),
        sdk_revision: event.payload["sdk_revision"]
            .as_u64()
            .ok_or_else(|| anyhow!("gjc turn failure missing sdk revision"))?,
        turn_id: turn_id.to_string(),
        command_id,
        model: event.payload["model"].as_str().map(bounded_text),
        profile: event.payload["profile"].as_str().map(bounded_text),
    }))
}

fn queue_pending_alert(record: &mut GjcLaneRecord, alert: GjcPendingAlert) -> Result<()> {
    if record
        .pending_alerts
        .iter()
        .any(|existing| existing.event_id == alert.event_id)
    {
        return Ok(());
    }
    if record.pending_alerts.len() >= MAX_PENDING_ALERTS {
        bail!("gjc pending alert queue is full");
    }
    record.pending_alerts.push(alert);
    Ok(())
}

struct GjcPendingAlertJournal {
    store: SharedGjcLaneStore,
    lane_id: String,
    event_id: String,
}

impl AlertDeliveryJournal for GjcPendingAlertJournal {
    fn state(&self, destination: &str) -> Option<AlertDeliveryState> {
        self.store
            .pending_alert_delivery_state(&self.lane_id, &self.event_id, destination)
            .map(|state| match state {
                GjcPendingAlertDeliveryState::Claimed => AlertDeliveryState::Claimed,
                GjcPendingAlertDeliveryState::Delivered => AlertDeliveryState::Delivered,
                GjcPendingAlertDeliveryState::Failed => AlertDeliveryState::Failed,
            })
    }

    fn claim(&self, destination: &str) -> bool {
        self.store
            .claim_pending_alert_delivery(&self.lane_id, &self.event_id, destination)
            .unwrap_or(false)
    }

    fn record(&self, destination: &str, delivered: bool) -> bool {
        self.store
            .record_pending_alert_delivery(&self.lane_id, &self.event_id, destination, delivered)
            .unwrap_or(false)
    }
}

fn bridge_alerts(
    previous: Option<&GjcSdkStateSnapshot>,
    snapshot: &GjcSdkStateSnapshot,
) -> Result<Vec<IncomingEvent>> {
    let mut bridge = GjcEventBridge::new();
    if let Some(previous) = previous {
        bridge
            .observe(previous)
            .map_err(|error| anyhow!("build prior sdk alert state: {error}"))?;
    }
    Ok(bridge
        .observe(snapshot)
        .map_err(|error| anyhow!("build sdk alert: {error}"))?
        .events
        .into_iter()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                "session.failed"
                    | "session.stalled"
                    | "session.endpoint-failed"
                    | "workflow.gate"
                    | "workflow.question"
            )
        })
        .collect())
}

impl GjcLaneStore {
    /// Open (or initialize) the durable lane-state file. Invalid content fails
    /// open as `IgnoredInvalid` rather than blocking the daemon, mirroring the
    /// tmux watch registry contract.
    pub fn open(path: &Path) -> Result<Self> {
        let (mut state, status) = match std::fs::read(path) {
            Ok(content) => match serde_json::from_slice::<GjcLaneStateFile>(&content) {
                Ok(parsed) if parsed.schema == GJC_LANE_STATE_SCHEMA => {
                    match validate_loaded_state(&parsed) {
                        Ok(()) => {
                            let lanes = parsed.lanes.len();
                            (parsed, GjcLaneStoreStatus::Loaded { lanes })
                        }
                        Err(error) => (
                            GjcLaneStateFile {
                                schema: GJC_LANE_STATE_SCHEMA.to_string(),
                                ..Default::default()
                            },
                            GjcLaneStoreStatus::IgnoredInvalid {
                                error: error.to_string(),
                            },
                        ),
                    }
                }
                Ok(parsed) => (
                    GjcLaneStateFile {
                        schema: GJC_LANE_STATE_SCHEMA.to_string(),
                        ..Default::default()
                    },
                    GjcLaneStoreStatus::IgnoredInvalid {
                        error: format!("unsupported lane-state schema {:?}", parsed.schema),
                    },
                ),
                Err(error) => (
                    GjcLaneStateFile {
                        schema: GJC_LANE_STATE_SCHEMA.to_string(),
                        ..Default::default()
                    },
                    GjcLaneStoreStatus::IgnoredInvalid {
                        error: error.to_string(),
                    },
                ),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (
                GjcLaneStateFile {
                    schema: GJC_LANE_STATE_SCHEMA.to_string(),
                    ..Default::default()
                },
                GjcLaneStoreStatus::Missing,
            ),
            Err(error) => {
                return Err(error).context(format!("failed to read {}", path.display()));
            }
        };
        let migrated = migrate_lane_ids(&mut state)? | migrate_failure_causality(&mut state);
        let store = Self {
            path: path.to_path_buf(),
            state: Mutex::new(state),
            status,
        };
        if migrated {
            let mut state = store
                .state
                .lock()
                .map_err(|_| anyhow!("lane store poisoned"))?;
            store.persist_locked(&mut state)?;
        }
        Ok(store)
    }

    pub fn status(&self) -> &GjcLaneStoreStatus {
        &self.status
    }

    pub fn generation(&self) -> u64 {
        self.state.lock().map(|state| state.generation).unwrap_or(0)
    }

    pub fn snapshot(&self) -> Vec<GjcLaneRecord> {
        self.state
            .lock()
            .map(|state| state.lanes.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Active watches exclude tombstoned (ghost-removed) lanes.
    pub fn snapshot_watches(&self, include_removed: bool) -> Vec<GjcLaneRecord> {
        self.snapshot()
            .into_iter()
            .filter(|record| include_removed || record.watch_removed_at.is_none())
            .collect()
    }

    pub fn record(&self, lane_id: &str) -> Option<GjcLaneRecord> {
        let state = self.state.lock().ok()?;
        let canonical = state
            .lane_id_aliases
            .get(lane_id)
            .map(String::as_str)
            .unwrap_or(lane_id);
        state.lanes.get(canonical).cloned()
    }

    pub fn record_for_session(&self, session_id: &str) -> Option<GjcLaneRecord> {
        let state = self.state.lock().ok()?;
        state
            .lanes
            .values()
            .find(|record| record.sdk_session_id == session_id)
            .cloned()
    }

    pub fn set_sdk_revision_if(
        &self,
        lane_id: &str,
        expected_sdk_revision: u64,
        new_sdk_revision: u64,
        now: &str,
    ) -> Result<GjcLaneRecord> {
        let current = self
            .record(lane_id)
            .ok_or_else(|| anyhow!("lane not found"))?;
        self.mutate(lane_id, current.revision, now, |mut record| {
            if record.sdk_revision != expected_sdk_revision
                || new_sdk_revision <= record.sdk_revision
            {
                bail!("sdk revision watermark conflict");
            }
            record.sdk_revision = new_sdk_revision;
            Ok(multi_audit(record, vec![]))
        })
    }

    pub fn stage_bridge_failure(
        &self,
        lane_id: &str,
        expected_revision: u64,
        failure: &IncomingEvent,
        now: &str,
    ) -> Result<GjcLaneRecord> {
        if failure.kind != "session.failed" {
            bail!("bridge failure event kind is not session.failed");
        }
        let session_id = failure.payload["session_id"]
            .as_str()
            .ok_or_else(|| anyhow!("bridge failure lacks session identity"))?;
        let turn_id = failure.payload["turn_id"]
            .as_str()
            .ok_or_else(|| anyhow!("bridge failure lacks turn identity"))?;
        let record = self
            .record(lane_id)
            .ok_or_else(|| anyhow!("lane not found: {lane_id}"))?;
        let observation = GjcSdkObservation {
            session_id: session_id.to_string(),
            worktree: record.worktree.clone(),
            branch: record.branch.clone(),
            branch_observed: false,
            endpoint_generation: record.endpoint_generation,
            revision: failure.payload["sdk_revision"]
                .as_u64()
                .ok_or_else(|| anyhow!("bridge failure lacks sdk revision"))?,
            turn_state: GjcTurnState::Failed,
            turn_id: Some(turn_id.to_string()),
            command_id: failure.payload["command_id"].as_str().map(str::to_string),
            prompt_accepted: true,
            model: failure.payload["model"].as_str().map(str::to_string),
            profile: failure.payload["profile"].as_str().map(str::to_string),
            gate_state: None,
            gate_section_present: false,
            gate_id: None,
            gate_revision: 0,
            gate_kind: None,
            gate_workflow_id: None,
            gate_title: None,
            gate_options: Vec::new(),
            disposition: GjcSessionDisposition::Live,
            error_summary: failure.payload["error_message"]
                .as_str()
                .or_else(|| failure.payload["summary"].as_str())
                .map(str::to_string),
        };
        self.apply_observation(lane_id, expected_revision, &observation, now)
            .map(|(record, _)| record)
    }

    pub fn resume_suspended(&self, now: &str) -> usize {
        let records = self.snapshot_watches(true);
        records
            .into_iter()
            .filter(|record| record.polling_suspended)
            .filter_map(|record| {
                self.mutate(&record.lane_id, record.revision, now, |mut updated| {
                    updated.polling_suspended = false;
                    updated.consecutive_unavailable_polls = 0;
                    Ok(multi_audit(
                        updated,
                        vec![(GjcAuditKind::PollingResumed, "manual force-resume".into())],
                    ))
                })
                .ok()
            })
            .count()
    }

    pub fn audit(&self) -> Vec<GjcReconcileAuditEntry> {
        self.state
            .lock()
            .map(|state| state.audit.clone())
            .unwrap_or_default()
    }

    pub fn audit_entries_dropped(&self) -> u64 {
        self.state
            .lock()
            .map(|state| state.audit_entries_dropped)
            .unwrap_or(0)
    }

    /// Record a daemon restart in the audit trail (restart-safe reconciliation marker).
    pub fn note_restart(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("lane store poisoned"))?;
        state.push_audit(GjcReconcileAuditEntry {
            at: now_rfc3339(),
            lane_id: "*".to_string(),
            kind: GjcAuditKind::RestartReconciled,
            detail: "durable lane state reloaded after daemon restart".to_string(),
        });
        self.persist_locked(&mut state)
    }

    /// Register a fresh lane watch. Fails on duplicate registration.
    pub fn register_lane(
        &self,
        request: &GjcLaneRegistrationRequest,
        now: &str,
    ) -> Result<GjcLaneRecord> {
        if let GjcLaneStoreStatus::IgnoredInvalid { .. } = self.status() {
            bail!("lane store is quarantined after invalid persisted state");
        }
        let lane_id = gjc_lane_id(&request.sdk_session_id);
        validate_registration(request)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("lane store poisoned"))?;
        if state.lanes.contains_key(&lane_id) {
            let existing = &state.lanes[&lane_id];
            let removed_note = if existing.watch_removed_at.is_some() {
                "; watch previously tombstoned"
            } else {
                ""
            };
            bail!("lane registration conflict: {lane_id}{removed_note}");
        }
        let ownership = match &request.owner_id {
            Some(owner_id) => GjcLaneOwnership {
                state: GjcOwnershipState::Claimed,
                owner_id: Some(owner_id.clone()),
                claimed_at: Some(now.to_string()),
                generation: 1,
            },
            None => GjcLaneOwnership::default(),
        };
        let pr = request.pr.as_ref().map(|pr| GjcPrBinding {
            repo: pr.repo.clone(),
            number: pr.number,
            head_sha: pr.head_sha.to_ascii_lowercase(),
            base_branch: pr.base_branch.clone(),
            bound_at: now.to_string(),
        });
        let record = GjcLaneRecord {
            lane_id: lane_id.clone(),
            sdk_session_id: request.sdk_session_id.clone(),
            worktree: request.worktree.clone(),
            branch: request
                .worktree
                .as_deref()
                .and_then(|worktree| current_git_branch(worktree).ok().flatten()),
            endpoint_generation: request.endpoint_generation.unwrap_or(0),
            endpoint_episode: 0,
            endpoint_alerted: false,
            revision: 1,
            sdk_revision: 0,
            last_turn: None,
            last_gate: None,
            ownership,
            terminal_disposition: None,
            phase: None,
            pr,
            evidence_revision: 1,
            review_evidence_valid: true,
            owner_evidence_valid: true,
            consecutive_unavailable_polls: 0,
            polling_suspended: false,
            last_query_at: None,
            watch_removed_at: None,
            registered_at: now.to_string(),
            updated_at: now.to_string(),
            pending_alerts: Vec::new(),
        };
        state.push_audit(GjcReconcileAuditEntry {
            at: now.to_string(),
            lane_id: lane_id.clone(),
            kind: GjcAuditKind::LaneRegistered,
            detail: format!(
                "session {} registered{}",
                request.sdk_session_id,
                request
                    .owner_id
                    .as_ref()
                    .map(|owner| format!(" owned by {owner}"))
                    .unwrap_or_default()
            ),
        });
        state.lanes.insert(lane_id, record.clone());
        self.persist_locked(&mut state)?;
        Ok(record)
    }

    /// Claim ownership with optimistic concurrency.
    ///
    /// Exercised extensively by the durable-store tests; the daemon HTTP verb
    /// lands together with the control-plane track's mutation surface.
    #[allow(dead_code)]
    pub fn claim_ownership(
        &self,
        lane_id: &str,
        expected_revision: u64,
        owner_id: &str,
        now: &str,
    ) -> Result<GjcLaneRecord> {
        self.mutate(lane_id, expected_revision, now, move |mut record| {
            if record.terminal_disposition.is_some() {
                bail!("terminal lane cannot reclaim ownership");
            }
            if record.watch_removed_at.is_some() {
                bail!("tombstoned lane cannot claim ownership");
            }
            if !valid_id(owner_id) {
                bail!("invalid owner id");
            }
            record.ownership = GjcLaneOwnership {
                state: GjcOwnershipState::Claimed,
                owner_id: Some(owner_id.to_string()),
                claimed_at: Some(now.to_string()),
                generation: record.ownership.generation + 1,
            };
            Ok(multi_audit(
                record,
                vec![(
                    GjcAuditKind::OwnershipChanged,
                    "ownership claimed".to_string(),
                )],
            ))
        })
    }

    /// Apply an authoritative SDK observation: refresh evidence snapshots,
    /// classify the lane, set terminal dispositions from SDK evidence, and
    /// invalidate stale evidence when the endpoint generation moved forward.
    pub fn apply_observation(
        &self,
        lane_id: &str,
        expected_revision: u64,
        observation: &GjcSdkObservation,
        now: &str,
    ) -> Result<(GjcLaneRecord, GjcApplyOutcome)> {
        let mut outcome = GjcApplyOutcome {
            phase_changed: None,
            terminal_set: None,
            evidence_invalidated: false,
        };
        let record = self.mutate(lane_id, expected_revision, now, |mut record| {
            let previous_record = record.clone();
            let mut observation = observation.clone();
            sanitize_observation_selection(&mut observation);
            #[cfg(not(test))]
            {
                if observation.session_id != record.sdk_session_id {
                    bail!("sdk observation session identity mismatch");
                }
                if let Some(turn_id) = observation.turn_id.as_deref() {
                    crate::gjc::model::TurnId::new(turn_id.to_string()).map_err(|_| {
                        anyhow::anyhow!("sdk observation turn identity malformed")
                    })?;
                }
                if let Some(command_id) = observation.command_id.as_deref() {
                    crate::gjc::model::CommandId::new(command_id.to_string()).map_err(|_| {
                        anyhow::anyhow!("sdk observation command identity malformed")
                    })?;
                }
                if let (Some(observed), Some(enrolled)) =
                    (observation.worktree.as_deref(), record.worktree.as_deref())
                    && observed != enrolled
                {
                    bail!("sdk observation worktree identity mismatch");
                }
            }
            if observation.revision < record.sdk_revision {
                bail!("stale sdk observation revision");
            }
            if matches!(
                (observation.disposition, observation.turn_state),
                (GjcSessionDisposition::Complete, state)
                    if state != GjcTurnState::Complete
            ) || matches!(
                (observation.disposition, observation.turn_state),
                (GjcSessionDisposition::Failed, state) if state != GjcTurnState::Failed
            ) {
                bail!("sdk observation disposition conflicts with turn state");
            }
            match observation.gate_state {
                Some(_) => {
                    if observation
                        .gate_id
                        .as_deref()
                        .is_none_or(|gate_id| !valid_gate_id(gate_id))
                        || observation.gate_revision == 0
                    {
                        bail!("gate observation lacks canonical identity or revision");
                    }
                }
                None => {
                    if observation.gate_id.is_some() || observation.gate_revision != 0 {
                        bail!("gate observation has identity without state");
                    }
                }
            }
            let mut generation_audit = Vec::new();
            let previous_generation = record.endpoint_generation;
            if observation.endpoint_generation != previous_generation
                && (record.review_evidence_valid || record.owner_evidence_valid)
            {
                record.review_evidence_valid = false;
                record.owner_evidence_valid = false;
                record.evidence_revision += 1;
                outcome.evidence_invalidated = true;
                record.endpoint_generation = observation.endpoint_generation;
                generation_audit.push((
                    GjcAuditKind::EvidenceInvalidated,
                    format!(
                        "endpoint generation {previous_generation} -> {}; stale review/owner evidence invalidated",
                        observation.endpoint_generation
                    ),
                ));
            }
            record.sdk_revision = observation.revision;
            record.endpoint_generation = observation.endpoint_generation;
            // A successful authoritative observation closes the current
            // endpoint-failure episode. The next transport failure receives a
            // fresh durable episode identity.
            record.endpoint_alerted = false;
            record.consecutive_unavailable_polls = 0;
            record.last_query_at = Some(now.to_string());
            if observation.branch_observed {
                record.branch = observation.branch.clone();
            }
            let resumed = record.polling_suspended;
            record.polling_suspended = false;

            record.last_turn = Some(GjcTurnSnapshot {
                state: observation.turn_state,
                turn_id: observation.turn_id.clone(),
                command_id: observation.command_id.clone(),
                prompt_accepted: observation.prompt_accepted,
                model: observation.model.clone(),
                profile: observation.profile.clone(),
                observed_at: now.to_string(),
            });
            if let Some(gate_state) = observation.gate_state {
                let update_gate = match record.last_gate.as_ref() {
                    None => true,
                    Some(previous) if previous.gate_id != observation.gate_id => true,
                    Some(previous) if observation.gate_revision < previous.revision => false,
                    Some(previous) if observation.gate_revision == previous.revision => {
                        if previous.state != gate_state
                            && !(previous.state == GjcGateState::Ready
                                && gate_state == GjcGateState::Answered)
                        {
                            bail!("conflicting equal gate revision");
                        }
                        previous.state != gate_state
                    }
                    Some(_) => true,
                };
                if update_gate {
                    record.last_gate = Some(GjcGateSnapshot {
                        state: gate_state,
                        gate_id: observation.gate_id.clone(),
                        revision: observation.gate_revision,
                        observed_at: now.to_string(),
                    });
                }
            } else if observation.gate_section_present
                && record.last_gate.as_ref().is_some_and(|gate| {
                    gate.state == GjcGateState::Ready
                        && (observation.gate_id.is_none()
                            || gate.gate_id == observation.gate_id)
                })
                && let Some(gate) = record.last_gate.as_mut()
            {
                gate.state = GjcGateState::Answered;
                gate.observed_at = now.to_string();
            }

            let previous_phase = record.phase;
            let previous_terminal = record.terminal_disposition.as_ref().map(|d| d.kind);

            if let Some(disposition_kind) =
                terminal_kind_from_disposition(observation.disposition)
                && record.terminal_disposition.is_none()
            {
                record.terminal_disposition = Some(GjcTerminalDisposition {
                    kind: disposition_kind,
                    observed_at: now.to_string(),
                    evidence: format!("sdk disposition {:?}", observation.disposition),
                });
                outcome.terminal_set = Some(disposition_kind);
                if record.ownership.state == GjcOwnershipState::Claimed {
                    record.ownership.state = GjcOwnershipState::Relinquished;
                }
            }

            record.phase = Some(classify_lane(&record, Some(&observation)));
            if record.phase != previous_phase {
                outcome.phase_changed = record.phase;
            }

            let mut audit = generation_audit;
            if resumed {
                audit.push((
                    GjcAuditKind::PollingResumed,
                    "authoritative observation received".to_string(),
                ));
            }
            if previous_phase != record.phase {
                audit.push((
                    GjcAuditKind::PhaseChanged,
                    format!(
                        "{} -> {}",
                        previous_phase.map(|p| p.as_str()).unwrap_or("unset"),
                        record.phase.map(|p| p.as_str()).unwrap_or("unset")
                    ),
                ));
            }
            if outcome.terminal_set.is_some() && previous_terminal.is_none() {
                audit.push((
                    GjcAuditKind::TerminalDispositionSet,
                    format!(
                        "{:?} from sdk evidence (revision {})",
                        outcome.terminal_set, observation.revision
                    ),
                ));
            }

            // Stage alert transitions in the same atomic write as the
            // observation. This closes the crash window between durable state
            // update and the reconciler's send attempt.
            let current_snapshot = observation_snapshot(
                &record,
                Some(&previous_record),
                &observation,
                now,
                None,
            );
            let new_failed_turn = observation.prompt_accepted
                && observation.turn_state == GjcTurnState::Failed
                && previous_record
                    .last_turn
                    .as_ref()
                    .map(|turn| {
                        turn.state != GjcTurnState::Failed
                            || turn.turn_id.as_deref() != observation.turn_id.as_deref()
                    })
                    .unwrap_or(true);
            if outcome.terminal_set.is_some() || new_failed_turn {
                for mut event in bridge_alerts(None, &current_snapshot)? {
                    if event.kind == "session.failed" {
                        event.payload["authority_scope"] = Value::from("current-session");
                    }
                    queue_pending_alert(&mut record, pending_from_event(event, now)?)?;
                }
            }
            let previous_turn_active = previous_record
                .last_turn
                .as_ref()
                .map(|turn| {
                    turn.prompt_accepted
                        && matches!(turn.state, GjcTurnState::Running | GjcTurnState::AwaitingInput)
                })
                .unwrap_or(false);
            let same_turn = previous_record
                .last_turn
                .as_ref()
                .and_then(|turn| turn.turn_id.as_deref())
                .zip(observation.turn_id.as_deref())
                .map(|(left, right)| left == right)
                .unwrap_or_else(|| {
                    previous_record
                        .last_turn
                        .as_ref()
                        .and_then(|turn| turn.turn_id.as_deref())
                        == current_snapshot.turn.as_ref().map(|turn| turn.id.as_str())
                });
            if previous_turn_active
                && same_turn
                && matches!(observation.turn_state, GjcTurnState::Idle | GjcTurnState::Aborted)
            {
                let previous_snapshot = observation_snapshot(
                    &previous_record,
                    None,
                    &GjcSdkObservation {
                        session_id: previous_record.sdk_session_id.clone(),
                        worktree: previous_record.worktree.clone(),
                        branch: previous_record.branch.clone(),
                        branch_observed: false,
                        endpoint_generation: previous_record.endpoint_generation,
                        revision: previous_record.sdk_revision,
                        turn_state: previous_record
                            .last_turn
                            .as_ref()
                            .map(|turn| turn.state)
                            .unwrap_or(GjcTurnState::Running),
                        turn_id: previous_record
                            .last_turn
                            .as_ref()
                            .and_then(|turn| turn.turn_id.clone()),
                        command_id: previous_record
                            .last_turn
                            .as_ref()
                            .and_then(|turn| turn.command_id.clone()),
                        prompt_accepted: true,
                        model: previous_record
                            .last_turn
                            .as_ref()
                            .and_then(|turn| turn.model.clone()),
                        profile: previous_record
                            .last_turn
                            .as_ref()
                            .and_then(|turn| turn.profile.clone()),
                        gate_revision: previous_record
                            .last_gate
                            .as_ref()
                            .map(|gate| gate.revision)
                            .unwrap_or(0),
                        gate_state: previous_record.last_gate.as_ref().map(|gate| gate.state),
                        gate_section_present: true,
                        gate_id: previous_record
                            .last_gate
                            .as_ref()
                            .and_then(|gate| gate.gate_id.clone()),
                        gate_kind: None,
                        gate_workflow_id: None,
                        gate_title: None,
                        gate_options: Vec::new(),
                        disposition: GjcSessionDisposition::Live,
                        error_summary: None,
                    },
                    now,
                    None,
                );
                for event in bridge_alerts(Some(&previous_snapshot), &current_snapshot)? {
                    queue_pending_alert(&mut record, pending_from_event(event, now)?)?;
                }
            }
            let new_gate_episode = observation.gate_state == Some(GjcGateState::Ready)
                && match previous_record.last_gate.as_ref() {
                    Some(previous) => {
                        previous.gate_id != observation.gate_id
                            || observation.gate_revision > previous.revision
                    }
                    None => true,
                };
            if new_gate_episode {
                for event in bridge_alerts(None, &current_snapshot)? {
                    queue_pending_alert(&mut record, pending_from_event(event, now)?)?;
                }
            }
            audit.push((
                GjcAuditKind::ObservationApplied,
                format!(
                    "sdk revision {} generation {} turn {:?} gate {:?} disposition {:?}",
                    observation.revision,
                    observation.endpoint_generation,
                    observation.turn_state,
                    observation.gate_state,
                    observation.disposition
                ),
            ));
            Ok(multi_audit(record, audit))
        })?;
        Ok((record, outcome))
    }

    /// Record a transport-level query failure: bounded backoff accounting and
    /// eventual suspension once the attempt budget (`suspend_after`) is
    /// exhausted. Pass `u32::MAX` to count failures without ever suspending.
    pub fn mark_unavailable(
        &self,
        lane_id: &str,
        expected_revision: u64,
        reason: &str,
        suspend_after: u32,
        now: &str,
    ) -> Result<(GjcLaneRecord, bool)> {
        let mut newly_suspended = false;
        let record = self.mutate(lane_id, expected_revision, now, |mut record| {
            let previous = record.phase;
            record.phase = Some(GjcLanePhase::Unavailable);
            record.last_query_at = Some(now.to_string());
            record.consecutive_unavailable_polls =
                record.consecutive_unavailable_polls.saturating_add(1);
            if !record.endpoint_alerted {
                record.endpoint_episode = record.endpoint_episode.saturating_add(1);
                let snapshot = GjcSdkStateSnapshot {
                    session_id: record.sdk_session_id.clone(),
                    revision: record.sdk_revision,
                    endpoint_episode: record.endpoint_episode,
                    endpoint: Some(GjcSdkEndpoint {
                        health: GjcSdkEndpointHealth::Failed,
                        detail: Some(reason.to_string()),
                    }),
                    repo_name: record.pr.as_ref().map(|pr| pr.repo.clone()),
                    worktree_path: record.worktree.clone(),
                    branch: record.branch.clone(),
                    observed_at: Some(now.to_string()),
                    ..GjcSdkStateSnapshot::default()
                };
                for event in bridge_alerts(None, &snapshot)? {
                    queue_pending_alert(&mut record, pending_from_event(event, now)?)?;
                }
                record.endpoint_alerted = true;
            }
            if !record.polling_suspended && record.consecutive_unavailable_polls >= suspend_after {
                record.polling_suspended = true;
                newly_suspended = true;
            }
            let mut audit = Vec::new();
            if previous != Some(GjcLanePhase::Unavailable) {
                audit.push((GjcAuditKind::PhaseChanged, "-> unavailable".to_string()));
            }
            if newly_suspended {
                audit.push((
                    GjcAuditKind::PollingSuspended,
                    format!(
                        "attempt budget exhausted after {} consecutive unavailable polls",
                        record.consecutive_unavailable_polls
                    ),
                ));
            }
            audit.push((
                GjcAuditKind::ObservationApplied,
                format!(
                    "unavailable (attempt {}): {}",
                    record.consecutive_unavailable_polls, "sdk endpoint unavailable"
                ),
            ));
            Ok(multi_audit(record, audit))
        })?;
        Ok((record, newly_suspended))
    }

    /// Durably stage an endpoint failure without changing lane classification.
    /// Ambiguous control-plane replies use this fail-closed path.
    pub fn enqueue_endpoint_failure(
        &self,
        lane_id: &str,
        expected_revision: u64,
        reason: &str,
        now: &str,
    ) -> Result<GjcLaneRecord> {
        self.mutate(lane_id, expected_revision, now, |mut record| {
            if !record.endpoint_alerted {
                record.endpoint_episode = record.endpoint_episode.saturating_add(1);
                let snapshot = GjcSdkStateSnapshot {
                    session_id: record.sdk_session_id.clone(),
                    revision: record.sdk_revision,
                    endpoint_episode: record.endpoint_episode,
                    endpoint: Some(GjcSdkEndpoint {
                        health: GjcSdkEndpointHealth::Failed,
                        detail: Some(reason.to_string()),
                    }),
                    repo_name: record.pr.as_ref().map(|pr| pr.repo.clone()),
                    worktree_path: record.worktree.clone(),
                    branch: record.branch.clone(),
                    observed_at: Some(now.to_string()),
                    ..GjcSdkStateSnapshot::default()
                };
                for event in bridge_alerts(None, &snapshot)? {
                    queue_pending_alert(&mut record, pending_from_event(event, now)?)?;
                }
                record.endpoint_alerted = true;
            }
            Ok(multi_audit(record, vec![]))
        })
    }

    /// Acknowledge one pending alert only after the reconciler's channel send
    /// succeeds. A repeated acknowledgement is an idempotent no-op.
    pub fn acknowledge_pending_alert(
        &self,
        lane_id: &str,
        expected_revision: u64,
        event_id: &str,
        now: &str,
    ) -> Result<GjcLaneRecord> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("lane store poisoned"))?;
        let canonical_id = canonical_lane_id(&state, lane_id);
        let mut record = state
            .lanes
            .get(&canonical_id)
            .cloned()
            .ok_or_else(|| anyhow!("lane not found: {lane_id}"))?;
        if record.revision != expected_revision {
            bail!(
                "lane revision conflict: expected {expected_revision}, current {}",
                record.revision
            );
        }
        let original_len = record.pending_alerts.len();
        record
            .pending_alerts
            .retain(|alert| alert.event_id != event_id);
        if record.pending_alerts.len() == original_len {
            return Ok(record);
        }
        record.revision = record
            .revision
            .checked_add(1)
            .context("revision overflow")?;
        record.updated_at = now.to_string();
        state.lanes.insert(canonical_id, record.clone());
        self.persist_locked(&mut state)?;
        Ok(record)
    }

    fn suppress_historical_failure_alert(
        &self,
        lane_id: &str,
        expected_revision: u64,
        alert: &GjcPendingAlert,
        current: &GjcSdkObservation,
        now: &str,
    ) -> Result<GjcLaneRecord> {
        let causality = alert.turn_failure.as_ref();
        self.mutate(lane_id, expected_revision, now, |mut record| {
            let original_len = record.pending_alerts.len();
            record
                .pending_alerts
                .retain(|pending| pending.event_id != alert.event_id);
            if record.pending_alerts.len() == original_len {
                return Ok(multi_audit(record, vec![]));
            }
            Ok(multi_audit(
                record,
                vec![(
                    GjcAuditKind::HistoricalFailureSuppressed,
                    format!(
                        "historical failure suppressed: command={} turn={} model={} profile={} current_command={} current_turn={} current_model={} current_profile={} current_state={:?}",
                        bounded_identity(causality.and_then(|value| value.command_id.as_deref())),
                        bounded_identity(causality.map(|value| value.turn_id.as_str())),
                        bounded_identity(causality.and_then(|value| value.model.as_deref())),
                        bounded_identity(causality.and_then(|value| value.profile.as_deref())),
                        bounded_identity(current.command_id.as_deref()),
                        bounded_identity(current.turn_id.as_deref()),
                        bounded_identity(current.model.as_deref()),
                        bounded_identity(current.profile.as_deref()),
                        current.turn_state,
                    ),
                )],
            ))
        })
    }

    pub(crate) fn pending_alert_delivery_state(
        &self,
        lane_id: &str,
        event_id: &str,
        destination: &str,
    ) -> Option<GjcPendingAlertDeliveryState> {
        let state = self.state.lock().ok()?;
        let canonical_id = canonical_lane_id(&state, lane_id);
        state
            .lanes
            .get(&canonical_id)?
            .pending_alerts
            .iter()
            .find(|alert| alert.event_id == event_id)
            .and_then(|alert| alert.deliveries.get(destination))
            .map(|delivery| delivery.state)
    }

    pub(crate) fn claim_pending_alert_delivery(
        &self,
        lane_id: &str,
        event_id: &str,
        destination: &str,
    ) -> Result<bool> {
        self.update_pending_alert_delivery(
            lane_id,
            event_id,
            destination,
            GjcPendingAlertDeliveryState::Claimed,
        )
    }

    pub(crate) fn record_pending_alert_delivery(
        &self,
        lane_id: &str,
        event_id: &str,
        destination: &str,
        delivered: bool,
    ) -> Result<bool> {
        self.update_pending_alert_delivery(
            lane_id,
            event_id,
            destination,
            if delivered {
                GjcPendingAlertDeliveryState::Delivered
            } else {
                GjcPendingAlertDeliveryState::Failed
            },
        )
    }

    fn update_pending_alert_delivery(
        &self,
        lane_id: &str,
        event_id: &str,
        destination: &str,
        next_state: GjcPendingAlertDeliveryState,
    ) -> Result<bool> {
        if destination.is_empty()
            || destination.len() > 512
            || destination.chars().any(char::is_control)
        {
            bail!("invalid pending alert destination");
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("lane store poisoned"))?;
        let canonical_id = canonical_lane_id(&state, lane_id);
        let record = state
            .lanes
            .get(&canonical_id)
            .cloned()
            .ok_or_else(|| anyhow!("lane not found: {lane_id}"))?;
        let mut updated = record.clone();
        let alert = updated
            .pending_alerts
            .iter_mut()
            .find(|alert| alert.event_id == event_id)
            .ok_or_else(|| anyhow!("pending alert not found: {event_id}"))?;
        if let Some(existing) = alert.deliveries.get(destination) {
            if existing.state == GjcPendingAlertDeliveryState::Delivered
                || (existing.state == GjcPendingAlertDeliveryState::Claimed
                    && next_state == GjcPendingAlertDeliveryState::Claimed)
            {
                return Ok(true);
            }
        } else if alert.deliveries.len() >= MAX_PENDING_ALERT_DELIVERIES {
            bail!("pending alert delivery map is full");
        }
        let attempts = alert
            .deliveries
            .get(destination)
            .map(|delivery| delivery.attempts)
            .unwrap_or_default()
            .saturating_add((next_state == GjcPendingAlertDeliveryState::Claimed) as u32);
        alert.deliveries.insert(
            destination.to_string(),
            GjcPendingAlertDelivery {
                state: next_state,
                attempts,
                updated_at: now_rfc3339(),
            },
        );
        updated.revision = updated
            .revision
            .checked_add(1)
            .context("revision overflow")?;
        updated.updated_at = now_rfc3339();
        state.lanes.insert(canonical_id, updated);
        self.persist_locked(&mut state)?;
        Ok(true)
    }

    pub fn note_pending_alert_attempt(
        &self,
        lane_id: &str,
        expected_revision: u64,
        event_id: &str,
        now: &str,
    ) -> Result<GjcLaneRecord> {
        self.mutate(lane_id, expected_revision, now, |mut record| {
            let alert = record
                .pending_alerts
                .iter_mut()
                .find(|alert| alert.event_id == event_id)
                .ok_or_else(|| anyhow!("pending alert not found: {event_id}"))?;
            alert.attempts = alert.attempts.saturating_add(1);
            Ok(multi_audit(record, vec![]))
        })
    }

    /// Mark a lane whose SDK session authoritatively no longer exists while
    /// the watch was not yet terminal: runtime-gone phase, retired disposition,
    /// released ownership, and invalidated evidence.
    pub fn mark_runtime_gone(
        &self,
        lane_id: &str,
        expected_revision: u64,
        evidence: &str,
        now: &str,
    ) -> Result<GjcLaneRecord> {
        self.mutate(lane_id, expected_revision, now, |mut record| {
            if record.terminal_disposition.is_some() {
                bail!("lane already carries a terminal disposition");
            }
            record.phase = Some(GjcLanePhase::RuntimeGone);
            record.review_evidence_valid = false;
            record.owner_evidence_valid = false;
            record.evidence_revision += 1;
            record.consecutive_unavailable_polls = 0;
            record.last_query_at = Some(now.to_string());
            record.terminal_disposition = Some(GjcTerminalDisposition {
                kind: GjcTerminalKind::Retired,
                observed_at: now.to_string(),
                evidence: format!("runtime gone: {}", bounded_text(evidence)),
            });
            if record.ownership.state == GjcOwnershipState::Claimed {
                record.ownership.state = GjcOwnershipState::Relinquished;
            }
            Ok(multi_audit(
                record,
                vec![
                    (GjcAuditKind::PhaseChanged, "runtime-gone".to_string()),
                    (
                        GjcAuditKind::TerminalDispositionSet,
                        "retired after authoritative session-not-found".to_string(),
                    ),
                    (
                        GjcAuditKind::EvidenceInvalidated,
                        "review/owner evidence invalidated by runtime-gone".to_string(),
                    ),
                ],
            ))
        })
    }

    /// Remove a terminal ghost watch while preserving the tombstone row and
    /// audit evidence in the durable state file.
    pub fn remove_ghost_watch(
        &self,
        lane_id: &str,
        expected_revision: u64,
        reason: &str,
        now: &str,
    ) -> Result<GjcLaneRecord> {
        self.mutate(lane_id, expected_revision, now, |mut record| {
            if record.watch_removed_at.is_some() {
                bail!("ghost watch already removed");
            }
            if record.terminal_disposition.is_none() {
                bail!("only terminal lanes qualify for ghost watch removal");
            }
            if !record.pending_alerts.is_empty() {
                bail!("pending alerts must be acknowledged before ghost removal");
            }
            record.watch_removed_at = Some(now.to_string());
            record.phase = Some(match record.terminal_disposition.as_ref() {
                Some(disposition) => match disposition.kind {
                    GjcTerminalKind::Complete => GjcLanePhase::Complete,
                    GjcTerminalKind::Failed => GjcLanePhase::Failed,
                    GjcTerminalKind::Retired => GjcLanePhase::Retired,
                },
                None => GjcLanePhase::Retired,
            });
            Ok(multi_audit(
                record,
                vec![(GjcAuditKind::GhostWatchRemoved, bounded_text(reason))],
            ))
        })
    }

    pub fn reclaim_tombstone(
        &self,
        lane_id: &str,
        expected_revision: u64,
        endpoint_generation: u64,
        worktree: Option<String>,
        now: &str,
    ) -> Result<GjcLaneRecord> {
        self.mutate(lane_id, expected_revision, now, |mut record| {
            if record.watch_removed_at.is_none() {
                bail!("lane is not a removed tombstone");
            }
            record.watch_removed_at = None;
            record.terminal_disposition = None;
            record.phase = None;
            record.worktree = worktree;
            record.branch = None;
            record.pr = None;
            record.endpoint_generation = endpoint_generation;
            record.endpoint_episode = 0;
            record.sdk_revision = 0;
            record.last_turn = None;
            record.last_gate = None;
            record.evidence_revision = 0;
            record.review_evidence_valid = true;
            record.owner_evidence_valid = true;
            record.ownership = GjcLaneOwnership::default();
            record.consecutive_unavailable_polls = 0;
            record.endpoint_alerted = false;
            Ok(multi_audit(
                record,
                vec![(
                    GjcAuditKind::LaneRegistered,
                    "removed tombstone reclaimed for native session re-enrollment".into(),
                )],
            ))
        })
    }

    pub fn note_endpoint_rotation(
        &self,
        lane_id: &str,
        expected_revision: u64,
        endpoint_generation: u64,
        now: &str,
    ) -> Result<GjcLaneRecord> {
        self.mutate(lane_id, expected_revision, now, |mut record| {
            if endpoint_generation == record.endpoint_generation {
                return Ok((record, vec![]));
            }
            record.endpoint_generation = endpoint_generation;
            record.endpoint_episode = record.endpoint_episode.saturating_add(1);
            record.endpoint_alerted = false;
            record.review_evidence_valid = false;
            record.owner_evidence_valid = false;
            Ok(multi_audit(
                record,
                vec![(
                    GjcAuditKind::EvidenceInvalidated,
                    "endpoint generation changed; awaiting authoritative query".into(),
                )],
            ))
        })
    }

    /// Manually retire a lane (operator action through the daemon API).
    pub fn retire_lane(
        &self,
        lane_id: &str,
        expected_revision: u64,
        reason: &str,
        now: &str,
    ) -> Result<GjcLaneRecord> {
        self.mutate(lane_id, expected_revision, now, |mut record| {
            if record.terminal_disposition.is_none() {
                record.terminal_disposition = Some(GjcTerminalDisposition {
                    kind: GjcTerminalKind::Retired,
                    observed_at: now.to_string(),
                    evidence: format!("manual retirement: {}", bounded_text(reason)),
                });
                if record.ownership.state == GjcOwnershipState::Claimed {
                    record.ownership.state = GjcOwnershipState::Relinquished;
                }
            }
            record.phase = Some(classify_lane(&record, None));
            Ok(multi_audit(
                record,
                vec![
                    (
                        GjcAuditKind::TerminalDispositionSet,
                        format!("manually retired ({})", bounded_text(reason)),
                    ),
                    (GjcAuditKind::PhaseChanged, "retired".to_string()),
                ],
            ))
        })
    }

    /// Reconcile the stored PR binding against freshly resolved head/base
    /// state. Pushed heads and base changes invalidate stale review/owner
    /// evidence; an unresolvable PR invalidates evidence exactly once.
    pub fn reconcile_pr_binding(
        &self,
        lane_id: &str,
        expected_revision: u64,
        resolved: Option<&GjcPrState>,
        now: &str,
    ) -> Result<(GjcLaneRecord, GjcPrReconcileOutcome)> {
        let mut outcome = GjcPrReconcileOutcome {
            head_changed: false,
            base_changed: false,
            evidence_invalidated: false,
            unresolved: false,
        };
        let record = self.mutate(lane_id, expected_revision, now, |mut record| {
            let Some(binding) = record.pr.clone() else {
                bail!("lane has no PR binding");
            };
            let Some(state) = resolved else {
                outcome.unresolved = true;
                if record.review_evidence_valid || record.owner_evidence_valid {
                    record.review_evidence_valid = false;
                    record.owner_evidence_valid = false;
                    record.evidence_revision += 1;
                    outcome.evidence_invalidated = true;
                    return Ok(multi_audit(
                        record,
                        vec![(
                            GjcAuditKind::EvidenceInvalidated,
                            format!(
                                "pr {}/{} unresolved; stale evidence invalidated",
                                binding.repo, binding.number
                            ),
                        )],
                    ));
                }
                return Ok(multi_audit(record, vec![]));
            };
            let head_sha = state.head_sha.to_ascii_lowercase();
            if head_sha != binding.head_sha {
                outcome.head_changed = true;
            }
            if state.base_branch != binding.base_branch {
                outcome.base_changed = true;
            }
            if !outcome.head_changed && !outcome.base_changed {
                return Ok(multi_audit(record, vec![]));
            }
            if record.review_evidence_valid || record.owner_evidence_valid {
                record.review_evidence_valid = false;
                record.owner_evidence_valid = false;
                record.evidence_revision += 1;
                outcome.evidence_invalidated = true;
            }
            let mut audit = Vec::new();
            if outcome.head_changed {
                audit.push((
                    GjcAuditKind::PrHeadChanged,
                    format!("{} -> {}", binding.head_sha, head_sha),
                ));
            }
            if outcome.base_changed {
                audit.push((
                    GjcAuditKind::PrBaseChanged,
                    format!("{} -> {}", binding.base_branch, state.base_branch),
                ));
            }
            if outcome.evidence_invalidated {
                audit.push((
                    GjcAuditKind::EvidenceInvalidated,
                    "review/owner evidence invalidated by PR movement".to_string(),
                ));
            }
            record.pr = Some(GjcPrBinding {
                repo: binding.repo,
                number: binding.number,
                head_sha,
                base_branch: state.base_branch.clone(),
                bound_at: now.to_string(),
            });
            Ok(multi_audit(record, audit))
        })?;
        Ok((record, outcome))
    }

    /// Append an audit-only note (no record mutation, no revision bump) used
    /// when a reconciler loses a CAS race with a concurrent writer.
    pub fn note_revision_conflict(&self, lane_id: &str, expected_revision: u64, now: &str) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        state.push_audit(GjcReconcileAuditEntry {
            at: now.to_string(),
            lane_id: lane_id.to_string(),
            kind: GjcAuditKind::ReconcileSkippedRevisionConflict,
            detail: format!(
                "expected revision {expected_revision} moved during reconcile; retry next poll"
            ),
        });
        let _ = self.persist_locked(&mut state);
    }

    /// Core optimistic-concurrency mutation. Applies `apply` to the current
    /// record only when `expected_revision` still matches, bumps the revision,
    /// stamps `updated_at`, appends the audit entries the closure returns, and
    /// persists atomically.
    pub fn mutate<F>(
        &self,
        lane_id: &str,
        expected_revision: u64,
        now: &str,
        apply: F,
    ) -> Result<GjcLaneRecord>
    where
        F: FnOnce(GjcLaneRecord) -> Result<(GjcLaneRecord, Vec<(GjcAuditKind, String)>)>,
    {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("lane store poisoned"))?;
        let canonical_id = canonical_lane_id(&state, lane_id);
        let record = state
            .lanes
            .get(&canonical_id)
            .cloned()
            .ok_or_else(|| anyhow!("lane not found: {lane_id}"))?;
        if record.revision != expected_revision {
            bail!(
                "lane revision conflict: expected {expected_revision}, current {}",
                record.revision
            );
        }
        let (mut updated, audit) = apply(record)?;
        updated.revision = updated
            .revision
            .checked_add(1)
            .context("revision overflow")?;
        updated.updated_at = now.to_string();
        for (kind, detail) in audit {
            state.push_audit(GjcReconcileAuditEntry {
                at: now.to_string(),
                lane_id: canonical_id.clone(),
                kind,
                detail,
            });
        }
        state.lanes.insert(canonical_id, updated.clone());
        self.persist_locked(&mut state)?;
        Ok(updated)
    }

    fn persist_locked(&self, state: &mut GjcLaneStateFile) -> Result<()> {
        state.generation = state.generation.wrapping_add(1);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let serialized = serde_json::to_vec_pretty(state).context("serialize lane state")?;
        let tmp_path = self.path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &serialized)
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        std::fs::File::open(&tmp_path)
            .and_then(|file| file.sync_all())
            .with_context(|| format!("failed to sync {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &self.path)
            .with_context(|| format!("failed to persist {}", self.path.display()))?;
        #[cfg(unix)]
        if let Some(parent) = self.path.parent() {
            std::fs::File::open(parent)
                .and_then(|file| file.sync_all())
                .with_context(|| format!("failed to sync {}", parent.display()))?;
        }
        Ok(())
    }
}

impl GjcLaneStateFile {
    fn push_audit(&mut self, entry: GjcReconcileAuditEntry) {
        if self.audit.len() >= MAX_AUDIT_ENTRIES {
            let overflow = self.audit.len() + 1 - MAX_AUDIT_ENTRIES;
            self.audit.drain(0..overflow);
            self.audit_entries_dropped += overflow as u64;
        }
        self.audit.push(entry);
    }
}

fn multi_audit(
    record: GjcLaneRecord,
    audit: Vec<(GjcAuditKind, String)>,
) -> (GjcLaneRecord, Vec<(GjcAuditKind, String)>) {
    (record, audit)
}

fn terminal_kind_from_disposition(disposition: GjcSessionDisposition) -> Option<GjcTerminalKind> {
    match disposition {
        GjcSessionDisposition::Live => None,
        GjcSessionDisposition::Complete => Some(GjcTerminalKind::Complete),
        GjcSessionDisposition::Failed => Some(GjcTerminalKind::Failed),
        GjcSessionDisposition::Retired => Some(GjcTerminalKind::Retired),
    }
}

/// Classify a lane strictly from durable state plus authoritative SDK evidence.
pub fn classify_lane(
    record: &GjcLaneRecord,
    observation: Option<&GjcSdkObservation>,
) -> GjcLanePhase {
    if record.watch_removed_at.is_some() {
        return GjcLanePhase::Retired;
    }
    if let Some(disposition) = &record.terminal_disposition {
        return match disposition.kind {
            GjcTerminalKind::Complete => GjcLanePhase::Complete,
            GjcTerminalKind::Failed => GjcLanePhase::Failed,
            GjcTerminalKind::Retired => GjcLanePhase::Retired,
        };
    }
    let Some(observation) = observation else {
        return record.phase.unwrap_or(GjcLanePhase::Unavailable);
    };
    if observation.session_id != record.sdk_session_id {
        return GjcLanePhase::RuntimeGone;
    }
    match observation.disposition {
        GjcSessionDisposition::Retired => GjcLanePhase::Retired,
        GjcSessionDisposition::Failed => GjcLanePhase::Failed,
        GjcSessionDisposition::Complete => GjcLanePhase::Complete,
        GjcSessionDisposition::Live => {
            if observation.gate_state == Some(GjcGateState::Ready)
                || (observation.turn_state == GjcTurnState::AwaitingInput
                    && observation.prompt_accepted)
            {
                GjcLanePhase::Blocked
            } else if observation.turn_state == GjcTurnState::Running && observation.prompt_accepted
            {
                GjcLanePhase::Active
            } else {
                GjcLanePhase::Idle
            }
        }
    }
}

/// Exponential backoff with a hard ceiling: `initial * 2^failures` capped at
/// `max`. The exponent saturates at 16 doublings so huge failure counts cannot
/// overflow.
pub fn backoff_delay_ms(initial_ms: u64, max_ms: u64, consecutive_failures: u32) -> u64 {
    let initial = initial_ms.max(1);
    let exponent = consecutive_failures.min(16);
    let scaled = initial.saturating_mul(1_u64 << exponent);
    scaled.min(max_ms.max(initial))
}

/// Bounded polling policy shared by the reconciler and health diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcPollingPolicy {
    pub poll_interval_secs: u64,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
    pub max_consecutive_attempts: u32,
    pub ghost_grace_polls: u32,
}

impl Default for GjcPollingPolicy {
    fn default() -> Self {
        Self {
            poll_interval_secs: 30,
            initial_backoff_ms: 500,
            max_backoff_ms: 30_000,
            max_consecutive_attempts: 8,
            ghost_grace_polls: 3,
        }
    }
}

impl GjcPollingPolicy {
    pub fn backoff_after(&self, consecutive_failures: u32) -> Duration {
        Duration::from_millis(backoff_delay_ms(
            self.initial_backoff_ms,
            self.max_backoff_ms,
            consecutive_failures,
        ))
    }

    fn validate(&self) -> Result<()> {
        if self.poll_interval_secs == 0 {
            bail!("gjc lanes poll interval must be positive");
        }
        if self.initial_backoff_ms == 0 {
            bail!("gjc lanes initial backoff must be positive");
        }
        if self.max_backoff_ms < self.initial_backoff_ms {
            bail!("gjc lanes max backoff must not be smaller than initial backoff");
        }
        if self.max_consecutive_attempts == 0 {
            bail!("gjc lanes attempt budget must be positive");
        }
        if self.ghost_grace_polls == 0 {
            bail!("gjc lanes ghost grace must be positive");
        }
        Ok(())
    }
}

/// `[gjc_lanes]` configuration for durable GJC SDK lane ownership (#325).
///
/// Module-owned like `UpdateConfig`; every field defaults so legacy configs
/// without the section parse unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GjcLanesConfig {
    /// Enable durable lane ownership reconciliation in the daemon.
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    /// Reconciler pass interval.
    #[serde(default = "default_gjc_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// First reconnect delay after an unavailable poll.
    #[serde(default = "default_gjc_initial_backoff_ms")]
    pub initial_backoff_ms: u64,
    /// Backoff ceiling.
    #[serde(default = "default_gjc_max_backoff_ms")]
    pub max_backoff_ms: u64,
    /// Bounded attempt budget before polling suspends until manual reconcile.
    #[serde(default = "default_gjc_max_consecutive_attempts")]
    pub max_consecutive_attempts: u32,
    /// Consecutive unavailable polls a terminal watch survives before ghost
    /// removal (definitive session-not-found removes immediately).
    #[serde(default = "default_gjc_ghost_grace_polls")]
    pub ghost_grace_polls: u32,
    /// Optional state-file override; defaults beside the cron state file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_path: Option<PathBuf>,
    /// Additional trusted worktrees whose native SDK endpoints are enrolled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovery_worktrees: Vec<PathBuf>,
    /// PR head/base reconciliation settings.
    #[serde(default)]
    pub pr: GjcLanesPrConfig,
}

impl Default for GjcLanesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_interval_secs: default_gjc_poll_interval_secs(),
            initial_backoff_ms: default_gjc_initial_backoff_ms(),
            max_backoff_ms: default_gjc_max_backoff_ms(),
            max_consecutive_attempts: default_gjc_max_consecutive_attempts(),
            ghost_grace_polls: default_gjc_ghost_grace_polls(),
            state_path: None,
            discovery_worktrees: Vec::new(),
            pr: GjcLanesPrConfig::default(),
        }
    }
}

impl GjcLanesConfig {
    pub fn is_empty(&self) -> bool {
        !self.enabled
            && self.state_path.is_none()
            && self.discovery_worktrees.is_empty()
            && self.pr.is_empty()
    }

    pub fn polling_policy(&self) -> GjcPollingPolicy {
        GjcPollingPolicy {
            poll_interval_secs: self.poll_interval_secs,
            initial_backoff_ms: self.initial_backoff_ms,
            max_backoff_ms: self.max_backoff_ms,
            max_consecutive_attempts: self.max_consecutive_attempts,
            ghost_grace_polls: self.ghost_grace_polls,
        }
    }

    /// Config-level validation shared by `AppConfig::validate`.
    pub fn validate(&self) -> Result<()> {
        self.polling_policy().validate()?;
        self.pr.validate()
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_gjc_poll_interval_secs() -> u64 {
    30
}
fn default_gjc_initial_backoff_ms() -> u64 {
    500
}
fn default_gjc_max_backoff_ms() -> u64 {
    30_000
}
fn default_gjc_max_consecutive_attempts() -> u32 {
    8
}
fn default_gjc_ghost_grace_polls() -> u32 {
    3
}

/// PR reconciliation configuration; token stays env-referenced by name only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GjcLanesPrConfig {
    /// `owner/repo` to resolve PR head/base against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Environment variable name holding an optional GitHub token. The value
    /// itself is never stored in config or logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_env: Option<String>,
    /// GitHub API base (override for tests).
    #[serde(default = "default_gjc_pr_api_base")]
    pub api_base: String,
}

impl GjcLanesPrConfig {
    pub fn is_empty(&self) -> bool {
        self.repo.is_none()
            && self.token_env.is_none()
            && self.api_base == default_gjc_pr_api_base()
    }

    fn validate(&self) -> Result<()> {
        if let Some(repo) = &self.repo
            && (repo.is_empty() || !repo.contains('/') || repo.len() > 200)
        {
            bail!("gjc_lanes.pr.repo must be owner/repo");
        }
        Ok(())
    }
}

fn default_gjc_pr_api_base() -> String {
    "https://api.github.com".to_string()
}

/// Aggregate result of one reconciler pass.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcReconcileOutcome {
    pub examined: usize,
    pub retained: usize,
    pub ghost_watches_removed: usize,
    pub terminal_set: usize,
    pub runtime_gone: usize,
    pub evidence_invalidations: usize,
    pub suspended: usize,
    pub skipped_conflicts: usize,
}

/// Restart-safe reconciler driving durable lane records against the
/// authoritative SDK control plane with bounded polling/backoff.
pub struct GjcReconciler {
    plane: Arc<dyn GjcSdkControlPlane>,
    pr_resolver: Option<Arc<dyn GjcLanePrResolver>>,
    store: SharedGjcLaneStore,
    tx: mpsc::Sender<IncomingEvent>,
    policy: GjcPollingPolicy,
    auto_enrollment: bool,
    discovery_worktrees: Vec<PathBuf>,
}

pub type SharedGjcReconciler = Arc<GjcReconciler>;

impl GjcReconciler {
    pub fn new(
        plane: Arc<dyn GjcSdkControlPlane>,
        pr_resolver: Option<Arc<dyn GjcLanePrResolver>>,
        store: SharedGjcLaneStore,
        tx: mpsc::Sender<IncomingEvent>,
        policy: GjcPollingPolicy,
    ) -> Self {
        Self {
            plane,
            pr_resolver,
            store,
            tx,
            policy,
            auto_enrollment: false,
            discovery_worktrees: Vec::new(),
        }
    }

    pub fn with_auto_enrollment(mut self, worktrees: Vec<PathBuf>) -> Self {
        self.auto_enrollment = true;
        self.discovery_worktrees = worktrees;
        self
    }

    /// Long-running loop: one bounded pass per interval. Runs until the task
    /// is dropped with the daemon shutdown.
    pub async fn run(self: Arc<Self>) {
        let mut ticker =
            tokio::time::interval(Duration::from_secs(self.policy.poll_interval_secs.max(1)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if let Err(error) = self.poll_once(OffsetDateTime::now_utc()).await {
                eprintln!("clawhip gjc reconciler pass failed: {error:#}");
            }
        }
    }

    /// Execute one reconciliation pass over every non-tombstoned lane.
    pub async fn poll_once(&self, now: OffsetDateTime) -> Result<GjcReconcileOutcome> {
        self.policy.validate()?;
        let now_text = now
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
        if self.auto_enrollment {
            self.auto_enroll_native_sessions(&now_text);
        }
        let mut outcome = GjcReconcileOutcome::default();
        let records = self
            .store
            .snapshot_watches(true)
            .into_iter()
            .filter(|record| record.watch_removed_at.is_none())
            .collect::<Vec<_>>();
        for snapshot in records {
            let (snapshot, revalidated) = self.replay_pending_alerts(&snapshot, &now_text).await;
            if snapshot.terminal_disposition.as_ref().map(|d| d.kind)
                == Some(GjcTerminalKind::Retired)
                && snapshot.pending_alerts.is_empty()
            {
                if let Ok(updated) = self.store.remove_ghost_watch(
                    &snapshot.lane_id,
                    snapshot.revision,
                    "retired watch cleanup",
                    &now_text,
                ) {
                    outcome.ghost_watches_removed += 1;
                    self.emit_status(&updated, &now_text);
                }
                continue;
            }
            if snapshot.polling_suspended && self.suspension_defers(&snapshot, now) {
                continue;
            }
            if self.backoff_defers(&snapshot, now) {
                outcome.retained += 1;
                continue;
            }
            outcome.examined += 1;
            let query = GjcLaneQuery {
                sdk_session_id: snapshot.sdk_session_id.clone(),
                worktree: snapshot.worktree.clone(),
                known_endpoint_generation: snapshot.endpoint_generation,
            };
            let expected = snapshot.revision;
            let observation = match revalidated {
                Some(observation) => Ok(observation),
                None => self.plane.query_lane(&query).await,
            };
            match observation {
                Ok(observation) => {
                    if observation.revision <= snapshot.sdk_revision {
                        outcome.retained += 1;
                        continue;
                    }
                    match self.store.apply_observation(
                        &snapshot.lane_id,
                        expected,
                        &observation,
                        &now_text,
                    ) {
                        Ok((record, applied)) => {
                            outcome.retained += 1;
                            let record = self.replay_pending_alerts(&record, &now_text).await.0;
                            if applied.terminal_set.is_some() {
                                outcome.terminal_set += 1;
                                self.emit_status(&record, &now_text);
                                continue;
                            }
                            if applied.evidence_invalidated {
                                outcome.evidence_invalidations += 1;
                            }
                            self.emit_status(&record, &now_text);
                            self.reconcile_pr(&record, &now_text, &mut outcome).await;
                        }
                        Err(error) => {
                            if error.to_string().contains("revision conflict") {
                                outcome.skipped_conflicts += 1;
                                self.store.note_revision_conflict(
                                    &snapshot.lane_id,
                                    expected,
                                    &now_text,
                                );
                            } else {
                                outcome.retained += 1;
                                eprintln!(
                                    "clawhip gjc lane observation retained after validation failure: {}",
                                    bounded_text(&error.to_string())
                                );
                            }
                        }
                    }
                }
                Err(GjcSdkQueryError::SessionNotFound(detail)) => {
                    if snapshot.terminal_disposition.is_some() {
                        // Definitive evidence the terminal session is gone:
                        // this is a ghost watch.
                        match self.store.remove_ghost_watch(
                            &snapshot.lane_id,
                            expected,
                            &format!("terminal session absent from control plane: {detail}"),
                            &now_text,
                        ) {
                            Ok(record) => {
                                outcome.ghost_watches_removed += 1;
                                self.emit_status(&record, &now_text);
                            }
                            Err(error) => {
                                if error.to_string().contains("revision conflict") {
                                    outcome.skipped_conflicts += 1;
                                    self.store.note_revision_conflict(
                                        &snapshot.lane_id,
                                        expected,
                                        &now_text,
                                    );
                                } else if !snapshot.pending_alerts.is_empty() {
                                    outcome.retained += 1;
                                    eprintln!(
                                        "clawhip gjc terminal watch retained with pending alerts: {}",
                                        bounded_text(&error.to_string())
                                    );
                                } else {
                                    return Err(error);
                                }
                            }
                        }
                    } else {
                        match self.store.mark_runtime_gone(
                            &snapshot.lane_id,
                            expected,
                            &detail,
                            &now_text,
                        ) {
                            Ok(record) => {
                                outcome.runtime_gone += 1;
                                outcome.terminal_set += 1;
                                outcome.evidence_invalidations += 1;
                                let _ = self.replay_pending_alerts(&record, &now_text).await;
                                self.emit_status(&record, &now_text);
                            }
                            Err(error) => {
                                if error.to_string().contains("revision conflict") {
                                    outcome.skipped_conflicts += 1;
                                    self.store.note_revision_conflict(
                                        &snapshot.lane_id,
                                        expected,
                                        &now_text,
                                    );
                                } else {
                                    return Err(error);
                                }
                            }
                        }
                    }
                }
                Err(GjcSdkQueryError::StaleEndpointGeneration { observed }) => {
                    match self.store.note_endpoint_rotation(
                        &snapshot.lane_id,
                        expected,
                        observed,
                        &now_text,
                    ) {
                        Ok(record) => {
                            outcome.evidence_invalidations += 1;
                            self.emit_status(&record, &now_text);
                        }
                        Err(error) => {
                            if error.to_string().contains("revision conflict") {
                                outcome.skipped_conflicts += 1;
                                self.store.note_revision_conflict(
                                    &snapshot.lane_id,
                                    expected,
                                    &now_text,
                                );
                            } else {
                                return Err(error);
                            }
                        }
                    }
                }
                Err(GjcSdkQueryError::EndpointUnavailable(detail))
                | Err(GjcSdkQueryError::Timeout(detail)) => {
                    if snapshot.terminal_disposition.is_some() {
                        // Ghost-watch grace: terminal watches survive only a
                        // bounded number of consecutive transport failures.
                        let misses = snapshot.consecutive_unavailable_polls + 1;
                        let (record, _) = match self.store.mark_unavailable(
                            &snapshot.lane_id,
                            expected,
                            &detail,
                            u32::MAX,
                            &now_text,
                        ) {
                            Ok(result) => result,
                            Err(error) => {
                                if error.to_string().contains("revision conflict") {
                                    outcome.skipped_conflicts += 1;
                                    self.store.note_revision_conflict(
                                        &snapshot.lane_id,
                                        expected,
                                        &now_text,
                                    );
                                    continue;
                                }
                                return Err(error);
                            }
                        };
                        let record = self.replay_pending_alerts(&record, &now_text).await.0;
                        if misses >= self.policy.ghost_grace_polls {
                            if !record.pending_alerts.is_empty() {
                                // Never tombstone a watch while an alert is
                                // still waiting for delivery; the pending
                                // payload is the durable replay source.
                                outcome.retained += 1;
                                continue;
                            }
                            match self.store.remove_ghost_watch(
                                &snapshot.lane_id,
                                record.revision,
                                &format!(
                                    "terminal watch exceeded ghost grace after {misses} unavailable polls: {detail}"
                                ),
                                &now_text,
                            ) {
                                Ok(record) => {
                                    outcome.ghost_watches_removed += 1;
                                    self.emit_status(&record, &now_text);
                                }
                                Err(error) => {
                                    if error.to_string().contains("revision conflict") {
                                        outcome.skipped_conflicts += 1;
                                        self.store.note_revision_conflict(
                                            &snapshot.lane_id,
                                            record.revision,
                                            &now_text,
                                        );
                                    } else {
                                        return Err(error);
                                    }
                                }
                            }
                        } else {
                            outcome.retained += 1;
                        }
                    } else {
                        match self.store.mark_unavailable(
                            &snapshot.lane_id,
                            expected,
                            &detail,
                            self.policy.max_consecutive_attempts,
                            &now_text,
                        ) {
                            Ok((record, suspended)) => {
                                let _ = self.replay_pending_alerts(&record, &now_text).await;
                                if suspended {
                                    outcome.suspended += 1;
                                }
                                outcome.retained += 1;
                            }
                            Err(error) => {
                                if error.to_string().contains("revision conflict") {
                                    outcome.skipped_conflicts += 1;
                                    self.store.note_revision_conflict(
                                        &snapshot.lane_id,
                                        expected,
                                        &now_text,
                                    );
                                } else {
                                    return Err(error);
                                }
                            }
                        }
                    }
                }
                Err(GjcSdkQueryError::InvalidState(_)) => {
                    outcome.retained += 1;
                }
                Err(GjcSdkQueryError::Ambiguous(detail)) => {
                    // Fail closed: no classification change without
                    // authoritative evidence.
                    if detail == "multiple ready workflow gates" {
                        outcome.retained += 1;
                        continue;
                    }
                    match self.store.enqueue_endpoint_failure(
                        &snapshot.lane_id,
                        expected,
                        &detail,
                        &now_text,
                    ) {
                        Ok(record) => {
                            let _ = self.replay_pending_alerts(&record, &now_text).await;
                        }
                        Err(error) if error.to_string().contains("revision conflict") => {
                            outcome.skipped_conflicts += 1;
                            self.store.note_revision_conflict(
                                &snapshot.lane_id,
                                expected,
                                &now_text,
                            );
                        }
                        Err(error) => return Err(error),
                    }
                    outcome.retained += 1;
                }
            }
        }
        Ok(outcome)
    }

    fn backoff_defers(&self, record: &GjcLaneRecord, now: OffsetDateTime) -> bool {
        let Some(last_query) = &record.last_query_at else {
            return false;
        };
        let Ok(parsed) = OffsetDateTime::parse(last_query, &Rfc3339) else {
            return false;
        };
        let elapsed_ms = (now - parsed).whole_milliseconds().max(0) as u64;
        elapsed_ms
            < self
                .policy
                .backoff_after(record.consecutive_unavailable_polls)
                .as_millis() as u64
    }

    fn suspension_defers(&self, record: &GjcLaneRecord, now: OffsetDateTime) -> bool {
        let Some(last_query) = &record.last_query_at else {
            return false;
        };
        let Ok(parsed) = OffsetDateTime::parse(last_query, &Rfc3339) else {
            return false;
        };
        (now - parsed).whole_seconds() < self.policy.poll_interval_secs.saturating_mul(60) as i64
    }

    fn auto_enroll_native_sessions(&self, now_text: &str) {
        let known = self.store.snapshot_watches(true);
        for worktree in &self.discovery_worktrees {
            let endpoints = match crate::gjc_sdk::discover_all(
                &crate::gjc_sdk::StateRoot::for_worktree(worktree),
            ) {
                Ok(endpoints) => endpoints,
                Err(_) => {
                    eprintln!(
                        "clawhip GJC auto-enrollment skipped unreadable worktree metadata: {}",
                        worktree.display()
                    );
                    continue;
                }
            };
            for endpoint in endpoints {
                if let Some(record) = known.iter().find(|record| {
                    record.watch_removed_at.is_some()
                        && record.sdk_session_id == endpoint.session_id()
                        && record
                            .terminal_disposition
                            .as_ref()
                            .is_some_and(|disposition| {
                                disposition.kind == GjcTerminalKind::Retired
                                    && disposition.evidence.starts_with("runtime gone:")
                            })
                }) {
                    let _ = self.store.reclaim_tombstone(
                        &record.lane_id,
                        record.revision,
                        endpoint.generation(),
                        Some(worktree.to_string_lossy().into_owned()),
                        now_text,
                    );
                }
                if known.iter().any(|record| {
                    record.watch_removed_at.is_none()
                        && record.sdk_session_id == endpoint.session_id()
                }) {
                    continue;
                }
                if self
                    .store
                    .register_lane(
                        &GjcLaneRegistrationRequest {
                            sdk_session_id: endpoint.session_id().to_string(),
                            worktree: Some(worktree.to_string_lossy().into_owned()),
                            endpoint_generation: Some(endpoint.generation()),
                            owner_id: None,
                            pr: None,
                        },
                        now_text,
                    )
                    .is_err()
                {
                    eprintln!(
                        "clawhip GJC auto-enrollment rejected endpoint in worktree: {}",
                        worktree.display()
                    );
                }
            }
        }
    }

    async fn reconcile_pr(
        &self,
        record: &GjcLaneRecord,
        now_text: &str,
        outcome: &mut GjcReconcileOutcome,
    ) {
        let Some(resolver) = &self.pr_resolver else {
            return;
        };
        let Some(binding) = &record.pr else {
            return;
        };
        let expected = record.revision;
        match resolver.resolve_pr(&binding.repo, binding.number).await {
            Ok(resolved) => {
                match self.store.reconcile_pr_binding(
                    &record.lane_id,
                    expected,
                    resolved.as_ref(),
                    now_text,
                ) {
                    Ok((_, pr_outcome)) => {
                        if pr_outcome.evidence_invalidated {
                            outcome.evidence_invalidations += 1;
                        }
                    }
                    Err(error) => {
                        if error.to_string().contains("revision conflict") {
                            outcome.skipped_conflicts += 1;
                            self.store
                                .note_revision_conflict(&record.lane_id, expected, now_text);
                        }
                    }
                }
            }
            Err(_) => {
                // Transient PR-resolution failure: leave evidence untouched and
                // retry next pass (fail closed rather than invalidating on
                // unreliable data).
            }
        }
    }

    fn emit_status(&self, record: &GjcLaneRecord, now_text: &str) {
        let payload = json!({
            "schema": GJC_LANE_STATUS_SCHEMA,
            "lane_id": record.lane_id,
            "sdk_session_id": record.sdk_session_id,
            "worktree_fingerprint": record.worktree.as_deref().map(fingerprint),
            "phase": record.phase.map(|phase| phase.as_str()),
            "ownership": record.ownership.state,
            "terminal": record.terminal_disposition.as_ref().map(|d| d.kind),
            "revision": record.revision,
            "evidence_revision": record.evidence_revision,
            "watch_removed": record.watch_removed_at.is_some(),
            "observed_at": now_text,
        });
        let _ = self.tx.try_send(IncomingEvent {
            kind: GJC_LANE_STATUS_EVENT.to_string(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload,
        });
    }

    async fn replay_pending_alerts(
        &self,
        initial: &GjcLaneRecord,
        now_text: &str,
    ) -> (GjcLaneRecord, Option<GjcSdkObservation>) {
        let mut current = initial.clone();
        let mut revalidated = None;
        while let Some(alert) = current.pending_alerts.first().cloned() {
            let permit =
                match tokio::time::timeout(ALERT_ACCEPTANCE_TIMEOUT, self.tx.reserve()).await {
                    Ok(Ok(permit)) => permit,
                    _ => break,
                };
            current = match self.store.note_pending_alert_attempt(
                &current.lane_id,
                current.revision,
                &alert.event_id,
                now_text,
            ) {
                Ok(updated) => updated,
                Err(_) => break,
            };
            if alert.kind == "session.failed" {
                let query = GjcLaneQuery {
                    sdk_session_id: current.sdk_session_id.clone(),
                    worktree: current.worktree.clone(),
                    known_endpoint_generation: current.endpoint_generation,
                };
                let mut authoritative = match self.plane.query_lane(&query).await {
                    Ok(observation) => observation,
                    Err(_) => break,
                };
                sanitize_observation_selection(&mut authoritative);
                revalidated = Some(authoritative.clone());
                if authoritative.revision > current.sdk_revision {
                    current = match self.store.apply_observation(
                        &current.lane_id,
                        current.revision,
                        &authoritative,
                        now_text,
                    ) {
                        Ok((updated, _)) => updated,
                        Err(_) => break,
                    };
                }
                match failure_alert_authority(&alert, &authoritative) {
                    FailureAlertAuthority::Current => {}
                    FailureAlertAuthority::Superseded => {
                        let causality = alert.turn_failure.as_ref();
                        current = match self.store.suppress_historical_failure_alert(
                            &current.lane_id,
                            current.revision,
                            &alert,
                            &authoritative,
                            now_text,
                        ) {
                            Ok(updated) => updated,
                            Err(_) => self.store.record(&current.lane_id).unwrap_or(current),
                        };
                        eprintln!(
                            "clawhip gjc historical turn failure suppressed: session={} command={} turn={} current_command={} current_turn={} current_state={:?}",
                            bounded_identity(Some(current.sdk_session_id.as_str())),
                            bounded_identity(
                                causality.and_then(|value| value.command_id.as_deref())
                            ),
                            bounded_identity(causality.map(|value| value.turn_id.as_str())),
                            bounded_identity(authoritative.command_id.as_deref()),
                            bounded_identity(authoritative.turn_id.as_deref()),
                            authoritative.turn_state,
                        );
                        continue;
                    }
                    FailureAlertAuthority::Unknown => break,
                }
            }
            register_alert_delivery_journal(
                &alert.event_id,
                Arc::new(GjcPendingAlertJournal {
                    store: self.store.clone(),
                    lane_id: current.lane_id.clone(),
                    event_id: alert.event_id.clone(),
                }),
            );
            let acceptance = register_alert_acceptance(&alert.event_id);
            permit.send(alert.incoming_event());
            let accepted = matches!(
                tokio::time::timeout(ALERT_ACCEPTANCE_TIMEOUT, acceptance).await,
                Ok(Ok(true))
            );
            if !accepted {
                cancel_alert_acceptance(&alert.event_id);
                clear_alert_delivery_journal(&alert.event_id);
                current = self.store.record(&current.lane_id).unwrap_or(current);
                break;
            }
            clear_alert_delivery_journal(&alert.event_id);
            match self.store.acknowledge_pending_alert(
                &current.lane_id,
                current.revision,
                &alert.event_id,
                now_text,
            ) {
                Ok(updated) => current = updated,
                Err(error) if error.to_string().contains("revision conflict") => {
                    let latest = self.store.record(&current.lane_id).unwrap_or(current);
                    current = match self.store.acknowledge_pending_alert(
                        &latest.lane_id,
                        latest.revision,
                        &alert.event_id,
                        now_text,
                    ) {
                        Ok(updated) => updated,
                        Err(_) => latest,
                    };
                }
                Err(_) => break,
            }
        }
        (current, revalidated)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureAlertAuthority {
    Current,
    Superseded,
    Unknown,
}

fn failure_alert_authority(
    alert: &GjcPendingAlert,
    current: &GjcSdkObservation,
) -> FailureAlertAuthority {
    let Some(causality) = alert.turn_failure.as_ref() else {
        return if current.prompt_accepted
            && matches!(
                current.turn_state,
                GjcTurnState::Running | GjcTurnState::AwaitingInput
            ) {
            FailureAlertAuthority::Superseded
        } else {
            FailureAlertAuthority::Unknown
        };
    };
    let Some(current_turn_id) = current.turn_id.as_deref() else {
        return FailureAlertAuthority::Unknown;
    };
    let turn_matches = causality.turn_id == current_turn_id;
    let command_matches = match (
        causality.command_id.as_deref(),
        current.command_id.as_deref(),
    ) {
        (Some(alert), Some(current)) => alert == current,
        (None, _) => true,
        (Some(_), None) => false,
    };
    if current.prompt_accepted
        && current.turn_state == GjcTurnState::Failed
        && turn_matches
        && command_matches
    {
        FailureAlertAuthority::Current
    } else if current.revision > causality.sdk_revision
        && current.prompt_accepted
        && matches!(
            current.turn_state,
            GjcTurnState::Running | GjcTurnState::AwaitingInput | GjcTurnState::Failed
        )
        && (!turn_matches || !command_matches)
    {
        FailureAlertAuthority::Superseded
    } else {
        FailureAlertAuthority::Unknown
    }
}

fn bounded_identity(value: Option<&str>) -> String {
    value
        .map(sanitize_identity)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn sanitize_identity(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() && !character.is_whitespace())
        .take(128)
        .collect()
}

fn public_selection_label(value: &str) -> String {
    let mut bounded = String::with_capacity(value.len().min(MAX_EVIDENCE_LEN));
    let mut previous_space = false;
    for character in value.chars() {
        let character = if character.is_control() || character.is_whitespace() {
            ' '
        } else {
            character
        };
        if character == ' ' && previous_space {
            continue;
        }
        previous_space = character == ' ';
        bounded.push(character);
        if bounded.chars().count() >= MAX_EVIDENCE_LEN {
            break;
        }
    }
    let bounded = bounded.trim().to_string();
    let lower = bounded.to_ascii_lowercase();
    let compact = lower
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>();
    if lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("api_key")
        || lower.contains("api-key")
        || lower.contains("apikey")
        || lower.contains("authorization")
        || lower.contains("credential")
        || lower.contains("passwd")
        || lower.contains("auth:")
        || lower.contains("auth=")
        || lower.contains("bearer")
        || lower.contains("://")
        || compact.contains("privatekey")
    {
        "details-redacted".to_string()
    } else {
        bounded
    }
}

fn sanitize_observation_selection(observation: &mut GjcSdkObservation) {
    observation.model = observation.model.as_deref().map(public_selection_label);
    observation.profile = observation.profile.as_deref().map(public_selection_label);
}

fn validate_registration(request: &GjcLaneRegistrationRequest) -> Result<()> {
    if !valid_session_id(&request.sdk_session_id) {
        bail!("invalid sdk session id");
    }
    if let Some(worktree) = &request.worktree
        && !valid_worktree(worktree)
    {
        bail!("invalid worktree path");
    }
    if let Some(owner_id) = &request.owner_id
        && !valid_id(owner_id)
    {
        bail!("invalid owner id");
    }
    if let Some(pr) = &request.pr {
        if pr.repo.is_empty() || !pr.repo.contains('/') {
            bail!("invalid PR repo binding");
        }
        if pr.number == 0 {
            bail!("invalid PR number");
        }
        if !valid_hex_sha(&pr.head_sha) {
            bail!("invalid PR head sha");
        }
        if pr.base_branch.is_empty()
            || pr.base_branch.len() > 128
            || pr.base_branch.chars().any(|ch| ch.is_control())
        {
            bail!("invalid PR base branch");
        }
    }
    Ok(())
}

fn validate_loaded_state(state: &GjcLaneStateFile) -> Result<()> {
    if state.lanes.len() > MAX_LANES {
        bail!("lane-state contains too many lanes");
    }
    for (lane_id, record) in &state.lanes {
        if !valid_session_id(&record.sdk_session_id)
            || lane_id.is_empty()
            || lane_id.len() > MAX_SESSION_LEN
            || lane_id.chars().any(|ch| ch.is_control())
        {
            bail!("lane-state contains an invalid lane identity");
        }
        if let Some(worktree) = &record.worktree
            && !valid_worktree(worktree)
        {
            bail!("lane-state contains an invalid worktree");
        }
        if let Some(gate) = &record.last_gate
            && (gate
                .gate_id
                .as_deref()
                .is_none_or(|gate_id| !valid_gate_id(gate_id))
                || gate.revision == 0)
        {
            bail!("lane-state contains an invalid gate identity");
        }
        if record.pending_alerts.len() > MAX_PENDING_ALERTS {
            bail!("lane-state contains too many pending alerts");
        }
        for alert in &record.pending_alerts {
            if alert.event_id.is_empty()
                || alert.event_id.len() > 256
                || alert.event_id.chars().any(|ch| ch.is_control())
                || alert.payload.to_string().len() > 64 * 1024
                || alert.deliveries.len() > MAX_PENDING_ALERT_DELIVERIES
            {
                bail!("lane-state contains an invalid pending alert");
            }
            if alert.deliveries.keys().any(|destination| {
                destination.is_empty()
                    || destination.len() > 512
                    || destination.chars().any(char::is_control)
            }) {
                bail!("lane-state contains an invalid pending alert destination");
            }
            if let Some(causality) = &alert.turn_failure {
                crate::gjc::model::SessionId::new(causality.session_id.clone())
                    .map_err(|_| anyhow!("lane-state contains invalid failure session identity"))?;
                crate::gjc::model::TurnId::new(causality.turn_id.clone())
                    .map_err(|_| anyhow!("lane-state contains invalid failure turn identity"))?;
                if let Some(command_id) = &causality.command_id {
                    crate::gjc::model::CommandId::new(command_id.clone()).map_err(|_| {
                        anyhow!("lane-state contains invalid failure command identity")
                    })?;
                }
                if causality
                    .model
                    .as_deref()
                    .is_some_and(|value| bounded_text(value) != value)
                    || causality
                        .profile
                        .as_deref()
                        .is_some_and(|value| bounded_text(value) != value)
                {
                    bail!("lane-state contains invalid failure causality");
                }
            }
        }
    }
    Ok(())
}

/// Build the health/status diagnostic snapshot for the daemon API.
pub fn health_json(store: &SharedGjcLaneStore, plane_registered: bool) -> Value {
    let records = store.snapshot();
    let mut phases: BTreeMap<String, usize> = BTreeMap::new();
    for record in &records {
        if record.watch_removed_at.is_some() || record.terminal_disposition.is_some() {
            continue;
        }
        let key = record
            .phase
            .map(|phase| phase.as_str().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        *phases.entry(key).or_insert(0) += 1;
    }
    let active = records
        .iter()
        .filter(|r| r.watch_removed_at.is_none() && r.terminal_disposition.is_none())
        .count();
    let removed = records
        .iter()
        .filter(|record| record.watch_removed_at.is_some())
        .count();
    json!({
        "schema": GJC_LANE_HEALTH_SCHEMA,
        "store": {
            "status": store.status(),
            "generation": store.generation(),
            "active_watches": active,
            "removed_watches": removed,
            "audit_entries": store.audit().len(),
            "audit_entries_dropped": store.audit_entries_dropped(),
        },
        "phases": phases,
        "control_plane_registered": plane_registered,
    })
}

// ---- Transport-independent surfaces -----------------------------------

/// Durable lane-state file location: beside the cron state file, mirroring
/// the tmux watch registry layout.
pub fn default_gjc_lane_state_path(cron_state_path: &Path) -> PathBuf {
    cron_state_path.with_file_name("gjc-lane-state.json")
}

/// Control-plane registration seam. The repaired SDK control-plane track
/// (#323, after the #328 transport repair lands) installs its authoritative
/// client here; until then the daemon keeps reconciliation idle and reports
/// the gap honestly in health output rather than fabricating evidence.
static REGISTERED_CONTROL_PLANE: Mutex<Option<Arc<dyn GjcSdkControlPlane>>> = Mutex::new(None);

/// Install the process-wide control plane. Returns `false` when a plane is
/// already registered (first registration wins).
#[allow(dead_code)] // invoked by the control-plane track once its transport contract is repaired
pub fn register_gjc_control_plane(plane: Arc<dyn GjcSdkControlPlane>) -> bool {
    let mut slot = match REGISTERED_CONTROL_PLANE.lock() {
        Ok(slot) => slot,
        Err(_) => return false,
    };
    if slot.is_some() {
        return false;
    }
    *slot = Some(plane);
    true
}

pub(crate) fn take_registered_control_plane() -> Option<Arc<dyn GjcSdkControlPlane>> {
    let mut slot = REGISTERED_CONTROL_PLANE.lock().ok()?;
    slot.take()
}
/// GitHub-backed PR head/base resolver used for stale-evidence invalidation.
pub struct GithubApiPrResolver {
    http: reqwest::Client,
    api_base: String,
    token_env: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PrHeadPayload {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct PrBasePayload {
    #[serde(rename = "ref")]
    ref_name: String,
}

#[derive(Debug, Deserialize)]
struct PrPayload {
    state: String,
    head: PrHeadPayload,
    base: PrBasePayload,
}

impl GithubApiPrResolver {
    /// Build the resolver from `[gjc_lanes.pr]` configuration. Returns `None`
    /// when no `repo` is configured (PR reconciliation stays dormant). The
    /// token is resolved per call from `token_env` and never persisted or
    /// logged.
    pub fn from_config(config: &GjcLanesPrConfig) -> Result<Option<Arc<Self>>> {
        let Some(_repo) = &config.repo else {
            return Ok(None);
        };
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("github pr resolver client construction")?;
        Ok(Some(Arc::new(Self {
            http,
            api_base: config.api_base.clone(),
            token_env: config.token_env.clone(),
        })))
    }

    fn bearer_token(&self) -> Option<String> {
        let env_name = self.token_env.as_ref()?;
        let value = std::env::var(env_name).ok()?;
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }
}

#[async_trait]
impl GjcLanePrResolver for GithubApiPrResolver {
    async fn resolve_pr(&self, repo: &str, number: u64) -> Result<Option<GjcPrState>> {
        let url = format!(
            "{}/repos/{repo}/pulls/{number}",
            self.api_base.trim_end_matches('/')
        );
        let mut request = self
            .http
            .get(&url)
            .header(reqwest::header::USER_AGENT, "clawhip")
            .header(reqwest::header::ACCEPT, "application/vnd.github+json");
        if let Some(token) = self.bearer_token() {
            request = request.bearer_auth(token);
        }
        let response = tokio::time::timeout(Duration::from_secs(10), request.send())
            .await
            .map_err(|_| anyhow!("github pr resolution timed out"))?
            .context("github pr resolution request failed")?;
        match response.status() {
            status if status.is_success() => {
                let payload: PrPayload = response.json().await.context("malformed pr payload")?;
                if payload.state == "open" {
                    Ok(Some(GjcPrState {
                        number,
                        head_sha: payload.head.sha.to_ascii_lowercase(),
                        base_branch: payload.base.ref_name,
                    }))
                } else {
                    Ok(None)
                }
            }
            reqwest::StatusCode::NOT_FOUND => Ok(None),
            status => bail!("github pr resolution failed with {status}"),
        }
    }
}

// ---- CLI surfaces (daemon-mediated, public-safe) ----------------------

/// Render `clawhip gjc status`: health plus retained watches.
pub fn render_status(health: &Value, lanes: &Value, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "health": health,
                "lanes": lanes,
            }))
            .unwrap_or_else(|_| "{}".to_string())
        );
        return;
    }
    let store = &health["store"];
    println!(
        "gjc lanes: active={} removed={} audit={} dropped={} generation={}",
        store["active_watches"],
        store["removed_watches"],
        store["audit_entries"],
        store["audit_entries_dropped"],
        store["generation"],
    );
    println!(
        "control_plane_registered: {}",
        health["control_plane_registered"]
    );
    if let Some(lane_list) = lanes["lanes"].as_array() {
        for lane in lane_list {
            println!(
                "  {} session={} phase={} ownership={} terminal={} revision={} evidence_rev={}",
                lane["lane_id"],
                lane["sdk_session_id"],
                lane["phase"].as_str().unwrap_or("unknown"),
                lane["ownership"]["state"].as_str().unwrap_or("unclaimed"),
                lane["terminal_disposition"]["kind"].as_str().unwrap_or("-"),
                lane["revision"],
                lane["evidence_revision"],
            );
        }
    }
}

/// Render one reconciler pass outcome for `clawhip gjc reconcile`.
pub fn render_reconcile(outcome: &GjcReconcileOutcome, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(outcome).unwrap_or_else(|_| "{}".to_string())
        );
        return;
    }
    println!(
        "reconcile: examined={} retained={} terminal_set={} runtime_gone={} ghost_removed={} invalidated={} suspended={} skipped={}",
        outcome.examined,
        outcome.retained,
        outcome.terminal_set,
        outcome.runtime_gone,
        outcome.ghost_watches_removed,
        outcome.evidence_invalidations,
        outcome.suspended,
        outcome.skipped_conflicts,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ts(secs: i64) -> String {
        OffsetDateTime::from_unix_timestamp(secs)
            .expect("timestamp")
            .format(&Rfc3339)
            .expect("rfc3339")
    }

    fn registration(session: &str) -> GjcLaneRegistrationRequest {
        GjcLaneRegistrationRequest {
            sdk_session_id: session.to_string(),
            worktree: Some("/tmp/worktree".to_string()),
            endpoint_generation: Some(3),
            owner_id: None,
            pr: None,
        }
    }

    fn observation(turn: GjcTurnState, disposition: GjcSessionDisposition) -> GjcSdkObservation {
        GjcSdkObservation {
            session_id: "sess-1".to_string(),
            worktree: Some("/tmp/worktree".to_string()),
            branch: Some("feature/test".to_string()),
            branch_observed: false,
            endpoint_generation: 3,
            revision: 10,
            turn_state: turn,
            turn_id: Some("turn-1".to_string()),
            command_id: Some("command-1".to_string()),
            prompt_accepted: matches!(
                turn,
                GjcTurnState::Running | GjcTurnState::AwaitingInput | GjcTurnState::Failed
            ),
            model: Some("test-model".to_string()),
            profile: Some("test-profile".to_string()),
            gate_state: None,
            gate_section_present: true,
            gate_id: None,
            gate_revision: 0,
            gate_kind: None,
            gate_workflow_id: None,
            gate_title: None,
            gate_options: Vec::new(),
            disposition,
            error_summary: None,
        }
    }

    fn store_with_lane(session: &str) -> (tempfile::TempDir, SharedGjcLaneStore, String) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            Arc::new(GjcLaneStore::open(&dir.path().join("gjc-lane-state.json")).expect("open"));
        let record = store
            .register_lane(&registration(session), &ts(1_000))
            .expect("register");
        (dir, store, record.lane_id)
    }

    #[test]
    fn classification_covers_every_phase_from_authoritative_evidence() {
        let (_dir, store, lane_id) = store_with_lane("sess-1");
        let record = store.record(&lane_id).expect("record");

        // No observation yet -> unknown phase preserved as unset/unavailable.
        assert_eq!(classify_lane(&record, None), GjcLanePhase::Unavailable);

        // Idle / Active / Blocked from live evidence.
        assert_eq!(
            classify_lane(
                &record,
                Some(&observation(
                    GjcTurnState::Idle,
                    GjcSessionDisposition::Live
                ))
            ),
            GjcLanePhase::Idle
        );
        assert_eq!(
            classify_lane(
                &record,
                Some(&observation(
                    GjcTurnState::Running,
                    GjcSessionDisposition::Live
                ))
            ),
            GjcLanePhase::Active
        );
        let mut blocked = observation(GjcTurnState::Idle, GjcSessionDisposition::Live);
        blocked.gate_state = Some(GjcGateState::Ready);
        assert_eq!(
            classify_lane(&record, Some(&blocked)),
            GjcLanePhase::Blocked
        );
        let mut awaiting = observation(GjcTurnState::AwaitingInput, GjcSessionDisposition::Live);
        awaiting.gate_state = None;
        assert_eq!(
            classify_lane(&record, Some(&awaiting)),
            GjcLanePhase::Blocked
        );

        // Terminal dispositions from SDK evidence.
        assert_eq!(
            classify_lane(
                &record,
                Some(&observation(
                    GjcTurnState::Complete,
                    GjcSessionDisposition::Complete
                ))
            ),
            GjcLanePhase::Complete
        );
        assert_eq!(
            classify_lane(
                &record,
                Some(&observation(
                    GjcTurnState::Failed,
                    GjcSessionDisposition::Failed
                ))
            ),
            GjcLanePhase::Failed
        );
        assert_eq!(
            classify_lane(
                &record,
                Some(&observation(
                    GjcTurnState::Idle,
                    GjcSessionDisposition::Retired
                ))
            ),
            GjcLanePhase::Retired
        );

        // Session identity mismatch is authoritative runtime-gone evidence.
        let mut alien = observation(GjcTurnState::Idle, GjcSessionDisposition::Live);
        alien.session_id = "other".to_string();
        assert_eq!(
            classify_lane(&record, Some(&alien)),
            GjcLanePhase::RuntimeGone
        );

        // Tombstoned watches report retired regardless of evidence.
        let _ = store.apply_observation(
            &lane_id,
            record.revision,
            &observation(GjcTurnState::Complete, GjcSessionDisposition::Complete),
            &ts(2_000),
        );
        let terminal = store.record(&lane_id).expect("record");
        let removed = store.remove_ghost_watch(&lane_id, terminal.revision, "test", &ts(3_000));
        assert!(removed.is_ok());
        let tombstone = store.record(&lane_id).expect("record");
        assert_eq!(classify_lane(&tombstone, None), GjcLanePhase::Retired);
    }

    #[test]
    fn backoff_is_bounded_and_exponential_then_capped() {
        assert_eq!(backoff_delay_ms(500, 30_000, 0), 500);
        assert_eq!(backoff_delay_ms(500, 30_000, 1), 1_000);
        assert_eq!(backoff_delay_ms(500, 30_000, 2), 2_000);
        assert_eq!(backoff_delay_ms(500, 30_000, 6), 30_000, "capped at max");
        assert_eq!(
            backoff_delay_ms(500, 30_000, 40),
            30_000,
            "huge counts stay capped"
        );
        assert_eq!(
            backoff_delay_ms(500, 100, 3),
            500,
            "cap never floors below the initial delay"
        );
        assert_eq!(
            backoff_delay_ms(0, 30_000, 0),
            1,
            "zero initial floors at 1ms"
        );
    }

    #[test]
    fn store_roundtrip_and_restart_reload_preserve_records_and_audit() {
        let (dir, store, lane_id) = store_with_lane("restart-sess");
        let path = dir.path().join("gjc-lane-state.json");
        let _ = store.claim_ownership(&lane_id, 1, "owner-a", &ts(1_100));
        let _ = store.apply_observation(
            &lane_id,
            2,
            &observation(GjcTurnState::Running, GjcSessionDisposition::Live),
            &ts(1_200),
        );

        // Simulate daemon restart: drop the store, reopen from disk.
        drop(store);
        let reopened = GjcLaneStore::open(&path).expect("reopen");
        match reopened.status() {
            GjcLaneStoreStatus::Loaded { lanes } => assert_eq!(*lanes, 1),
            other => panic!("expected loaded status, got {other:?}"),
        }
        let record = reopened.record(&lane_id).expect("record survives restart");
        assert_eq!(record.sdk_session_id, "restart-sess");
        assert_eq!(record.endpoint_generation, 3);
        assert_eq!(record.revision, 3);
        assert_eq!(record.ownership.state, GjcOwnershipState::Claimed);
        assert_eq!(
            record.last_turn.as_ref().expect("turn").state,
            GjcTurnState::Running
        );
        assert!(
            reopened
                .audit()
                .iter()
                .any(|entry| entry.kind == GjcAuditKind::LaneRegistered)
        );
        assert!(dir.path().join("gjc-lane-state.json").exists());
    }

    #[test]
    fn cas_revision_conflict_rejects_lost_updates_and_audits_skips() {
        let (_dir, store, lane_id) = store_with_lane("cas-sess");
        let winner = store.claim_ownership(&lane_id, 1, "winner", &ts(1_050));
        assert!(winner.is_ok());
        let loser = store.claim_ownership(&lane_id, 1, "loser", &ts(1_051));
        let error = loser.expect_err("stale revision must lose the race");
        assert!(error.to_string().contains("revision conflict"));
        let record = store.record(&lane_id).expect("record");
        assert_eq!(record.ownership.owner_id.as_deref(), Some("winner"));
        store.note_revision_conflict(&lane_id, 1, &ts(1_052));
        assert!(
            store
                .audit()
                .iter()
                .any(|entry| entry.kind == GjcAuditKind::ReconcileSkippedRevisionConflict)
        );
    }

    #[test]
    fn terminal_transitions_are_final_and_release_ownership() {
        let (_dir, store, lane_id) = store_with_lane("sess-1");
        let _ = store.claim_ownership(&lane_id, 1, "owner", &ts(1_100));
        let (record, outcome) = store
            .apply_observation(
                &lane_id,
                2,
                &observation(GjcTurnState::Complete, GjcSessionDisposition::Complete),
                &ts(1_200),
            )
            .expect("apply");
        assert_eq!(outcome.terminal_set, Some(GjcTerminalKind::Complete));
        assert_eq!(record.ownership.state, GjcOwnershipState::Relinquished);
        let reclaim = store.claim_ownership(&lane_id, record.revision, "again", &ts(1_300));
        assert!(reclaim.unwrap_err().to_string().contains("terminal"));
        let revive = store.mark_runtime_gone(&lane_id, record.revision, "late", &ts(1_301));
        assert!(
            revive
                .unwrap_err()
                .to_string()
                .contains("terminal disposition")
        );
    }

    #[test]
    fn failed_alert_is_staged_before_send_and_survives_restart() {
        let (dir, store, lane_id) = store_with_lane("failed-alert");
        let record = store.record(&lane_id).expect("record");
        let (failed, _) = store
            .apply_observation(
                &lane_id,
                record.revision,
                &observation(GjcTurnState::Failed, GjcSessionDisposition::Failed),
                &ts(1_200),
            )
            .expect("failed observation");
        assert_eq!(failed.pending_alerts.len(), 1);
        assert_eq!(failed.pending_alerts[0].kind, "session.failed");
        let event_id = failed.pending_alerts[0].event_id.clone();

        drop(store);
        let reopened = GjcLaneStore::open(&dir.path().join("gjc-lane-state.json")).expect("reopen");
        let restored = reopened.record(&lane_id).expect("restored record");
        assert_eq!(restored.pending_alerts.len(), 1);
        assert_eq!(restored.pending_alerts[0].event_id, event_id);
    }

    #[test]
    fn legacy_failed_alerts_migrate_typed_causality_on_restart() {
        let (dir, store, lane_id) = store_with_lane("legacy-failure");
        let record = store.record(&lane_id).expect("record");
        let (failed, _) = store
            .apply_observation(
                &lane_id,
                record.revision,
                &observation(GjcTurnState::Failed, GjcSessionDisposition::Live),
                &ts(1_200),
            )
            .expect("failed observation");
        assert!(failed.pending_alerts[0].turn_failure.is_some());
        drop(store);

        let path = dir.path().join("gjc-lane-state.json");
        let mut persisted: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read state")).expect("json");
        persisted["lanes"][&lane_id]["pending_alerts"][0]
            .as_object_mut()
            .expect("pending alert")
            .remove("turn_failure");
        std::fs::write(&path, serde_json::to_vec(&persisted).expect("serialize")).expect("write");

        let reopened = GjcLaneStore::open(&path).expect("reopen");
        let migrated = reopened.record(&lane_id).expect("record");
        let causality = migrated.pending_alerts[0]
            .turn_failure
            .as_ref()
            .expect("migrated causality");
        assert_eq!(causality.sdk_revision, migrated.sdk_revision);
        assert_eq!(causality.turn_id, "turn-1");
    }

    #[test]
    fn legacy_multi_failure_migration_preserves_each_alert_identity() {
        let (dir, store, lane_id) = store_with_lane("legacy-multi-failure");
        let record = store.record(&lane_id).expect("record");
        let mut first = observation(GjcTurnState::Failed, GjcSessionDisposition::Live);
        first.revision = 10;
        first.turn_id = Some("turn-first".to_string());
        first.command_id = Some("command-first".to_string());
        let (record, _) = store
            .apply_observation(&lane_id, record.revision, &first, &ts(1_200))
            .expect("first failure");
        let mut second = first;
        second.revision = 11;
        second.turn_id = Some("turn-second".to_string());
        second.command_id = Some("command-second".to_string());
        let (record, _) = store
            .apply_observation(&lane_id, record.revision, &second, &ts(1_300))
            .expect("second failure");
        assert_eq!(record.pending_alerts.len(), 2);
        drop(store);

        let path = dir.path().join("gjc-lane-state.json");
        let mut persisted: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read state")).expect("json");
        for alert in persisted["lanes"][&lane_id]["pending_alerts"]
            .as_array_mut()
            .expect("pending alerts")
        {
            alert
                .as_object_mut()
                .expect("pending alert")
                .remove("turn_failure");
        }
        std::fs::write(&path, serde_json::to_vec(&persisted).expect("serialize")).expect("write");

        let reopened = GjcLaneStore::open(&path).expect("reopen");
        let migrated = reopened.record(&lane_id).expect("record");
        let identities = migrated
            .pending_alerts
            .iter()
            .map(|alert| {
                let causality = alert.turn_failure.as_ref().expect("causality");
                (
                    causality.turn_id.as_str(),
                    causality.command_id.as_deref(),
                    causality.sdk_revision,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            identities,
            vec![
                ("turn-first", Some("command-first"), 10),
                ("turn-second", Some("command-second"), 11),
            ]
        );
    }

    #[test]
    fn bridge_failure_is_staged_in_the_durable_authority_queue() {
        let (_dir, store, lane_id) = store_with_lane("bridge-failure");
        let record = store.record(&lane_id).expect("record");
        let mut bridge = GjcEventBridge::new();
        let accepted = GjcSdkStateSnapshot {
            session_id: record.sdk_session_id.clone(),
            revision: 9,
            turn: Some(GjcSdkTurn {
                id: "bridge-turn".to_string(),
                state: GjcSdkTurnPhase::Active,
                prompt_accepted: true,
                attempt: 0,
                error_summary: None,
            }),
            prompt: Some(GjcSdkPrompt {
                command_id: "bridge-command".to_string(),
                status: GjcSdkPromptStatus::Accepted,
            }),
            model: Some("gpt-5.6-sol".to_string()),
            profile: Some("gpt-heavy".to_string()),
            repo_path: record.worktree.clone(),
            worktree_path: record.worktree.clone(),
            ..GjcSdkStateSnapshot::default()
        };
        bridge.observe(&accepted).expect("accepted prompt");
        let mut failed = accepted;
        failed.revision = 10;
        failed.prompt = None;
        failed.turn.as_mut().expect("turn").state = GjcSdkTurnPhase::Failed;
        failed.turn.as_mut().expect("turn").prompt_accepted = false;
        failed.turn.as_mut().expect("turn").error_summary =
            Some("Prompt submission failed".to_string());
        let failure = bridge
            .observe(&failed)
            .expect("failed turn")
            .events
            .into_iter()
            .find(|event| event.kind == "session.failed")
            .expect("reducer-proven failure");
        let direct_event_id = failure.payload["event_id"]
            .as_str()
            .expect("event id")
            .to_string();
        let staged = store
            .stage_bridge_failure(&lane_id, record.revision, &failure, &ts(1_200))
            .expect("stage");
        assert_eq!(staged.pending_alerts.len(), 1);
        assert_eq!(staged.pending_alerts[0].event_id, direct_event_id);
        let causality = staged.pending_alerts[0]
            .turn_failure
            .as_ref()
            .expect("causality");
        assert_eq!(causality.command_id.as_deref(), Some("bridge-command"));
        assert_eq!(causality.turn_id, "bridge-turn");
    }

    #[test]
    fn selection_context_is_bounded_and_redacted_before_persistence() {
        let (_dir, store, lane_id) = store_with_lane("selection-redaction");
        let record = store.record(&lane_id).expect("record");
        let mut failed = observation(GjcTurnState::Failed, GjcSessionDisposition::Live);
        failed.model = Some("authorization=Bearer raw-secret\n".to_string());
        failed.profile = Some("https://endpoint.invalid/private_key".to_string());
        let (staged, _) = store
            .apply_observation(&lane_id, record.revision, &failed, &ts(1_200))
            .expect("apply");
        let turn = staged.last_turn.as_ref().expect("turn");
        assert_eq!(turn.model.as_deref(), Some("details-redacted"));
        assert_eq!(turn.profile.as_deref(), Some("details-redacted"));
        let serialized = serde_json::to_string(&staged).expect("serialize");
        assert!(!serialized.contains("raw-secret"));
        assert!(!serialized.contains("endpoint.invalid"));
        assert!(!serialized.contains("private_key"));
    }

    #[tokio::test]
    async fn failed_turn_replaced_in_same_session_is_historical_before_delivery() {
        let (_dir, store, lane_id) = store_with_lane("profile-replacement");
        let record = store.record(&lane_id).expect("record");
        let mut failed = observation(GjcTurnState::Failed, GjcSessionDisposition::Live);
        failed.command_id = Some("command-glm-failed".to_string());
        failed.turn_id = Some("turn-glm-failed".to_string());
        failed.model = Some("glm-superseded".to_string());
        failed.profile = Some("glm-superseded".to_string());
        let (failed_record, _) = store
            .apply_observation(&lane_id, record.revision, &failed, &ts(1_200))
            .expect("failed observation");
        assert_eq!(failed_record.pending_alerts.len(), 1);

        let mut replacement = observation(GjcTurnState::Running, GjcSessionDisposition::Live);
        replacement.revision = 11;
        replacement.command_id = Some("command-gpt-heavy-current".to_string());
        replacement.turn_id = Some("turn-gpt-heavy-current".to_string());
        replacement.model = Some("gpt-5.6-sol".to_string());
        replacement.profile = Some("gpt-heavy".to_string());
        let (replacement_record, _) = store
            .apply_observation(&lane_id, failed_record.revision, &replacement, &ts(1_300))
            .expect("replacement observation");
        assert_eq!(replacement_record.pending_alerts.len(), 1);

        let plane = Arc::new(ScriptedPlane {
            responses: Mutex::new(VecDeque::from(vec![
                Ok(replacement.clone()),
                Ok(replacement),
            ])),
        });
        let (tx, sink, mut rx) = event_channel(8);
        let reconciler =
            GjcReconciler::new(plane, None, store.clone(), tx, GjcPollingPolicy::default());
        reconciler.poll_once(fixed_now(10_000)).await.expect("poll");
        pump_events(&mut rx, &sink);

        assert!(
            sink.lock()
                .expect("sink")
                .iter()
                .all(|event| event.kind != "session.failed")
        );
        assert!(
            store
                .record(&lane_id)
                .expect("record")
                .pending_alerts
                .is_empty()
        );
        assert!(store.audit().iter().any(|entry| {
            entry.kind == GjcAuditKind::HistoricalFailureSuppressed
                && entry.detail.contains("command-glm-failed")
                && entry.detail.contains("command-gpt-heavy-current")
                && entry.detail.contains("glm-superseded")
                && entry.detail.contains("gpt-heavy")
        }));
    }

    #[tokio::test]
    async fn replacement_between_failure_detection_and_delivery_suppresses_page() {
        let (_dir, store, lane_id) = store_with_lane("delivery-race");
        let mut failed = observation(GjcTurnState::Failed, GjcSessionDisposition::Live);
        failed.command_id = Some("command-superseded".to_string());
        failed.turn_id = Some("turn-superseded".to_string());
        let mut replacement = observation(GjcTurnState::Running, GjcSessionDisposition::Live);
        replacement.revision = 11;
        replacement.command_id = None;
        replacement.turn_id = Some("turn-replacement".to_string());
        let plane = Arc::new(ScriptedPlane {
            responses: Mutex::new(VecDeque::from(vec![Ok(failed), Ok(replacement)])),
        });
        let (tx, sink, mut rx) = event_channel(8);
        let reconciler =
            GjcReconciler::new(plane, None, store.clone(), tx, GjcPollingPolicy::default());
        reconciler.poll_once(fixed_now(20_000)).await.expect("poll");
        pump_events(&mut rx, &sink);

        assert!(
            sink.lock()
                .expect("sink")
                .iter()
                .all(|event| event.kind != "session.failed")
        );
        let record = store.record(&lane_id).expect("record");
        assert!(record.pending_alerts.is_empty());
        assert!(store.audit().iter().any(|entry| {
            entry.kind == GjcAuditKind::HistoricalFailureSuppressed
                && entry.detail.contains("turn-superseded")
                && entry.detail.contains("turn-replacement")
        }));
    }

    #[tokio::test]
    async fn genuinely_current_failed_turn_alerts_once_with_bounded_identity() {
        let (_dir, store, lane_id) = store_with_lane("current-failure");
        let mut failed = observation(GjcTurnState::Failed, GjcSessionDisposition::Live);
        failed.command_id = Some("command-current".to_string());
        failed.turn_id = Some("turn-current".to_string());
        let plane = Arc::new(ScriptedPlane {
            responses: Mutex::new(VecDeque::from(vec![Ok(failed.clone()), Ok(failed)])),
        });
        let (tx, sink, mut rx) = event_channel(8);
        let callback_sink = sink.clone();
        let callback = tokio::spawn(async move {
            if let Some(event) = rx.recv().await {
                callback_sink.lock().expect("sink").push(event.clone());
                crate::dispatch::resolve_alert_acceptance_id(
                    event.payload["event_id"].as_str().expect("event id"),
                    true,
                );
            }
        });
        let reconciler =
            GjcReconciler::new(plane, None, store.clone(), tx, GjcPollingPolicy::default());
        reconciler.poll_once(fixed_now(30_000)).await.expect("poll");
        callback.await.expect("callback");

        let events = sink.lock().expect("sink");
        let alerts = events
            .iter()
            .filter(|event| event.kind == "session.failed")
            .collect::<Vec<_>>();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].payload["authority_scope"], "current-session");
        assert_eq!(alerts[0].payload["command_id"], "command-current");
        assert_eq!(alerts[0].payload["turn_id"], "turn-current");
        let rendered = crate::render::Renderer::render(
            &crate::render::DefaultRenderer,
            alerts[0],
            &crate::events::MessageFormat::Alert,
        )
        .expect("render");
        assert!(rendered.contains("current turn failed"), "{rendered}");
        assert!(rendered.contains("command=command-current"), "{rendered}");
        assert!(rendered.contains("turn=turn-current"), "{rendered}");
        assert!(
            store
                .record(&lane_id)
                .expect("record")
                .pending_alerts
                .is_empty()
        );
    }

    #[tokio::test]
    async fn newer_failed_replacement_suppresses_old_and_alerts_only_current_turn() {
        let (_dir, store, lane_id) = store_with_lane("failed-replacement");
        let record = store.record(&lane_id).expect("record");
        let mut old = observation(GjcTurnState::Failed, GjcSessionDisposition::Live);
        old.revision = 10;
        old.turn_id = Some("turn-old".to_string());
        old.command_id = Some("command-old".to_string());
        let (record, _) = store
            .apply_observation(&lane_id, record.revision, &old, &ts(1_200))
            .expect("old failure");
        assert_eq!(record.pending_alerts.len(), 1);

        let mut current = old;
        current.revision = 11;
        current.turn_id = Some("turn-current-failed".to_string());
        current.command_id = Some("command-current-failed".to_string());
        let plane = Arc::new(ScriptedPlane {
            responses: Mutex::new(VecDeque::from(vec![Ok(current.clone()), Ok(current)])),
        });
        let (tx, sink, mut rx) = event_channel(8);
        let callback_sink = sink.clone();
        let callback = tokio::spawn(async move {
            if let Some(event) = rx.recv().await {
                callback_sink.lock().expect("sink").push(event.clone());
                crate::dispatch::resolve_alert_acceptance_id(
                    event.payload["event_id"].as_str().expect("event id"),
                    true,
                );
            }
        });
        let reconciler =
            GjcReconciler::new(plane, None, store.clone(), tx, GjcPollingPolicy::default());
        reconciler.poll_once(fixed_now(40_000)).await.expect("poll");
        callback.await.expect("callback");

        let events = sink.lock().expect("sink");
        let alerts = events
            .iter()
            .filter(|event| event.kind == "session.failed")
            .collect::<Vec<_>>();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].payload["turn_id"], "turn-current-failed");
        assert_eq!(alerts[0].payload["command_id"], "command-current-failed");
        assert!(
            store
                .record(&lane_id)
                .expect("record")
                .pending_alerts
                .is_empty()
        );
    }

    #[test]
    fn pending_alert_delivery_ownership_is_partial_and_restart_safe() {
        let (dir, store, lane_id) = store_with_lane("delivery-ownership");
        let record = store.record(&lane_id).expect("record");
        let (failed, _) = store
            .apply_observation(
                &lane_id,
                record.revision,
                &observation(GjcTurnState::Failed, GjcSessionDisposition::Failed),
                &ts(1_200),
            )
            .expect("failed observation");
        let event_id = failed.pending_alerts[0].event_id.clone();

        assert!(
            store
                .claim_pending_alert_delivery(&lane_id, &event_id, "discord:channel:a")
                .expect("claim a")
        );
        assert!(
            store
                .record_pending_alert_delivery(&lane_id, &event_id, "discord:channel:a", true)
                .expect("deliver a")
        );
        assert!(
            store
                .claim_pending_alert_delivery(&lane_id, &event_id, "discord:channel:b")
                .expect("claim b")
        );
        assert!(
            store
                .record_pending_alert_delivery(&lane_id, &event_id, "discord:channel:b", false)
                .expect("fail b")
        );

        let partial = store.record(&lane_id).expect("partial record");
        let alert = &partial.pending_alerts[0];
        assert_eq!(
            alert.deliveries["discord:channel:a"].state,
            GjcPendingAlertDeliveryState::Delivered
        );
        assert_eq!(
            alert.deliveries["discord:channel:b"].state,
            GjcPendingAlertDeliveryState::Failed
        );

        drop(store);
        let reopened =
            Arc::new(GjcLaneStore::open(&dir.path().join("gjc-lane-state.json")).expect("reopen"));
        assert_eq!(
            reopened.pending_alert_delivery_state(&lane_id, &event_id, "discord:channel:a"),
            Some(GjcPendingAlertDeliveryState::Delivered)
        );
        assert_eq!(
            reopened.pending_alert_delivery_state(&lane_id, &event_id, "discord:channel:b"),
            Some(GjcPendingAlertDeliveryState::Failed)
        );
        assert!(
            reopened
                .claim_pending_alert_delivery(&lane_id, &event_id, "discord:channel:b")
                .expect("retry b")
        );
        assert_eq!(
            reopened.pending_alert_delivery_state(&lane_id, &event_id, "discord:channel:b"),
            Some(GjcPendingAlertDeliveryState::Claimed)
        );
        assert!(
            reopened
                .record_pending_alert_delivery(&lane_id, &event_id, "discord:channel:b", true)
                .expect("deliver b")
        );
        assert_eq!(
            reopened.pending_alert_delivery_state(&lane_id, &event_id, "discord:channel:a"),
            Some(GjcPendingAlertDeliveryState::Delivered)
        );
        assert_eq!(
            reopened.pending_alert_delivery_state(&lane_id, &event_id, "discord:channel:b"),
            Some(GjcPendingAlertDeliveryState::Delivered)
        );
    }

    #[test]
    fn clean_success_and_initial_idle_remain_silent() {
        let (_dir, store, lane_id) = store_with_lane("silent-lane");
        let record = store.record(&lane_id).expect("record");
        let (idle, _) = store
            .apply_observation(
                &lane_id,
                record.revision,
                &observation(GjcTurnState::Idle, GjcSessionDisposition::Live),
                &ts(1_100),
            )
            .expect("idle observation");
        assert!(idle.pending_alerts.is_empty());
        let (complete, _) = store
            .apply_observation(
                &lane_id,
                idle.revision,
                &observation(GjcTurnState::Complete, GjcSessionDisposition::Complete),
                &ts(1_200),
            )
            .expect("complete observation");
        assert!(complete.pending_alerts.is_empty());
    }

    #[test]
    fn endpoint_alerts_dedupe_until_recovery_then_start_new_episode() {
        let (_dir, store, lane_id) = store_with_lane("endpoint-alert");
        let record = store.record(&lane_id).expect("record");
        let (first, _) = store
            .mark_unavailable(&lane_id, record.revision, "timeout", u32::MAX, &ts(1_100))
            .expect("first failure");
        assert_eq!(first.pending_alerts.len(), 1);
        let first_id = first.pending_alerts[0].event_id.clone();
        let (second, _) = store
            .mark_unavailable(
                &lane_id,
                first.revision,
                "connection closed",
                u32::MAX,
                &ts(1_200),
            )
            .expect("repeat failure");
        assert_eq!(second.pending_alerts.len(), 1);
        assert_eq!(second.pending_alerts[0].event_id, first_id);

        let (recovered, _) = store
            .apply_observation(
                &lane_id,
                second.revision,
                &observation(GjcTurnState::Idle, GjcSessionDisposition::Live),
                &ts(1_300),
            )
            .expect("recovery");
        let (new_episode, _) = store
            .mark_unavailable(
                &lane_id,
                recovered.revision,
                "timeout",
                u32::MAX,
                &ts(1_400),
            )
            .expect("new failure");
        assert_eq!(new_episode.pending_alerts.len(), 2);
        assert_ne!(new_episode.pending_alerts[1].event_id, first_id);
    }

    #[tokio::test]
    async fn restarted_reconciler_replays_staged_failed_alert_once() {
        let (dir, store, lane_id) = store_with_lane("failed-replay");
        let record = store.record(&lane_id).expect("record");
        let (failed, _) = store
            .apply_observation(
                &lane_id,
                record.revision,
                &observation(GjcTurnState::Failed, GjcSessionDisposition::Failed),
                &ts(1_200),
            )
            .expect("failed observation");
        let event_id = failed.pending_alerts[0].event_id.clone();
        drop(store);
        let reopened =
            Arc::new(GjcLaneStore::open(&dir.path().join("gjc-lane-state.json")).expect("reopen"));
        let plane = Arc::new(ScriptedPlane {
            responses: Mutex::new(VecDeque::from(vec![Ok(observation(
                GjcTurnState::Failed,
                GjcSessionDisposition::Failed,
            ))])),
        });
        let (tx, sink, mut rx) = event_channel(8);
        let callback_sink = sink.clone();
        let callback = tokio::spawn(async move {
            if let Some(event) = rx.recv().await {
                callback_sink.lock().expect("sink").push(event.clone());
                crate::dispatch::resolve_alert_acceptance_id(
                    event.payload["event_id"].as_str().expect("event id"),
                    true,
                );
            }
        });
        let reconciler = GjcReconciler::new(
            plane,
            None,
            reopened.clone(),
            tx,
            GjcPollingPolicy::default(),
        );
        reconciler.poll_once(fixed_now(10_000)).await.expect("poll");
        callback.await.expect("callback");
        let events = sink.lock().expect("sink");
        let alerts = events
            .iter()
            .filter(|event| event.kind == "session.failed")
            .collect::<Vec<_>>();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].payload["event_id"], event_id);
        assert!(
            reopened
                .record(&lane_id)
                .expect("record")
                .pending_alerts
                .is_empty()
        );
    }

    #[test]
    fn endpoint_generation_bump_invalidates_stale_review_owner_evidence() {
        let (_dir, store, lane_id) = store_with_lane("sess-1");
        let record = store.record(&lane_id).expect("record");
        assert!(record.review_evidence_valid);
        let mut advanced = observation(GjcTurnState::Idle, GjcSessionDisposition::Live);
        advanced.endpoint_generation = 4;
        let (_, outcome) = store
            .apply_observation(&lane_id, record.revision, &advanced, &ts(1_200))
            .expect("apply");
        assert!(outcome.evidence_invalidated);
        let updated = store.record(&lane_id).expect("record");
        assert_eq!(updated.endpoint_generation, 4);
        assert!(!updated.review_evidence_valid);
        assert!(!updated.owner_evidence_valid);
        assert_eq!(updated.evidence_revision, 2);

        // Opaque fingerprints are not numerically ordered; a lower value is
        // still a distinct replacement generation and must be accepted.
        let mut regressed = advanced.clone();
        regressed.endpoint_generation = 2;
        let (_, outcome) = store
            .apply_observation(&lane_id, updated.revision, &regressed, &ts(1_300))
            .expect("replacement generation accepted");
        assert!(!outcome.evidence_invalidated);
    }

    #[test]
    fn gate_revisions_ignore_stale_and_conflicting_equal_observations() {
        let (_dir, store, lane_id) = store_with_lane("gate-ordering");
        let mut opened = observation(GjcTurnState::Idle, GjcSessionDisposition::Live);
        opened.gate_state = Some(GjcGateState::Ready);
        opened.gate_id = Some("gate-1".into());
        opened.gate_revision = 5;
        let record = store.record(&lane_id).expect("record");
        let (record, _) = store
            .apply_observation(&lane_id, record.revision, &opened, &ts(1_200))
            .expect("open gate");

        let mut stale = opened.clone();
        stale.gate_state = Some(GjcGateState::Closed);
        stale.gate_revision = 4;
        let (record, _) = store
            .apply_observation(&lane_id, record.revision, &stale, &ts(1_300))
            .expect("ignore stale gate");
        assert_eq!(record.last_gate.as_ref().expect("gate").revision, 5);
        assert_eq!(
            record.last_gate.as_ref().expect("gate").state,
            GjcGateState::Ready
        );

        let mut conflicting = opened.clone();
        conflicting.gate_state = Some(GjcGateState::Closed);
        assert!(
            store
                .apply_observation(&lane_id, record.revision, &conflicting, &ts(1_400))
                .is_err()
        );

        let mut newer = opened;
        newer.gate_state = Some(GjcGateState::Closed);
        newer.gate_revision = 6;
        let record = store
            .apply_observation(&lane_id, record.revision, &newer, &ts(1_500))
            .expect("new gate revision")
            .0;
        assert_eq!(record.last_gate.as_ref().expect("gate").revision, 6);
        assert_eq!(
            record.last_gate.as_ref().expect("gate").state,
            GjcGateState::Closed
        );
    }

    #[test]
    fn malformed_gate_identity_is_rejected_before_durable_mutation() {
        let (_dir, store, lane_id) = store_with_lane("sess-1");
        let before = store.record(&lane_id).expect("record");

        let mut malformed_id = observation(GjcTurnState::Idle, GjcSessionDisposition::Live);
        malformed_id.gate_state = Some(GjcGateState::Ready);
        malformed_id.gate_id = Some("gate invalid".to_string());
        malformed_id.gate_revision = 1;
        let error = store
            .apply_observation(&lane_id, before.revision, &malformed_id, &ts(1_200))
            .expect_err("malformed gate id must fail closed");
        assert!(error.to_string().contains("canonical identity"));

        let mut malformed_revision = malformed_id;
        malformed_revision.gate_id = Some("gate-1".to_string());
        malformed_revision.gate_revision = 0;
        let error = store
            .apply_observation(&lane_id, before.revision, &malformed_revision, &ts(1_201))
            .expect_err("zero gate revision must fail closed");
        assert!(error.to_string().contains("canonical identity"));
        assert_eq!(store.record(&lane_id).expect("record"), before);
    }

    #[test]
    fn gate_episode_deduplication_survives_restart_and_reopens_on_higher_revision() {
        let (dir, store, lane_id) = store_with_lane("sess-1");
        let mut opened = observation(GjcTurnState::Idle, GjcSessionDisposition::Live);
        opened.gate_state = Some(GjcGateState::Ready);
        opened.gate_id = Some("gate-1".to_string());
        opened.gate_revision = 7;
        let record = store.record(&lane_id).expect("record");
        let (record, _) = store
            .apply_observation(&lane_id, record.revision, &opened, &ts(1_200))
            .expect("open gate");
        assert_eq!(record.pending_alerts.len(), 1);
        let first_event = record.pending_alerts[0].event_id.clone();

        // Remove the staged event as a successful delivery would, leaving the
        // durable gate episode watermark behind for restart reconciliation.
        let record = store
            .acknowledge_pending_alert(&lane_id, record.revision, &first_event, &ts(1_201))
            .expect("acknowledge first gate alert");
        drop(store);
        let reopened =
            Arc::new(GjcLaneStore::open(&dir.path().join("gjc-lane-state.json")).expect("reopen"));

        // Replaying the same gate at a newer session revision remains quiet.
        let mut replay = opened.clone();
        replay.revision += 1;
        let (record, _) = reopened
            .apply_observation(&lane_id, record.revision, &replay, &ts(1_202))
            .expect("replay gate");
        assert!(record.pending_alerts.is_empty());

        // Reopening the same id requires a strictly higher gate revision.
        let mut higher = replay;
        higher.revision += 1;
        higher.gate_revision = 8;
        let record = reopened
            .apply_observation(&lane_id, record.revision, &higher, &ts(1_203))
            .expect("higher gate revision")
            .0;
        assert_eq!(record.pending_alerts.len(), 1);
        assert_ne!(record.pending_alerts[0].event_id, first_event);
    }

    #[test]
    fn pr_head_and_base_movement_invalidates_stale_evidence_exactly_once() {
        let (_dir, store, _lane_id) = store_with_lane("pr-sess");
        let binding = GjcPrBinding {
            repo: "Yeachan-Heo/clawhip".to_string(),
            number: 325,
            head_sha: "a".repeat(40),
            base_branch: "dev".to_string(),
            bound_at: ts(900),
        };
        // Attach a binding via direct registration replacement: register a new
        // lane carrying the PR binding.
        let request = GjcLaneRegistrationRequest {
            sdk_session_id: "pr-live".to_string(),
            worktree: None,
            endpoint_generation: None,
            owner_id: None,
            pr: Some(GjcPrBindingInput {
                repo: binding.repo.clone(),
                number: binding.number,
                head_sha: binding.head_sha.clone(),
                base_branch: binding.base_branch.clone(),
            }),
        };
        let pr_lane = store.register_lane(&request, &ts(1_000)).expect("register");

        // Same head/base: no-op.
        let state_same = GjcPrState {
            number: 325,
            head_sha: binding.head_sha.clone(),
            base_branch: "dev".to_string(),
        };
        let (_, outcome) = store
            .reconcile_pr_binding(
                &pr_lane.lane_id,
                pr_lane.revision,
                Some(&state_same),
                &ts(1_100),
            )
            .expect("reconcile");
        assert!(!outcome.evidence_invalidated);

        // Pushed head: evidence invalidated once.
        let pushed = GjcPrState {
            number: 325,
            head_sha: "b".repeat(40),
            base_branch: "dev".to_string(),
        };
        let (_, outcome) = store
            .reconcile_pr_binding(
                &pr_lane.lane_id,
                pr_lane.revision + 1,
                Some(&pushed),
                &ts(1_200),
            )
            .expect("reconcile");
        assert!(outcome.head_changed);
        assert!(outcome.evidence_invalidated);
        let updated = store.record(&pr_lane.lane_id).expect("record");
        assert!(!updated.review_evidence_valid);

        // Base retarget also invalidates.
        let retargeted = GjcPrState {
            number: 325,
            head_sha: pushed.head_sha.clone(),
            base_branch: "main".to_string(),
        };
        let (_, outcome) = store
            .reconcile_pr_binding(
                &pr_lane.lane_id,
                updated.revision,
                Some(&retargeted),
                &ts(1_300),
            )
            .expect("reconcile");
        assert!(outcome.base_changed);
        assert!(
            !outcome.evidence_invalidated,
            "evidence was already invalidated by the head change: exactly-once semantics"
        );

        // PR disappears after invalidation already happened: no new bump.
        let (_, outcome) = store
            .reconcile_pr_binding(
                &pr_lane.lane_id,
                store.record(&pr_lane.lane_id).expect("record").revision,
                None,
                &ts(1_400),
            )
            .expect("reconcile");
        assert!(outcome.unresolved && !outcome.evidence_invalidated);
        let (_, outcome) = store
            .reconcile_pr_binding(
                &pr_lane.lane_id,
                store.record(&pr_lane.lane_id).expect("record").revision,
                None,
                &ts(1_500),
            )
            .expect("reconcile");
        assert!(outcome.unresolved && !outcome.evidence_invalidated);
        assert!(
            store
                .audit()
                .iter()
                .any(|entry| entry.kind == GjcAuditKind::PrHeadChanged)
        );
        assert!(
            store
                .audit()
                .iter()
                .any(|entry| entry.kind == GjcAuditKind::PrBaseChanged)
        );
    }

    #[test]
    fn ghost_watch_removal_requires_terminal_and_preserves_audit() {
        let (dir, store, lane_id) = store_with_lane("sess-1");
        let record = store.record(&lane_id).expect("record");

        // Non-terminal lanes never qualify.
        let premature = store.remove_ghost_watch(&lane_id, record.revision, "early", &ts(1_100));
        assert!(premature.unwrap_err().to_string().contains("terminal"));

        let (terminal, _) = store
            .apply_observation(
                &lane_id,
                record.revision,
                &observation(GjcTurnState::Complete, GjcSessionDisposition::Complete),
                &ts(1_200),
            )
            .expect("apply");
        let removed = store
            .remove_ghost_watch(
                &lane_id,
                terminal.revision,
                "sdk session absent",
                &ts(1_300),
            )
            .expect("remove");
        assert!(removed.watch_removed_at.is_some());
        let double = store.remove_ghost_watch(&lane_id, removed.revision, "again", &ts(1_400));
        assert!(double.unwrap_err().to_string().contains("already"));

        // Tombstone + audit evidence survive persistence.
        let path = dir.path().join("gjc-lane-state.json");
        drop(store);
        let reopened = GjcLaneStore::open(&path).expect("reopen");
        let tombstone = reopened.record(&lane_id).expect("tombstone retained");
        assert!(tombstone.watch_removed_at.is_some());
        assert_eq!(
            tombstone.terminal_disposition.expect("disposition").kind,
            GjcTerminalKind::Complete
        );
        assert!(
            reopened
                .audit()
                .iter()
                .any(|entry| entry.kind == GjcAuditKind::GhostWatchRemoved)
        );
        let reclaimed = reopened
            .reclaim_tombstone(
                &lane_id,
                tombstone.revision,
                tombstone.endpoint_generation,
                tombstone.worktree.clone(),
                &ts(1_500),
            )
            .expect("reclaim tombstone");
        assert!(reclaimed.watch_removed_at.is_none());
        assert!(reclaimed.terminal_disposition.is_none());
    }

    #[test]
    fn corrupt_state_file_fails_open_as_ignored_invalid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("gjc-lane-state.json");
        std::fs::write(&path, b"{not json").expect("write junk");
        let store = GjcLaneStore::open(&path).expect("open falls back");
        match store.status() {
            GjcLaneStoreStatus::IgnoredInvalid { .. } => {}
            other => panic!("expected ignored-invalid, got {other:?}"),
        }
        assert!(
            store
                .register_lane(&registration("after-junk"), &ts(1_000))
                .is_err()
        );
    }

    #[test]
    fn registration_validation_rejects_bad_input_without_mutating() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = GjcLaneStore::open(&dir.path().join("state.json")).expect("open");
        for bad in [
            GjcLaneRegistrationRequest {
                sdk_session_id: String::new(),
                worktree: None,
                endpoint_generation: None,
                owner_id: None,
                pr: None,
            },
            GjcLaneRegistrationRequest {
                sdk_session_id: "bad id with spaces".to_string(),
                worktree: None,
                endpoint_generation: None,
                owner_id: None,
                pr: None,
            },
            GjcLaneRegistrationRequest {
                sdk_session_id: "ok".to_string(),
                worktree: Some("relative/../worktree".to_string()),
                endpoint_generation: None,
                owner_id: None,
                pr: None,
            },
            GjcLaneRegistrationRequest {
                sdk_session_id: "ok".to_string(),
                worktree: None,
                endpoint_generation: None,
                owner_id: None,
                pr: Some(GjcPrBindingInput {
                    repo: "no-slash".to_string(),
                    number: 1,
                    head_sha: "a".repeat(40),
                    base_branch: "dev".to_string(),
                }),
            },
            GjcLaneRegistrationRequest {
                sdk_session_id: "ok".to_string(),
                worktree: None,
                endpoint_generation: None,
                owner_id: None,
                pr: Some(GjcPrBindingInput {
                    repo: "Yeachan-Heo/clawhip".to_string(),
                    number: 0,
                    head_sha: "a".repeat(40),
                    base_branch: "dev".to_string(),
                }),
            },
            GjcLaneRegistrationRequest {
                sdk_session_id: "ok".to_string(),
                worktree: None,
                endpoint_generation: None,
                owner_id: None,
                pr: Some(GjcPrBindingInput {
                    repo: "Yeachan-Heo/clawhip".to_string(),
                    number: 1,
                    head_sha: "a".repeat(40),
                    base_branch: "dev\nbranch".to_string(),
                }),
            },
        ] {
            let error = store.register_lane(&bad, &ts(1_000)).expect_err("reject");
            assert!(!error.to_string().contains("poisoned"));
        }
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn duplicate_registration_conflicts() {
        let (_dir, store, _) = store_with_lane("dup-sess");
        let error = store
            .register_lane(&registration("dup-sess"), &ts(1_100))
            .unwrap_err();
        assert!(error.to_string().contains("registration conflict"));
    }

    #[test]
    fn audit_trail_is_bounded_with_drop_accounting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(GjcLaneStore::open(&dir.path().join("state.json")).expect("open"));
        let request = GjcLaneRegistrationRequest {
            sdk_session_id: "churn".to_string(),
            worktree: None,
            endpoint_generation: None,
            owner_id: None,
            pr: None,
        };
        let record = store.register_lane(&request, &ts(1_000)).expect("register");
        let mut revision = record.revision;
        for index in 0..(MAX_AUDIT_ENTRIES + 50) {
            let next = store
                .claim_ownership(
                    &record.lane_id,
                    revision,
                    &format!("owner-{index}"),
                    &ts(1_100 + index as i64),
                )
                .expect("churn");
            revision = next.revision;
        }
        assert!(store.audit().len() <= MAX_AUDIT_ENTRIES);
        assert!(store.audit_entries_dropped() > 0);
    }

    #[test]
    fn runtime_gone_classifies_retires_and_releases_ownership() {
        let (_dir, store, lane_id) = store_with_lane("gone-sess");
        let _ = store.claim_ownership(&lane_id, 1, "owner", &ts(1_100));
        let record = store
            .mark_runtime_gone(&lane_id, 2, "session not found", &ts(1_200))
            .expect("mark");
        assert_eq!(record.phase, Some(GjcLanePhase::RuntimeGone));
        assert_eq!(
            record.terminal_disposition.as_ref().expect("retired").kind,
            GjcTerminalKind::Retired
        );
        assert_eq!(record.ownership.state, GjcOwnershipState::Relinquished);
        assert!(!record.review_evidence_valid);
        assert!(!record.owner_evidence_valid);
        assert!(classify_lane(&record, None) == GjcLanePhase::Retired);
    }

    struct ScriptedPlane {
        responses: Mutex<VecDeque<std::result::Result<GjcSdkObservation, GjcSdkQueryError>>>,
    }

    #[async_trait]
    impl GjcSdkControlPlane for ScriptedPlane {
        async fn query_lane(
            &self,
            _query: &GjcLaneQuery,
        ) -> std::result::Result<GjcSdkObservation, GjcSdkQueryError> {
            self.responses
                .lock()
                .expect("script lock")
                .pop_front()
                .unwrap_or_else(|| Err(GjcSdkQueryError::Ambiguous("script exhausted".to_string())))
        }
    }

    struct StaticPrResolver {
        states: Mutex<VecDeque<Option<GjcPrState>>>,
    }

    #[async_trait]
    impl GjcLanePrResolver for StaticPrResolver {
        async fn resolve_pr(&self, _repo: &str, _number: u64) -> Result<Option<GjcPrState>> {
            Ok(self
                .states
                .lock()
                .expect("resolver lock")
                .pop_front()
                .flatten())
        }
    }

    fn event_channel(
        capacity: usize,
    ) -> (
        mpsc::Sender<IncomingEvent>,
        Arc<Mutex<Vec<IncomingEvent>>>,
        mpsc::Receiver<IncomingEvent>,
    ) {
        // Single-threaded test runtime: return the receiver so assertions can
        // pump queued events deterministically instead of relying on a
        // background task being polled.
        let (tx, rx) = mpsc::channel::<IncomingEvent>(capacity);
        let sink: Arc<Mutex<Vec<IncomingEvent>>> = Arc::new(Mutex::new(Vec::new()));
        (tx, sink, rx)
    }

    fn pump_events(rx: &mut mpsc::Receiver<IncomingEvent>, sink: &Arc<Mutex<Vec<IncomingEvent>>>) {
        while let Ok(event) = rx.try_recv() {
            sink.lock().expect("sink").push(event);
        }
    }

    fn fixed_now(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(secs).expect("fixed now")
    }

    #[tokio::test]
    async fn automatic_enrollment_discovers_native_session_before_polling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let worktree = dir.path().join("worktree");
        let sdk_dir = worktree.join(".gjc/state/sdk");
        std::fs::create_dir_all(&sdk_dir).expect("sdk state");
        let session = "01a02ccd-c754-7656-95c7-f40b5a140bc3";
        std::fs::write(
            sdk_dir.join(format!("{session}.json")),
            format!(
                r#"{{"version":1,"sessionId":"{session}","url":"ws://127.0.0.1:1/","token":"test","pid":{}}}"#,
                std::process::id()
            ),
        )
        .expect("metadata");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                sdk_dir.join(format!("{session}.json")),
                std::fs::Permissions::from_mode(0o600),
            )
            .expect("metadata permissions");
        }
        assert_eq!(
            crate::gjc_sdk::discover_all(&crate::gjc_sdk::StateRoot::for_worktree(&worktree))
                .expect("discover metadata")
                .len(),
            1
        );
        let store =
            Arc::new(GjcLaneStore::open(&dir.path().join("lane-state.json")).expect("open store"));
        let (tx, _sink, _rx) = event_channel(8);
        let mut live = observation(GjcTurnState::Running, GjcSessionDisposition::Live);
        live.session_id = session.to_string();
        live.worktree = Some(worktree.to_string_lossy().into_owned());
        live.revision = 1;
        let reconciler = GjcReconciler::new(
            Arc::new(ScriptedPlane {
                responses: Mutex::new(VecDeque::from([Ok(live)])),
            }),
            None,
            store.clone(),
            tx,
            GjcPollingPolicy::default(),
        )
        .with_auto_enrollment(vec![worktree]);
        reconciler.auto_enroll_native_sessions("2026-08-30T00:00:00Z");
        assert!(store.record_for_session(session).is_some());
        let outcome = reconciler.poll_once(fixed_now(1_000)).await.expect("poll");
        assert_eq!(outcome.examined, 1);
        assert_eq!(store.record_for_session(session).unwrap().sdk_revision, 1);
    }

    #[tokio::test]
    async fn reconciler_applies_live_evidence_then_removes_terminal_ghost_watch() {
        let (_dir, store, lane_id) = store_with_lane("sess-1");
        let mut complete_observation =
            observation(GjcTurnState::Complete, GjcSessionDisposition::Complete);
        complete_observation.revision = 11;
        let plane = Arc::new(ScriptedPlane {
            responses: Mutex::new(VecDeque::from(vec![
                Ok(observation(
                    GjcTurnState::Running,
                    GjcSessionDisposition::Live,
                )),
                Ok(complete_observation),
                Err(GjcSdkQueryError::SessionNotFound(
                    "session reaped".to_string(),
                )),
            ])),
        });
        let (tx, sink, mut rx) = event_channel(64);
        let reconciler =
            GjcReconciler::new(plane, None, store.clone(), tx, GjcPollingPolicy::default());

        let outcome = reconciler
            .poll_once(fixed_now(10_000))
            .await
            .expect("pass 1");
        assert_eq!(outcome.examined, 1);
        assert_eq!(outcome.terminal_set, 0);
        let record = store.record(&lane_id).expect("record");
        assert_eq!(record.phase, Some(GjcLanePhase::Active));

        let outcome = reconciler
            .poll_once(fixed_now(10_060))
            .await
            .expect("pass 2");
        assert_eq!(outcome.terminal_set, 1);
        let terminal = store.record(&lane_id).expect("record");
        assert_eq!(
            terminal.terminal_disposition.expect("terminal").kind,
            GjcTerminalKind::Complete
        );

        let outcome = reconciler
            .poll_once(fixed_now(10_120))
            .await
            .expect("pass 3");
        assert_eq!(outcome.ghost_watches_removed, 1);
        let tombstone = store.record(&lane_id).expect("record");
        assert!(tombstone.watch_removed_at.is_some());

        pump_events(&mut rx, &sink);
        let events = sink.lock().expect("sink");
        assert!(
            events
                .iter()
                .any(|event| event.kind == GJC_LANE_STATUS_EVENT)
        );
        let statuses: Vec<&Value> = events
            .iter()
            .filter(|event| event.kind == GJC_LANE_STATUS_EVENT)
            .map(|event| &event.payload)
            .collect();
        assert!(
            statuses
                .iter()
                .any(|payload| payload["watch_removed"] == Value::Bool(true))
        );
    }

    #[tokio::test]
    async fn reconciler_marks_runtime_gone_on_session_not_found() {
        let (_dir, store, lane_id) = store_with_lane("gone-recon");
        let plane = Arc::new(ScriptedPlane {
            responses: Mutex::new(VecDeque::from(vec![Err(
                GjcSdkQueryError::SessionNotFound("no such session".to_string()),
            )])),
        });
        let (tx, _sink, _rx) = event_channel(8);
        let reconciler =
            GjcReconciler::new(plane, None, store.clone(), tx, GjcPollingPolicy::default());
        let outcome = reconciler.poll_once(fixed_now(20_000)).await.expect("pass");
        assert_eq!(outcome.runtime_gone, 1);
        assert_eq!(outcome.terminal_set, 1);
        let record = store.record(&lane_id).expect("record");
        assert_eq!(record.phase, Some(GjcLanePhase::RuntimeGone));
    }

    #[tokio::test]
    async fn reconciler_bounds_unavailable_polling_and_suspends() {
        let (_dir, store, lane_id) = store_with_lane("unavail");
        let plane = Arc::new(UnavailablePlane::default());
        let (tx, _sink, _rx) = event_channel(8);
        let policy = GjcPollingPolicy {
            poll_interval_secs: 30,
            initial_backoff_ms: 1,
            max_backoff_ms: 2,
            max_consecutive_attempts: 3,
            ghost_grace_polls: 2,
        };
        let reconciler = GjcReconciler::new(plane, None, store.clone(), tx, policy);

        for tick in 0..3_u64 {
            let outcome = reconciler
                .poll_once(fixed_now(30_000 + (tick * 60) as i64))
                .await
                .expect("pass");
            assert_eq!(outcome.examined, 1);
        }
        let record = store.record(&lane_id).expect("record");
        assert_eq!(record.phase, Some(GjcLanePhase::Unavailable));
        assert_eq!(record.consecutive_unavailable_polls, 3);
        assert!(record.polling_suspended);

        // Suspended lanes are skipped entirely (bounded polling).
        let outcome = reconciler.poll_once(fixed_now(31_000)).await.expect("pass");
        assert_eq!(outcome.examined, 0);

        // Backoff deferral: fresh lane with recent last_query_at is deferred.
        let (_dir2, store2, lane2) = store_with_lane("deferred");
        let plane2 = Arc::new(UnavailablePlane::default());
        let (tx2, _sink2, _rx2) = event_channel(8);
        let policy2 = GjcPollingPolicy {
            initial_backoff_ms: 60_000,
            max_backoff_ms: 60_000,
            ..GjcPollingPolicy::default()
        };
        let reconciler2 = GjcReconciler::new(plane2, None, store2.clone(), tx2, policy2);
        reconciler2
            .poll_once(fixed_now(40_000))
            .await
            .expect("first");
        let outcome = reconciler2
            .poll_once(fixed_now(40_010))
            .await
            .expect("second");
        assert_eq!(outcome.examined, 0, "within backoff window the poll defers");
        assert_eq!(outcome.retained, 1);
        assert!(
            store2
                .record(&lane2)
                .expect("record")
                .last_query_at
                .is_some()
        );
    }

    #[derive(Default)]
    struct UnavailablePlane {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl GjcSdkControlPlane for UnavailablePlane {
        async fn query_lane(
            &self,
            _query: &GjcLaneQuery,
        ) -> std::result::Result<GjcSdkObservation, GjcSdkQueryError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(GjcSdkQueryError::EndpointUnavailable(
                "connection refused".to_string(),
            ))
        }
    }

    #[tokio::test]
    async fn reconciler_invalidates_stale_evidence_when_pr_head_moves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(GjcLaneStore::open(&dir.path().join("state.json")).expect("open"));
        let request = GjcLaneRegistrationRequest {
            sdk_session_id: "pr-recon".to_string(),
            worktree: None,
            endpoint_generation: None,
            owner_id: None,
            pr: Some(GjcPrBindingInput {
                repo: "Yeachan-Heo/clawhip".to_string(),
                number: 325,
                head_sha: "a".repeat(40),
                base_branch: "dev".to_string(),
            }),
        };
        let record = store.register_lane(&request, &ts(1_000)).expect("register");
        let plane = Arc::new(ScriptedPlane {
            responses: Mutex::new(VecDeque::from(vec![Ok(observation(
                GjcTurnState::Idle,
                GjcSessionDisposition::Live,
            ))])),
        });
        let resolver = Arc::new(StaticPrResolver {
            states: Mutex::new(VecDeque::from(vec![Some(GjcPrState {
                number: 325,
                head_sha: "f".repeat(40),
                base_branch: "dev".to_string(),
            })])),
        });
        let (tx, _sink, _rx) = event_channel(8);
        let reconciler = GjcReconciler::new(
            plane,
            Some(resolver),
            store.clone(),
            tx,
            GjcPollingPolicy::default(),
        );
        let outcome = reconciler.poll_once(fixed_now(50_000)).await.expect("pass");
        assert_eq!(outcome.evidence_invalidations, 1);
        let updated = store.record(&record.lane_id).expect("record");
        assert!(!updated.review_evidence_valid);
        assert_eq!(updated.pr.expect("binding").head_sha, "f".repeat(40));
    }

    #[tokio::test]
    async fn reconciler_survives_cas_race_without_lost_updates() {
        let (dir, store, lane_id) = store_with_lane("race-sess");
        let plane = Arc::new(ScriptedPlane {
            responses: Mutex::new(VecDeque::from(vec![Ok(observation(
                GjcTurnState::Idle,
                GjcSessionDisposition::Live,
            ))])),
        });
        let (tx, _sink, _rx) = event_channel(8);
        let reconciler = Arc::new(GjcReconciler::new(
            plane,
            None,
            store.clone(),
            tx,
            GjcPollingPolicy::default(),
        ));
        let snapshot_revision = store.record(&lane_id).expect("record").revision;
        let racer = store.clone();
        let racer_lane = lane_id.clone();
        let racing = tokio::task::spawn_blocking(move || {
            racer.claim_ownership(&racer_lane, snapshot_revision, "racer", &ts(60_000))
        });
        let reconcile = reconciler.poll_once(fixed_now(60_000));
        let (racing_result, reconcile_result) = tokio::join!(racing, reconcile);
        let raced_ok = racing_result.expect("join").is_ok();
        let reconciled = reconcile_result.expect("reconcile outcome");
        // Exactly one of the two CAS writers wins; the loser leaves an audit skip.
        let total = if raced_ok {
            1 + reconciled.skipped_conflicts
        } else {
            reconciled.examined
        };
        assert!(total >= 1);
        assert!(
            store.record(&lane_id).expect("final").revision > snapshot_revision,
            "one or both concurrent CAS writers must commit"
        );
        // Durable file stays valid JSON after the race.
        let content =
            std::fs::read_to_string(dir.path().join("gjc-lane-state.json")).expect("read state");
        let parsed: Value = serde_json::from_str(&content).expect("valid json after race");
        assert_eq!(parsed["schema"], GJC_LANE_STATE_SCHEMA);
    }

    #[test]
    fn migrated_lane_id_remains_a_lookup_and_mutation_alias() {
        let (dir, store, canonical_id) = store_with_lane("legacy-alias");
        let legacy_id = "legacy-lane-id".to_string();
        {
            let mut state = store.state.lock().expect("state");
            let mut record = state.lanes.remove(&canonical_id).expect("record");
            record.lane_id = legacy_id.clone();
            state.lanes.insert(legacy_id.clone(), record);
            store.persist_locked(&mut state).expect("persist");
        }

        let reopened = GjcLaneStore::open(&dir.path().join("gjc-lane-state.json")).expect("reopen");
        let record = reopened.record(&legacy_id).expect("legacy lookup");
        assert_eq!(record.lane_id, canonical_id);
        let updated = reopened
            .mutate(&legacy_id, record.revision, &ts(80_000), |mut record| {
                record.phase = Some(GjcLanePhase::Unavailable);
                Ok(multi_audit(record, vec![]))
            })
            .expect("legacy mutation");
        assert_eq!(updated.phase, Some(GjcLanePhase::Unavailable));
    }

    #[test]
    fn health_json_reports_store_and_phase_diagnostics() {
        let (_dir, store, lane_id) = store_with_lane("sess-1");
        let _ = store.apply_observation(
            &lane_id,
            1,
            &observation(GjcTurnState::Running, GjcSessionDisposition::Live),
            &ts(70_000),
        );
        let health = health_json(&store, true);
        assert_eq!(health["schema"], GJC_LANE_HEALTH_SCHEMA);
        assert_eq!(health["store"]["active_watches"], 1);
        assert_eq!(health["phases"]["active"], 1);
        assert_eq!(health["control_plane_registered"], true);
    }

    #[test]
    fn polling_policy_validation_rejects_zero_intervals() {
        assert!(
            GjcPollingPolicy {
                poll_interval_secs: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            GjcPollingPolicy {
                initial_backoff_ms: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            GjcPollingPolicy {
                max_backoff_ms: 10,
                initial_backoff_ms: 20,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(GjcPollingPolicy::default().validate().is_ok());
    }
}
