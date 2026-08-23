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

use crate::events::IncomingEvent;

pub const GJC_LANE_STATE_SCHEMA: &str = "clawhip.gjc-lane-state.v1";
pub const GJC_LANE_STATUS_EVENT: &str = "gjc.lane.status";
pub const GJC_LANE_STATUS_SCHEMA: &str = "clawhip.gjc-lane-status.v1";
pub const GJC_LANE_HEALTH_SCHEMA: &str = "clawhip.gjc-health.v1";

/// Audit trail is bounded: oldest entries are dropped once this many exist.
pub const MAX_AUDIT_ENTRIES: usize = 512;
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
    pub endpoint_generation: u64,
    pub revision: u64,
    pub turn_state: GjcTurnState,
    pub turn_id: Option<String>,
    pub gate_state: Option<GjcGateState>,
    pub gate_id: Option<String>,
    pub disposition: GjcSessionDisposition,
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
    pub observed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcGateSnapshot {
    pub state: GjcGateState,
    pub gate_id: Option<String>,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcReconcileAuditEntry {
    pub at: String,
    pub lane_id: String,
    pub kind: GjcAuditKind,
    pub detail: String,
}

/// Durable record for one GJC SDK-backed lane watch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GjcLaneRecord {
    pub lane_id: String,
    pub sdk_session_id: String,
    pub worktree: Option<String>,
    /// Last observed SDK endpoint generation; a bump invalidates stale
    /// review/owner evidence captured under the previous generation.
    pub endpoint_generation: u64,
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
    format!("gjc-{:016x}", Sha256::digest(sdk_session_id.as_bytes())[0])
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
    !value.is_empty()
        && value.len() <= MAX_SESSION_LEN
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn valid_worktree(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_WORKTREE_LEN && !value.chars().any(char::is_control)
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

impl GjcLaneStore {
    /// Open (or initialize) the durable lane-state file. Invalid content fails
    /// open as `IgnoredInvalid` rather than blocking the daemon, mirroring the
    /// tmux watch registry contract.
    pub fn open(path: &Path) -> Result<Self> {
        let (state, status) = match std::fs::read(path) {
            Ok(content) => match serde_json::from_slice::<GjcLaneStateFile>(&content) {
                Ok(parsed) if parsed.schema == GJC_LANE_STATE_SCHEMA => {
                    let lanes = parsed.lanes.len();
                    (parsed, GjcLaneStoreStatus::Loaded { lanes })
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
        Ok(Self {
            path: path.to_path_buf(),
            state: Mutex::new(state),
            status,
        })
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
        self.state.lock().ok()?.lanes.get(lane_id).cloned()
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
            endpoint_generation: request.endpoint_generation.unwrap_or(0),
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
            if observation.endpoint_generation < record.endpoint_generation {
                // Never regress generation; treat as ambiguous upstream state.
                bail!("lane revision conflict: endpoint generation regressed");
            }
            let previous_generation = record.endpoint_generation;
            if observation.endpoint_generation > previous_generation
                && (record.review_evidence_valid || record.owner_evidence_valid)
            {
                record.review_evidence_valid = false;
                record.owner_evidence_valid = false;
                record.evidence_revision += 1;
                outcome.evidence_invalidated = true;
                record.endpoint_generation = observation.endpoint_generation;
                return Ok(multi_audit(
                    record,
                    vec![(
                        GjcAuditKind::EvidenceInvalidated,
                        format!(
                            "endpoint generation {previous_generation} -> {}; stale review/owner evidence invalidated",
                            observation.endpoint_generation
                        ),
                    )],
                ));
            }
            record.sdk_revision = observation.revision;
            record.endpoint_generation = record.endpoint_generation.max(observation.endpoint_generation);
            record.consecutive_unavailable_polls = 0;
            record.last_query_at = Some(now.to_string());
            let resumed = record.polling_suspended;
            record.polling_suspended = false;

            record.last_turn = Some(GjcTurnSnapshot {
                state: observation.turn_state,
                turn_id: observation.turn_id.clone(),
                observed_at: now.to_string(),
            });
            record.last_gate = observation.gate_state.map(|gate_state| GjcGateSnapshot {
                state: gate_state,
                gate_id: observation.gate_id.clone(),
                observed_at: now.to_string(),
            });

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

            record.phase = Some(classify_lane(&record, Some(observation)));
            if record.phase != previous_phase {
                outcome.phase_changed = record.phase;
            }

            let mut audit = Vec::new();
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
                    record.consecutive_unavailable_polls,
                    bounded_text(reason)
                ),
            ));
            Ok(multi_audit(record, audit))
        })?;
        Ok((record, newly_suspended))
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
        let record = state
            .lanes
            .get(lane_id)
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
                lane_id: lane_id.to_string(),
                kind,
                detail,
            });
        }
        state.lanes.insert(lane_id.to_string(), updated.clone());
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
        std::fs::rename(&tmp_path, &self.path)
            .with_context(|| format!("failed to persist {}", self.path.display()))?;
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
                || observation.turn_state == GjcTurnState::AwaitingInput
            {
                GjcLanePhase::Blocked
            } else if observation.turn_state == GjcTurnState::Running {
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
            pr: GjcLanesPrConfig::default(),
        }
    }
}

impl GjcLanesConfig {
    pub fn is_empty(&self) -> bool {
        !self.enabled && self.state_path.is_none() && self.pr.is_empty()
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
        }
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
        let mut outcome = GjcReconcileOutcome::default();
        let records = self.store.snapshot_watches(false);
        for snapshot in records {
            if snapshot.polling_suspended {
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
            match self.plane.query_lane(&query).await {
                Ok(observation) => match self.store.apply_observation(
                    &snapshot.lane_id,
                    expected,
                    &observation,
                    &now_text,
                ) {
                    Ok((record, applied)) => {
                        outcome.retained += 1;
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
                            return Err(error);
                        }
                    }
                },
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
                    match self.store.apply_observation(
                        &snapshot.lane_id,
                        expected,
                        &GjcSdkObservation {
                            session_id: snapshot.sdk_session_id.clone(),
                            worktree: snapshot.worktree.clone(),
                            endpoint_generation: observed,
                            revision: snapshot.sdk_revision,
                            turn_state: snapshot
                                .last_turn
                                .as_ref()
                                .map(|turn| turn.state)
                                .unwrap_or(GjcTurnState::Idle),
                            turn_id: None,
                            gate_state: snapshot.last_gate.as_ref().map(|gate| gate.state),
                            gate_id: None,
                            disposition: GjcSessionDisposition::Live,
                        },
                        &now_text,
                    ) {
                        Ok((_, applied)) => {
                            if applied.evidence_invalidated {
                                outcome.evidence_invalidations += 1;
                            }
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
                        if misses >= self.policy.ghost_grace_polls {
                            match self.store.remove_ghost_watch(
                                &snapshot.lane_id,
                                expected,
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
                                            expected,
                                            &now_text,
                                        );
                                    } else {
                                        return Err(error);
                                    }
                                }
                            }
                        } else {
                            match self.store.mark_unavailable(
                                &snapshot.lane_id,
                                expected,
                                &detail,
                                u32::MAX,
                                &now_text,
                            ) {
                                Ok(_) => outcome.retained += 1,
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
                    } else {
                        match self.store.mark_unavailable(
                            &snapshot.lane_id,
                            expected,
                            &detail,
                            self.policy.max_consecutive_attempts,
                            &now_text,
                        ) {
                            Ok((_, suspended)) => {
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
                Err(GjcSdkQueryError::Ambiguous(detail)) => {
                    // Fail closed: no classification change without
                    // authoritative evidence.
                    self.store
                        .note_revision_conflict(&snapshot.lane_id, expected, &now_text);
                    let _ = detail;
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
        if pr.base_branch.is_empty() || pr.base_branch.len() > 128 {
            bail!("invalid PR base branch");
        }
    }
    Ok(())
}

/// Build the health/status diagnostic snapshot for the daemon API.
pub fn health_json(store: &SharedGjcLaneStore, plane_registered: bool) -> Value {
    let records = store.snapshot();
    let mut phases: BTreeMap<String, usize> = BTreeMap::new();
    for record in &records {
        if record.watch_removed_at.is_some() {
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
        .filter(|r| r.watch_removed_at.is_none())
        .count();
    let removed = records.len() - active;
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
            endpoint_generation: 3,
            revision: 10,
            turn_state: turn,
            turn_id: Some("turn-1".to_string()),
            gate_state: None,
            gate_id: None,
            disposition,
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

        // Generation regression is rejected as conflicting upstream state.
        let mut regressed = advanced.clone();
        regressed.endpoint_generation = 2;
        let error = store
            .apply_observation(&lane_id, updated.revision, &regressed, &ts(1_300))
            .expect_err("regression rejected");
        assert!(error.to_string().contains("regressed"));
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
        // And remains usable: registrations proceed on the recovered file.
        store
            .register_lane(&registration("after-junk"), &ts(1_000))
            .expect("usable");
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
    async fn reconciler_applies_live_evidence_then_removes_terminal_ghost_watch() {
        let (_dir, store, lane_id) = store_with_lane("sess-1");
        let plane = Arc::new(ScriptedPlane {
            responses: Mutex::new(VecDeque::from(vec![
                Ok(observation(
                    GjcTurnState::Running,
                    GjcSessionDisposition::Live,
                )),
                Ok(observation(
                    GjcTurnState::Complete,
                    GjcSessionDisposition::Complete,
                )),
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
        assert_eq!(
            store.record(&lane_id).expect("final").revision,
            snapshot_revision + 1
        );
        // Durable file stays valid JSON after the race.
        let content =
            std::fs::read_to_string(dir.path().join("gjc-lane-state.json")).expect("read state");
        let parsed: Value = serde_json::from_str(&content).expect("valid json after race");
        assert_eq!(parsed["schema"], GJC_LANE_STATE_SCHEMA);
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
