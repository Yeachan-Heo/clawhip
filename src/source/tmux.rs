use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::process::Command;
use tokio::sync::{RwLock, mpsc};
use tokio::time::sleep;

use crate::Result;
use crate::client::DaemonClient;
use crate::config::{AppConfig, TmuxSessionMonitor};
use crate::events::{IncomingEvent, MessageFormat, RoutingMetadata};
use crate::keyword_window::{
    KeywordHit, KeywordMatchProvenance, KeywordMatchSource, PendingKeywordHits,
    collect_keyword_hits_with_provenance,
};
use crate::router::glob_match;
use crate::source::Source;
use crate::telemetry;

pub type SharedTmuxRegistry = Arc<RwLock<HashMap<String, RegisteredTmuxSession>>>;
static REGISTRY_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static NEXT_REGISTRATION_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Mint a process-wide monotonically increasing registration generation.
/// Client-supplied values are always overwritten with the daemon-minted
/// value, so a stale cleanup observation cannot match a re-registered
/// entry under the same session name.
fn mint_registration_generation() -> u64 {
    NEXT_REGISTRATION_GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Advance the allocator past any restored values so a freshly reloaded
/// registration is never assigned a generation that collides with a stale
/// prune candidate from before the restart.
fn advance_registration_generation_above(loaded: u64) {
    let mut current = NEXT_REGISTRATION_GENERATION.load(Ordering::Relaxed);
    loop {
        if loaded <= current {
            break;
        }
        match NEXT_REGISTRATION_GENERATION.compare_exchange(
            current,
            loaded + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TmuxRegistryDiagnostics {
    pub registered_count: usize,
    pub durable_runtime_count: usize,
    pub config_projected_count: usize,
    pub live_probe: TmuxLiveProbeDiagnostics,
    pub registry_state: TmuxRegistryStateDiagnostics,
}

#[derive(Debug, Clone, Serialize)]
pub struct TmuxLiveProbeDiagnostics {
    pub ok: bool,
    pub count: usize,
    pub sample: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TmuxRegistryStateDiagnostics {
    pub path: PathBuf,
    pub status: TmuxRegistryStateStatus,
    pub loaded: usize,
    pub ignored: usize,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TmuxRegistryStateStatus {
    Missing,
    Loaded,
    IgnoredInvalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RegistrationSource {
    CliWatch,
    CliNew,
    #[default]
    ConfigMonitor,
}

impl RegistrationSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CliWatch => "cli-watch",
            Self::CliNew => "cli-new",
            Self::ConfigMonitor => "config-monitor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentProcessInfo {
    pub pid: u32,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredTmuxSession {
    pub session: String,
    pub channel: Option<String>,
    pub mention: Option<String>,
    #[serde(default)]
    pub routing: RoutingMetadata,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default = "default_keyword_window_secs")]
    pub keyword_window_secs: u64,
    pub stale_minutes: u64,
    pub format: Option<MessageFormat>,
    #[serde(default = "current_timestamp_rfc3339")]
    pub registered_at: String,
    #[serde(default)]
    pub registration_source: RegistrationSource,
    #[serde(default)]
    pub parent_process: Option<ParentProcessInfo>,
    #[serde(default)]
    pub registration_generation: u64,
    #[serde(default)]
    pub active_wrapper_monitor: bool,
    #[serde(skip)]
    pub(crate) lane: Option<LaneEvidence>,
}

/// Private, durable lane evidence. It is deliberately excluded from legacy
/// registration serialization and `/api/tmux` projections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneEvidence {
    pub lane_version: u8,
    pub generation_id: String,
    pub kickoff_operation_id: String,
    pub launch_operation_id: String,
    pub executor_id: String,
    pub worker_effect_kind: WorkerEffectKind,
    pub launch_state: LaneLaunchState,
    #[serde(default)]
    pub workflow: LaneWorkflow,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub quiesced: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kickoff_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kickoff_delivered_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<LaneVisibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<LaneVerification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<BoundedCategory>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_update_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_update_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_update_delivered_at: Option<String>,
    #[serde(default)]
    pub delivery_retry_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_disposition: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkerEffectKind {
    CommandSubmission,
    SessionCreation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaneLaunchState {
    Ready,
    Claimed,
    IdentityVerified,
    Launched,
    NoWorkerEffect,
    CommandSubmitAmbiguous,
    SessionCreationAmbiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LaneWorkflow {
    #[default]
    Active,
    NeedsReview,
    NeedsQa,
    PrOpen,
    AwaitingCi,
    AwaitingHuman,
    Retired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaneVisibility {
    Unverified,
    Visible,
    Unreachable,
    DeliveryFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneVerification {
    pub checked_at: String,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<BoundedReason>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BoundedReason(String);
impl BoundedReason {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into().chars().take(96).collect())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BoundedCategory(String);
impl BoundedCategory {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into().chars().take(96).collect())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// On-disk migration wrapper. Old maps deserialize through `deserialize_stored_registry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTmuxRegistration {
    pub registration: RegisteredTmuxSession,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<LaneEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneSnapshot {
    pub session: String,
    pub generation_id: String,
    pub kickoff_operation_id: String,
    pub launch_operation_id: String,
    pub executor_id: String,
    pub worker_effect_kind: WorkerEffectKind,
    pub durable_launch_state: LaneLaunchState,
    pub derived_status: Option<String>,
    pub observation_reason: Option<BoundedReason>,
    pub worker_started: Option<bool>,
    pub evidence_commit: bool,
    pub repair_needed: bool,
    pub healthy: bool,
    pub exit_category: Option<BoundedCategory>,
    pub workflow: LaneWorkflow,
    pub visibility: Option<LaneVisibility>,
    pub revision: u64,
    #[serde(default)]
    pub kickoff_message_id: Option<String>,
    #[serde(default)]
    pub kickoff_delivered_at: Option<String>,
    #[serde(default)]
    pub latest_update_message_id: Option<String>,
    #[serde(default)]
    pub latest_update_kind: Option<String>,
    #[serde(default)]
    pub latest_update_delivered_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneDetail {
    pub snapshot: LaneSnapshot,
    pub thread_id: Option<String>,
    pub kickoff_message_id: Option<String>,
    pub kickoff_delivered_at: Option<String>,
    pub verification: Option<LaneVerification>,
    pub latest_update_message_id: Option<String>,
    pub latest_update_kind: Option<String>,
    pub latest_update_delivered_at: Option<String>,
    pub last_error: Option<BoundedCategory>,
    pub quiesced: bool,
}

fn lane_detail(session: &str, lane: &LaneEvidence, runtime_live: Option<bool>) -> LaneDetail {
    LaneDetail {
        snapshot: lane_snapshot(session, lane, runtime_live),
        thread_id: lane.thread_id.clone(),
        kickoff_message_id: lane.kickoff_message_id.clone(),
        kickoff_delivered_at: lane.kickoff_delivered_at.clone(),
        verification: lane.verification.clone(),
        latest_update_message_id: lane.latest_update_message_id.clone(),
        latest_update_kind: lane.latest_update_kind.clone(),
        latest_update_delivered_at: lane.latest_update_delivered_at.clone(),
        last_error: lane.last_failure.clone(),
        quiesced: lane.quiesced,
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum StoredRegistryValue {
    Wrapped(Box<StoredTmuxRegistration>),
    Legacy(Box<RegisteredTmuxSession>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneEvidenceMutation {
    pub session: String,
    pub expected_revision: u64,
    pub generation_id: String,
    pub launch_operation_id: String,
    pub launch_state: LaneLaunchState,
    #[serde(default)]
    pub failure_category: Option<BoundedCategory>,
    pub executor_id: String,
    pub worker_effect_kind: WorkerEffectKind,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneVerificationMutation {
    pub session: String,
    pub expected_revision: u64,
    pub checked_at: String,
    pub outcome: String,
    #[serde(default)]
    pub reason: Option<BoundedReason>,
    pub visibility: LaneVisibility,
    pub generation_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneDeliveryMutation {
    pub session: String,
    pub expected_revision: u64,
    #[serde(default)]
    pub workflow: Option<LaneWorkflow>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub delivered_at: Option<String>,
    pub visibility: LaneVisibility,
    #[serde(default)]
    pub error_category: Option<BoundedCategory>,
    #[serde(default)]
    pub disposition: Option<String>,
    pub generation_id: String,
    #[serde(default)]
    pub initial_kickoff: bool,
    #[serde(default)]
    pub kickoff_operation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneRegistrationInput {
    pub registration: RegisteredTmuxSession,
    pub generation_id: String,
    pub kickoff_operation_id: String,
    pub launch_operation_id: String,
    pub executor_id: String,
    pub worker_effect_kind: WorkerEffectKind,
    #[serde(default)]
    pub thread_id: Option<String>,
    pub expect_absent_or_retired: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneWorkflowMutation {
    pub session: String,
    pub generation_id: String,
    pub expected_revision: u64,
    pub workflow: LaneWorkflow,
    #[serde(default)]
    pub quiesced: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneRetirementMutation {
    pub session: String,
    pub generation_id: String,
    pub expected_revision: u64,
}

fn lane_of(registration: &RegisteredTmuxSession) -> Option<&LaneEvidence> {
    registration.lane.as_ref()
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}
fn valid_discord_id(value: &str) -> bool {
    (1..=20).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit())
}
fn valid_session(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}
fn valid_text(value: &str) -> bool {
    !value.is_empty() && value.len() <= 64 && value.is_ascii()
}
fn valid_delivery(input: &LaneDeliveryMutation) -> bool {
    let error_ok = matches!(
        (
            input.disposition.as_deref(),
            input.error_category.as_ref().map(BoundedCategory::as_str),
        ),
        (Some("accepted"), None)
            | (
                Some("definitive-failure"),
                Some(
                    "payload-too-large"
                        | "bad-request"
                        | "unauthorized"
                        | "forbidden"
                        | "not-found"
                        | "rate-limited"
                        | "archived-or-not-writable"
                        | "transport",
                ),
            )
            | (
                Some("ambiguous-acceptance"),
                Some(
                    "timeout"
                        | "transport"
                        | "malformed-success"
                        | "empty-message-id"
                        | "missing-message-id",
                ),
            )
    );
    let kind_ok = input.kind.as_deref().is_none_or(|value| {
        matches!(
            value,
            "kickoff" | "progress" | "blocked" | "pr-open" | "handoff"
        )
    });
    let disposition_ok = match input.disposition.as_deref() {
        Some("accepted") => {
            input.message_id.is_some()
                && input.delivered_at.is_some()
                && input.visibility == LaneVisibility::Visible
                && input.error_category.is_none()
        }
        Some("definitive-failure") => {
            input.message_id.is_none()
                && input.delivered_at.is_none()
                && input.visibility == LaneVisibility::DeliveryFailed
                && input.error_category.is_some()
        }
        Some("ambiguous-acceptance") => {
            input.message_id.is_none()
                && input.delivered_at.is_none()
                && input.visibility == LaneVisibility::Unverified
                && input.error_category.is_some()
        }
        _ => false,
    };
    input.message_id.as_deref().is_none_or(valid_discord_id)
        && input.delivered_at.as_deref().is_none_or(valid_text)
        && kind_ok
        && error_ok
        && disposition_ok
}
fn valid_verification(input: &LaneVerificationMutation) -> bool {
    let discord_error = |reason: &str| {
        matches!(
            reason,
            "payload-too-large"
                | "bad-request"
                | "unauthorized"
                | "forbidden"
                | "not-found"
                | "rate-limited"
                | "archived-or-not-writable"
                | "timeout"
                | "transport"
                | "malformed-success"
                | "empty-message-id"
                | "missing-message-id"
        )
    };
    let reason = input.reason.as_ref().map(BoundedReason::as_str);
    let reason_ok = reason.is_none_or(|reason| {
        discord_error(reason)
            || matches!(
                reason,
                "thread-target-missing"
                    | "kickoff-missing"
                    | "kickoff-receipt-missing"
                    | "no-message-observed"
                    | "unreachable"
            )
    });
    valid_text(&input.checked_at)
        && reason_ok
        && match input.outcome.as_str() {
            "kickoff-visible" => reason.is_none(),
            "kickoff-missing" => reason == Some("kickoff-missing"),
            "message-observed-unverified" => reason == Some("kickoff-receipt-missing"),
            "unverified" => reason.is_some_and(|reason| {
                reason == "thread-target-missing"
                    || reason == "no-message-observed"
                    || discord_error(reason)
            }),
            "unreachable" => {
                reason.is_some_and(|reason| reason == "unreachable" || discord_error(reason))
            }
            _ => false,
        }
}
fn terminal_category_is_valid(
    kind: WorkerEffectKind,
    state: LaneLaunchState,
    category: Option<&BoundedCategory>,
) -> bool {
    matches!(
        (kind, state, category.map(BoundedCategory::as_str)),
        (
            _,
            LaneLaunchState::Ready
                | LaneLaunchState::Claimed
                | LaneLaunchState::IdentityVerified
                | LaneLaunchState::Launched,
            None,
        ) | (
            WorkerEffectKind::CommandSubmission,
            LaneLaunchState::NoWorkerEffect,
            Some("session-create-failed"),
        ) | (
            WorkerEffectKind::CommandSubmission,
            LaneLaunchState::CommandSubmitAmbiguous,
            Some(
                "submission-attempt-error"
                    | "submitted-marker-set-failed"
                    | "submitted-marker-read-failed"
                    | "submitted-marker-missing"
                    | "submitted-marker-mismatch",
            ),
        ) | (
            _,
            LaneLaunchState::SessionCreationAmbiguous,
            Some(
                "identity-marker-set-failed"
                    | "identity-marker-read-failed"
                    | "identity-marker-mismatch"
                    | "r1-to-r2-persistence-failed-after-create"
                    | "owner-aborted-before-r2",
            ),
        )
    )
}

fn lane_snapshot(session: &str, lane: &LaneEvidence, runtime_live: Option<bool>) -> LaneSnapshot {
    let (
        derived_status,
        observation_reason,
        worker_started,
        evidence_commit,
        repair_needed,
        healthy,
        exit_category,
    ) = match lane.launch_state {
        LaneLaunchState::Launched => {
            let healthy = lane.workflow == LaneWorkflow::Active
                && runtime_live == Some(true)
                && lane.visibility == Some(LaneVisibility::Visible);
            let status = match (runtime_live, lane.workflow) {
                (Some(false), LaneWorkflow::Active) => Some("tmux-missing".into()),
                (
                    Some(false),
                    LaneWorkflow::NeedsReview
                    | LaneWorkflow::NeedsQa
                    | LaneWorkflow::PrOpen
                    | LaneWorkflow::AwaitingCi
                    | LaneWorkflow::AwaitingHuman,
                ) => Some("workflow-handoff".into()),
                _ => None,
            };
            (status, None, Some(true), true, false, healthy, None)
        }
        LaneLaunchState::CommandSubmitAmbiguous => {
            let category = lane
                .last_failure
                .clone()
                .unwrap_or_else(|| BoundedCategory::new("submission-attempt-error"));
            (
                Some("command-submit-ambiguous-blocked".into()),
                Some(BoundedReason::new(category.as_str())),
                None,
                true,
                false,
                false,
                Some(category),
            )
        }
        LaneLaunchState::SessionCreationAmbiguous => {
            let category = lane
                .last_failure
                .clone()
                .unwrap_or_else(|| BoundedCategory::new("owner-aborted-before-r2"));
            (
                Some("session-creation-ambiguous-blocked".into()),
                Some(BoundedReason::new(category.as_str())),
                None,
                true,
                false,
                false,
                Some(category),
            )
        }
        LaneLaunchState::IdentityVerified => {
            let reason = lane
                .last_failure
                .clone()
                .unwrap_or_else(|| BoundedCategory::new("session-liveness-unavailable"));
            let status = if reason.as_str() == "identity-mismatch" {
                "identity-conflict"
            } else if lane.worker_effect_kind == WorkerEffectKind::CommandSubmission {
                "launch-commit-ambiguous"
            } else {
                "session-creation-effect-uncommitted"
            };
            (
                Some(status.into()),
                Some(BoundedReason::new(reason.as_str())),
                None,
                false,
                true,
                false,
                Some(reason),
            )
        }
        LaneLaunchState::NoWorkerEffect => {
            let category = lane
                .last_failure
                .clone()
                .unwrap_or_else(|| BoundedCategory::new("launch-failed-no-worker-effect"));
            (
                Some("launch-failed-no-worker-effect".into()),
                Some(BoundedReason::new(category.as_str())),
                None,
                true,
                false,
                false,
                Some(category),
            )
        }
        LaneLaunchState::Ready => {
            if let Some(category) = lane.last_failure.clone() {
                let status = match lane.delivery_disposition.as_deref() {
                    Some("ambiguous-acceptance") => "kickoff-ambiguous",
                    Some("definitive-failure") => "kickoff-failed",
                    _ => "pending",
                };
                (
                    Some(status.into()),
                    Some(BoundedReason::new(category.as_str())),
                    None,
                    true,
                    true,
                    false,
                    Some(category),
                )
            } else {
                (Some("pending".into()), None, None, false, true, false, None)
            }
        }
        LaneLaunchState::Claimed => {
            let status = if lane.worker_effect_kind == WorkerEffectKind::CommandSubmission {
                "command-execution-claim-stranded"
            } else {
                "session-creation-effect-uncommitted"
            };
            (Some(status.into()), None, None, false, true, false, None)
        }
    };
    LaneSnapshot {
        session: session.into(),
        generation_id: lane.generation_id.clone(),
        kickoff_operation_id: lane.kickoff_operation_id.clone(),
        launch_operation_id: lane.launch_operation_id.clone(),
        executor_id: lane.executor_id.clone(),
        worker_effect_kind: lane.worker_effect_kind,
        durable_launch_state: lane.launch_state,
        derived_status,
        observation_reason,
        worker_started,
        evidence_commit,
        repair_needed,
        healthy,
        exit_category,
        workflow: lane.workflow,
        visibility: lane.visibility,
        revision: lane.revision,
        kickoff_message_id: lane.kickoff_message_id.clone(),
        kickoff_delivered_at: lane.kickoff_delivered_at.clone(),
        latest_update_message_id: lane.latest_update_message_id.clone(),
        latest_update_kind: lane.latest_update_kind.clone(),
        latest_update_delivered_at: lane.latest_update_delivered_at.clone(),
    }
}

impl From<&TmuxSessionMonitor> for RegisteredTmuxSession {
    fn from(value: &TmuxSessionMonitor) -> Self {
        Self {
            session: value.session.clone(),
            channel: value.channel.clone(),
            mention: value.mention.clone(),
            routing: RoutingMetadata::default(),
            keywords: value.keywords.clone(),
            keyword_window_secs: value.keyword_window_secs,
            stale_minutes: value.stale_minutes,
            format: value.format.clone(),
            registered_at: current_timestamp_rfc3339(),
            registration_source: RegistrationSource::ConfigMonitor,
            parent_process: None,
            registration_generation: 0,
            active_wrapper_monitor: false,
            lane: None,
        }
    }
}

pub struct TmuxSource {
    config: Arc<AppConfig>,
    registry: SharedTmuxRegistry,
    registry_state_path: PathBuf,
}

impl TmuxSource {
    pub fn new(
        config: Arc<AppConfig>,
        registry: SharedTmuxRegistry,
        registry_state_path: PathBuf,
    ) -> Self {
        Self {
            config,
            registry,
            registry_state_path,
        }
    }
}

#[async_trait::async_trait]
impl Source for TmuxSource {
    fn name(&self) -> &str {
        "tmux"
    }

    async fn run(&self, tx: mpsc::Sender<IncomingEvent>) -> Result<()> {
        let mut state = TmuxMonitorState::default();

        loop {
            if self.config.monitors.tmux.sessions.is_empty()
                && self.registry.read().await.is_empty()
            {
                sleep(Duration::from_secs(
                    self.config.monitors.poll_interval_secs.max(1),
                ))
                .await;
                continue;
            }
            poll_tmux(
                self.config.as_ref(),
                &self.registry,
                &self.registry_state_path,
                &tx,
                &mut state,
            )
            .await?;
            sleep(Duration::from_secs(
                self.config.monitors.poll_interval_secs.max(1),
            ))
            .await;
        }
    }
}

#[async_trait::async_trait]
trait EventEmitter: Send + Sync {
    async fn emit(&self, event: IncomingEvent) -> Result<()>;
}

#[async_trait::async_trait]
impl EventEmitter for mpsc::Sender<IncomingEvent> {
    async fn emit(&self, event: IncomingEvent) -> Result<()> {
        self.send(event)
            .await
            .map_err(|error| format!("tmux source channel closed: {error}").into())
    }
}

#[async_trait::async_trait]
impl EventEmitter for DaemonClient {
    async fn emit(&self, event: IncomingEvent) -> Result<()> {
        self.send_event(&event).await
    }
}

struct TmuxPaneState {
    session: String,
    pane_name: String,
    snapshot: String,
    content_hash: u64,
    last_change: Instant,
    last_stale_notification: Option<Instant>,
    pane_dead: bool,
}

#[derive(Default)]
struct TmuxMonitorState {
    panes: HashMap<String, TmuxPaneState>,
    pending_keyword_hits: HashMap<String, PendingKeywordHits>,
}

struct TmuxPaneSnapshot {
    pane_id: String,
    session: String,
    pane_name: String,
    content: String,
    pane_dead: bool,
}

pub async fn monitor_registered_session(
    registration: RegisteredTmuxSession,
    client: DaemonClient,
) -> Result<()> {
    let mut panes = HashMap::new();
    let mut pending_keyword_hits = None;
    let poll_interval = Duration::from_secs(1);

    loop {
        let now = Instant::now();
        if !session_exists(&registration.session).await? {
            telemetry::emit(source_record(
                telemetry::event_name::SOURCE_INVENTORY,
                "source_missing",
                Some(&registration.session),
                None,
            ));
            break;
        }

        flush_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration,
            &client,
            &registration.session,
            now,
            Duration::from_secs(registration.keyword_window_secs.max(1)),
            false,
        )
        .await?;

        let panes_snapshot = snapshot_tmux_session(&registration.session).await?;
        let mut active_panes = HashSet::new();

        for pane in panes_snapshot {
            active_panes.insert(pane.pane_id.clone());
            let pane_key = pane.pane_id.clone();
            let hash = content_hash(&pane.content);
            let latest_line = last_nonempty_line(&pane.content);

            if pane.pane_dead {
                pending_keyword_hits = None;
            }

            match panes.get_mut(&pane_key) {
                None => {
                    panes.insert(
                        pane_key,
                        TmuxPaneState {
                            session: pane.session,
                            pane_name: pane.pane_name,
                            content_hash: hash,
                            snapshot: pane.content,
                            last_change: now,
                            last_stale_notification: None,
                            pane_dead: pane.pane_dead,
                        },
                    );
                }
                Some(existing) => {
                    existing.pane_dead = pane.pane_dead;
                    if existing.content_hash != hash {
                        let hits = if pane.pane_dead {
                            Vec::new()
                        } else {
                            collect_keyword_hits_with_provenance(
                                &existing.snapshot,
                                &pane.content,
                                &registration.keywords,
                                KeywordMatchProvenance {
                                    pane_id: pane.pane_id.clone(),
                                    pane_name: pane.pane_name.clone(),
                                    cursor: None,
                                    source: KeywordMatchSource::FreshOutput,
                                },
                            )
                        };
                        push_pending_keyword_hits(&mut pending_keyword_hits, now, hits);

                        existing.session = pane.session;
                        existing.pane_name = pane.pane_name;
                        existing.content_hash = hash;
                        existing.snapshot = pane.content;
                        existing.last_change = now;
                        existing.last_stale_notification = None;
                    } else if should_emit_stale(existing, now, registration.stale_minutes) {
                        client
                            .emit(tmux_stale_event(
                                &registration,
                                existing.session.clone(),
                                existing.pane_name.clone(),
                                latest_line,
                            ))
                            .await?;
                        existing.last_stale_notification = Some(now);
                    }
                }
            }
        }

        panes.retain(|pane_id, _| active_panes.contains(pane_id));
        sleep(poll_interval).await;
    }

    Ok(())
}

pub fn default_registry_state_path(cron_state_path: &Path) -> PathBuf {
    cron_state_path.with_file_name("tmux-watch-registry.json")
}

pub fn normalize_runtime_registration_source(source: RegistrationSource) -> RegistrationSource {
    match source {
        RegistrationSource::ConfigMonitor => RegistrationSource::CliWatch,
        RegistrationSource::CliWatch | RegistrationSource::CliNew => source,
    }
}

fn durable_runtime_entries(
    registry: &HashMap<String, RegisteredTmuxSession>,
) -> BTreeMap<String, StoredTmuxRegistration> {
    registry
        .iter()
        .filter(|(_, registration)| {
            registration.registration_source != RegistrationSource::ConfigMonitor
        })
        .map(|(session, registration)| {
            (
                session.clone(),
                StoredTmuxRegistration {
                    registration: registration.clone(),
                    lane: registration.lane.clone(),
                },
            )
        })
        .collect()
}

async fn save_durable_tmux_registry(
    path: &Path,
    registry: &HashMap<String, RegisteredTmuxSession>,
) -> Result<usize> {
    let durable = durable_runtime_entries(registry);
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let content = serde_json::to_vec_pretty(&durable)?;
    let tmp_path = path.with_extension("json.tmp");
    tokio::fs::write(&tmp_path, content).await?;
    tokio::fs::rename(&tmp_path, path).await?;
    Ok(durable.len())
}

pub async fn load_tmux_registry_state(
    path: &Path,
    registry: &SharedTmuxRegistry,
) -> TmuxRegistryStateDiagnostics {
    match tokio::fs::read(path).await {
        Ok(content) => {
            match serde_json::from_slice::<BTreeMap<String, StoredRegistryValue>>(&content) {
                Ok(loaded) => {
                    let mut write = registry.write().await;
                    let mut max_loaded_generation = 0u64;
                    for (session, stored) in loaded {
                        let mut registration = match stored {
                            StoredRegistryValue::Wrapped(stored) => {
                                let mut registration = stored.registration;
                                registration.lane = stored.lane;
                                registration
                            }
                            StoredRegistryValue::Legacy(registration) => *registration,
                        };
                        registration.registration_source =
                            normalize_runtime_registration_source(registration.registration_source);
                        // Legacy or pre-generation entries (generation 0) are
                        // re-minted so a stale cleanup candidate from before
                        // the restart cannot match them.
                        if registration.registration_generation == 0 {
                            registration.registration_generation = mint_registration_generation();
                        }
                        max_loaded_generation =
                            max_loaded_generation.max(registration.registration_generation);
                        write.insert(session, registration);
                    }
                    advance_registration_generation_above(max_loaded_generation);
                    TmuxRegistryStateDiagnostics {
                        path: path.to_path_buf(),
                        status: TmuxRegistryStateStatus::Loaded,
                        loaded: durable_runtime_entries(&write).len(),
                        ignored: 0,
                        last_error: None,
                    }
                }
                Err(error) => TmuxRegistryStateDiagnostics {
                    path: path.to_path_buf(),
                    status: TmuxRegistryStateStatus::IgnoredInvalid,
                    loaded: 0,
                    ignored: 1,
                    last_error: Some(error.to_string()),
                },
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            TmuxRegistryStateDiagnostics {
                path: path.to_path_buf(),
                status: TmuxRegistryStateStatus::Missing,
                loaded: 0,
                ignored: 0,
                last_error: None,
            }
        }
        Err(error) => TmuxRegistryStateDiagnostics {
            path: path.to_path_buf(),
            status: TmuxRegistryStateStatus::IgnoredInvalid,
            loaded: 0,
            ignored: 1,
            last_error: Some(error.to_string()),
        },
    }
}

pub async fn inspect_tmux_registry_state(path: &Path) -> TmuxRegistryStateDiagnostics {
    match tokio::fs::read(path).await {
        Ok(content) => {
            match serde_json::from_slice::<BTreeMap<String, StoredRegistryValue>>(&content) {
                Ok(loaded) => TmuxRegistryStateDiagnostics {
                    path: path.to_path_buf(),
                    status: TmuxRegistryStateStatus::Loaded,
                    loaded: loaded.len(),
                    ignored: 0,
                    last_error: None,
                },
                Err(error) => TmuxRegistryStateDiagnostics {
                    path: path.to_path_buf(),
                    status: TmuxRegistryStateStatus::IgnoredInvalid,
                    loaded: 0,
                    ignored: 1,
                    last_error: Some(error.to_string()),
                },
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            TmuxRegistryStateDiagnostics {
                path: path.to_path_buf(),
                status: TmuxRegistryStateStatus::Missing,
                loaded: 0,
                ignored: 0,
                last_error: None,
            }
        }
        Err(error) => TmuxRegistryStateDiagnostics {
            path: path.to_path_buf(),
            status: TmuxRegistryStateStatus::IgnoredInvalid,
            loaded: 0,
            ignored: 1,
            last_error: Some(error.to_string()),
        },
    }
}

pub async fn register_runtime_tmux_registration(
    registry: &SharedTmuxRegistry,
    path: &Path,
    mut registration: RegisteredTmuxSession,
) -> Result<usize> {
    registration.registration_source =
        normalize_runtime_registration_source(registration.registration_source);
    registration.registration_generation = mint_registration_generation();
    let _mutation = REGISTRY_MUTATION_LOCK.lock().await;
    let mut next = registry.read().await.clone();
    if next
        .get(&registration.session)
        .and_then(lane_of)
        .is_some_and(|lane| lane.workflow != LaneWorkflow::Retired)
    {
        return Err("lane session is retained until explicit quiesced retirement".into());
    }
    next.insert(registration.session.clone(), registration);
    let durable_count = save_durable_tmux_registry(path, &next).await?;
    *registry.write().await = next;
    Ok(durable_count)
}

pub async fn remove_tmux_registrations(
    registry: &SharedTmuxRegistry,
    path: &Path,
    sessions: &[String],
) -> Result<usize> {
    let _mutation = REGISTRY_MUTATION_LOCK.lock().await;
    let mut next = registry.read().await.clone();
    let mut removed = 0;
    for session in sessions {
        let removable = next.get(session).is_none_or(|registration| {
            registration
                .lane
                .as_ref()
                .is_none_or(|lane| lane.workflow == LaneWorkflow::Retired && lane.quiesced)
        });

        if removable && next.remove(session).is_some() {
            removed += 1;
        }
    }
    if removed > 0 {
        save_durable_tmux_registry(path, &next).await?;
        *registry.write().await = next;
    }
    Ok(removed)
}

/// A candidate for dynamic-registration pruning identified by session name
/// and the registration generation observed at selection time. The
/// generation acts as a compare-and-swap token: if the current registry
/// entry has a different generation (because a newer registration arrived),
/// the prune is skipped for that entry.
#[derive(Debug, Clone)]
pub struct AbsentRegistrationCandidate {
    pub session: String,
    pub registration_generation: u64,
}

/// Prune daemon-owned dynamic registrations and explicitly retired/quiesced
/// lane registrations whose tmux session has been observed absent. Active and
/// handoff lane evidence is preserved regardless of liveness to avoid
/// destroying in-flight R0/R1/R2 lane evidence that is expected to exist
/// before the tmux session is created.
///
/// Config-monitor registrations are never selected or removed by this
/// function — they are reconciled declaratively via
/// `sync_active_config_registrations`.
pub async fn prune_absent_dynamic_registrations(
    registry: &SharedTmuxRegistry,
    path: &Path,
    candidates: &[AbsentRegistrationCandidate],
) -> Result<usize> {
    if candidates.is_empty() {
        return Ok(0);
    }
    let _mutation = REGISTRY_MUTATION_LOCK.lock().await;
    let mut next = registry.read().await.clone();
    let mut removed = 0;
    let mut first_removed: Option<&str> = None;
    for candidate in candidates {
        let removable = next.get(&candidate.session).is_some_and(|registration| {
            registration.registration_generation == candidate.registration_generation
                && registration.registration_source != RegistrationSource::ConfigMonitor
                && registration
                    .lane
                    .as_ref()
                    .is_none_or(|lane| lane.workflow == LaneWorkflow::Retired && lane.quiesced)
        });

        if removable && next.remove(&candidate.session).is_some() {
            if first_removed.is_none() {
                first_removed = Some(candidate.session.as_str());
            }
            removed += 1;
        }
    }
    if removed > 0 {
        save_durable_tmux_registry(path, &next).await?;
        *registry.write().await = next;
        let mut record = source_record(
            telemetry::event_name::SOURCE_INVENTORY,
            "dynamic_registration_pruned_absent",
            None,
            None,
        );
        record.insert("removed_count".to_string(), serde_json::json!(removed));
        if let Some(sample) = first_removed {
            record.insert("sample_session".to_string(), serde_json::json!(sample));
        }
        telemetry::emit(record);
    }
    Ok(removed)
}

pub async fn lane_detail_for_session(
    registry: &SharedTmuxRegistry,
    session: &str,
) -> Result<LaneDetail> {
    let runtime_live = None;
    let registration = registry
        .read()
        .await
        .get(session)
        .cloned()
        .ok_or("lane session not found")?;
    let lane = registration
        .lane
        .as_ref()
        .ok_or("session has no lane evidence")?;
    Ok(lane_detail(session, lane, runtime_live))
}
pub async fn register_lane_registration(
    registry: &SharedTmuxRegistry,
    path: &Path,
    input: LaneRegistrationInput,
) -> Result<LaneDetail> {
    if !input.expect_absent_or_retired
        || !valid_session(&input.registration.session)
        || !valid_id(&input.generation_id)
        || !valid_id(&input.kickoff_operation_id)
        || !valid_id(&input.launch_operation_id)
        || !valid_id(&input.executor_id)
        || input
            .thread_id
            .as_deref()
            .is_some_and(|value| !valid_discord_id(value))
    {
        return Err("invalid lane registration input".into());
    }
    let mut registration = input.registration;
    if registration.lane.is_some() {
        return Err("lane registration input must not contain evidence".into());
    }
    let session = registration.session.clone();
    registration.registration_source =
        normalize_runtime_registration_source(registration.registration_source);
    registration.registration_generation = mint_registration_generation();
    let _mutation = REGISTRY_MUTATION_LOCK.lock().await;
    let mut next = registry.read().await.clone();
    if let Some(existing) = next.get(&session) {
        if let Some(lane) = existing.lane.as_ref().filter(|lane| {
            lane.launch_state == LaneLaunchState::Ready
                && lane.revision == 0
                && lane.workflow == LaneWorkflow::Active
                && lane.generation_id == input.generation_id
                && lane.kickoff_operation_id == input.kickoff_operation_id
                && lane.launch_operation_id == input.launch_operation_id
                && lane.executor_id == input.executor_id
                && lane.worker_effect_kind == input.worker_effect_kind
                && lane.thread_id == input.thread_id
        }) {
            let mut public_existing = existing.clone();
            public_existing.lane = None;
            if serde_json::to_value(&public_existing).ok()
                == serde_json::to_value(&registration).ok()
            {
                return Ok(lane_detail(&session, lane, None));
            }
        }
        if existing
            .lane
            .as_ref()
            .is_none_or(|lane| lane.workflow != LaneWorkflow::Retired)
        {
            return Err("lane registration conflicts with existing session".into());
        }
    }
    registration.lane = Some(LaneEvidence {
        lane_version: 1,
        generation_id: input.generation_id,
        kickoff_operation_id: input.kickoff_operation_id,
        launch_operation_id: input.launch_operation_id,
        executor_id: input.executor_id,
        worker_effect_kind: input.worker_effect_kind,
        launch_state: LaneLaunchState::Ready,
        workflow: LaneWorkflow::Active,
        revision: 0,
        quiesced: false,
        thread_id: input.thread_id,
        kickoff_message_id: None,
        kickoff_delivered_at: None,
        visibility: Some(LaneVisibility::Unverified),
        verification: None,
        last_failure: None,
        latest_update_message_id: None,
        latest_update_kind: None,
        latest_update_delivered_at: None,
        delivery_retry_count: 0,
        delivery_disposition: None,
    });
    next.insert(session.clone(), registration);
    save_durable_tmux_registry(path, &next).await?;
    *registry.write().await = next;
    drop(_mutation);
    lane_detail_for_session(registry, &session).await
}
pub async fn record_lane_verification(
    registry: &SharedTmuxRegistry,
    path: &Path,
    input: LaneVerificationMutation,
) -> Result<LaneSnapshot> {
    {
        let snapshot = registry.read().await;
        let lane = snapshot
            .get(&input.session)
            .and_then(|registration| registration.lane.as_ref())
            .ok_or("lane session not found")?;
        if lane.revision == input.expected_revision.saturating_add(1)
            && lane.generation_id == input.generation_id
            && lane.visibility == Some(input.visibility)
            && lane.verification.as_ref().is_some_and(|value| {
                value.checked_at == input.checked_at
                    && value.outcome == input.outcome
                    && value.reason == input.reason
            })
        {
            return Ok(lane_snapshot(&input.session, lane, None));
        }
    }
    if !valid_verification(&input) || !valid_id(&input.generation_id) {
        return Err("invalid lane verification input".into());
    }
    mutate_lane(
        registry,
        path,
        &input.session,
        input.expected_revision,
        |lane| {
            if lane.generation_id != input.generation_id || lane.workflow == LaneWorkflow::Retired {
                return Err("lane generation conflict or retired".into());
            }
            lane.verification = Some(LaneVerification {
                checked_at: input.checked_at,
                outcome: input.outcome,
                reason: input.reason,
            });
            lane.visibility = Some(input.visibility);
            Ok(())
        },
    )
    .await
}
pub async fn record_lane_delivery(
    registry: &SharedTmuxRegistry,
    path: &Path,
    input: LaneDeliveryMutation,
) -> Result<LaneSnapshot> {
    if !valid_delivery(&input)
        || !valid_id(&input.generation_id)
        || (input.initial_kickoff
            && input
                .kickoff_operation_id
                .as_deref()
                .is_none_or(|value| !valid_id(value)))
    {
        return Err("invalid lane delivery input".into());
    }
    if input.initial_kickoff {
        let snapshot = registry.read().await;
        let lane = snapshot
            .get(&input.session)
            .and_then(|registration| registration.lane.as_ref())
            .ok_or("lane session not found")?;
        if lane.revision == input.expected_revision.saturating_add(1)
            && lane.generation_id == input.generation_id
            && lane.launch_state == LaneLaunchState::Ready
            && input.kind.as_deref() == Some("kickoff")
            && input.kickoff_operation_id.as_deref() == Some(lane.kickoff_operation_id.as_str())
            && lane.kickoff_message_id == input.message_id
            && lane.kickoff_delivered_at == input.delivered_at
            && lane.last_failure == input.error_category
            && lane.delivery_disposition == input.disposition
            && lane.visibility == Some(input.visibility)
            && input.workflow.is_none()
        {
            return Ok(lane_snapshot(&input.session, lane, None));
        }
    }
    if !input.initial_kickoff {
        let snapshot = registry.read().await;
        let lane = snapshot
            .get(&input.session)
            .and_then(|registration| registration.lane.as_ref())
            .ok_or("lane session not found")?;
        if lane.revision == input.expected_revision.saturating_add(1)
            && lane.generation_id == input.generation_id
            && lane.latest_update_message_id == input.message_id
            && lane.latest_update_kind == input.kind
            && lane.latest_update_delivered_at == input.delivered_at
            && lane.visibility == Some(input.visibility)
            && lane.last_failure == input.error_category
            && lane.delivery_disposition == input.disposition
            && input
                .workflow
                .is_none_or(|workflow| lane.workflow == workflow)
        {
            return Ok(lane_snapshot(&input.session, lane, None));
        }
    }
    mutate_lane(
        registry,
        path,
        &input.session,
        input.expected_revision,
        |lane| {
            if lane.generation_id != input.generation_id
                || (lane.workflow == LaneWorkflow::Retired
                    && input.workflow != Some(LaneWorkflow::Retired))
            {
                return Err("lane generation conflict or retired".into());
            }
            if let Some(workflow) = input.workflow {
                lane.workflow = workflow;
            }
            if input.initial_kickoff {
                if input.kind.as_deref() != Some("kickoff")
                    || input.kickoff_operation_id.as_deref()
                        != Some(lane.kickoff_operation_id.as_str())
                    || lane.launch_state != LaneLaunchState::Ready
                    || lane.kickoff_message_id.is_some()
                    || lane.delivery_disposition.is_some()
                {
                    return Err("initial kickoff fact conflict".into());
                }
                lane.kickoff_message_id = input.message_id;
                lane.kickoff_delivered_at = input.delivered_at;
            } else {
                lane.latest_update_message_id = input.message_id;
                lane.latest_update_kind = input.kind;
                lane.latest_update_delivered_at = input.delivered_at;
            }
            lane.visibility = Some(input.visibility);
            lane.last_failure = input.error_category;
            lane.delivery_disposition = input.disposition;
            lane.delivery_retry_count = lane.delivery_retry_count.saturating_add(1);
            Ok(())
        },
    )
    .await
}
pub async fn retire_lane_if_absent(
    registry: &SharedTmuxRegistry,
    path: &Path,
    input: LaneRetirementMutation,
) -> Result<LaneSnapshot> {
    if !valid_id(&input.generation_id) {
        return Err("invalid lane retirement generation".into());
    }
    {
        let snapshot = registry.read().await;
        let lane = snapshot
            .get(&input.session)
            .and_then(|registration| registration.lane.as_ref())
            .ok_or("lane session not found")?;
        if lane.revision == input.expected_revision.saturating_add(1)
            && lane.generation_id == input.generation_id
            && lane.workflow == LaneWorkflow::Retired
            && lane.quiesced
        {
            return Ok(lane_snapshot(&input.session, lane, None));
        }
    }
    if session_exists(&input.session).await? {
        return Err("lane runtime remains active".into());
    }
    mutate_lane(
        registry,
        path,
        &input.session,
        input.expected_revision,
        |lane| {
            if lane.generation_id != input.generation_id {
                return Err("lane generation conflict".into());
            }
            if lane.workflow == LaneWorkflow::Retired && lane.quiesced {
                return Ok(());
            }
            lane.workflow = LaneWorkflow::Retired;
            lane.quiesced = true;
            Ok(())
        },
    )
    .await
}

pub async fn list_lane_snapshots(
    registry: &SharedTmuxRegistry,
    session: Option<&str>,
) -> Vec<LaneSnapshot> {
    let runtime = list_tmux_sessions().await.ok();
    registry
        .read()
        .await
        .iter()
        .filter(|(name, registration)| {
            session.is_none_or(|requested| requested == name.as_str())
                && registration.lane.is_some()
        })
        .filter_map(|(name, registration)| {
            registration.lane.as_ref().map(|lane| {
                lane_snapshot(
                    name,
                    lane,
                    runtime.as_ref().map(|sessions| sessions.contains(name)),
                )
            })
        })
        .collect()
}

pub async fn claim_lane(
    registry: &SharedTmuxRegistry,
    path: &Path,
    session: &str,
    generation_id: &str,
    executor_id: &str,
    expected_revision: u64,
) -> Result<LaneSnapshot> {
    if !valid_id(generation_id) || !valid_id(executor_id) {
        return Err("invalid lane claim identity".into());
    }
    {
        let snapshot = registry.read().await;
        let lane = snapshot
            .get(session)
            .and_then(|registration| registration.lane.as_ref())
            .ok_or("lane session not found")?;
        if lane.workflow == LaneWorkflow::Retired {
            return Err("lane claim conflicts with retired workflow".into());
        }
        if lane.revision == expected_revision.saturating_add(1)
            && lane.generation_id == generation_id
            && lane.executor_id == executor_id
            && lane.launch_state == LaneLaunchState::Claimed
        {
            return Ok(lane_snapshot(session, lane, None));
        }
    }
    mutate_lane(registry, path, session, expected_revision, |lane| {
        if lane.generation_id != generation_id || lane.executor_id != executor_id {
            return Err("lane claim identity conflict".into());
        }
        if lane.workflow == LaneWorkflow::Retired {
            return Err("lane claim conflicts with retired workflow".into());
        }
        if lane.launch_state == LaneLaunchState::Ready {
            lane.launch_state = LaneLaunchState::Claimed;
        }
        if lane.launch_state != LaneLaunchState::Claimed {
            return Err("lane claim conflicts with durable state".into());
        }
        Ok(())
    })
    .await
}

pub async fn update_lane_evidence(
    registry: &SharedTmuxRegistry,
    path: &Path,
    input: LaneEvidenceMutation,
) -> Result<LaneSnapshot> {
    if !valid_id(&input.generation_id)
        || !valid_id(&input.launch_operation_id)
        || !valid_id(&input.executor_id)
    {
        return Err("invalid lane evidence identity".into());
    }
    {
        let snapshot = registry.read().await;
        let existing = snapshot
            .get(&input.session)
            .and_then(|registration| registration.lane.as_ref())
            .ok_or("lane session not found")?;
        if (existing.revision == input.expected_revision
            || existing.revision == input.expected_revision.saturating_add(1))
            && existing.generation_id == input.generation_id
            && existing.launch_operation_id == input.launch_operation_id
            && existing.executor_id == input.executor_id
            && existing.worker_effect_kind == input.worker_effect_kind
            && existing.launch_state == input.launch_state
            && existing.last_failure == input.failure_category
        {
            return Ok(lane_snapshot(&input.session, existing, None));
        }
    }
    mutate_lane(
        registry,
        path,
        &input.session,
        input.expected_revision,
        |lane| {
            if lane.generation_id != input.generation_id
                || lane.launch_operation_id != input.launch_operation_id
                || lane.executor_id != input.executor_id
                || lane.worker_effect_kind != input.worker_effect_kind
                || lane.workflow == LaneWorkflow::Retired
            {
                return Err("lane immutable identity conflict or retired".into());
            }
            if !terminal_category_is_valid(
                lane.worker_effect_kind,
                input.launch_state,
                input.failure_category.as_ref(),
            ) {
                return Err("invalid terminal category for worker effect kind".into());
            }
            let valid = lane.launch_state == input.launch_state
                || matches!(
                    (lane.launch_state, input.launch_state),
                    (LaneLaunchState::Claimed, LaneLaunchState::IdentityVerified)
                        | (LaneLaunchState::IdentityVerified, LaneLaunchState::Launched)
                        | (
                            LaneLaunchState::Claimed,
                            LaneLaunchState::NoWorkerEffect
                                | LaneLaunchState::CommandSubmitAmbiguous
                                | LaneLaunchState::SessionCreationAmbiguous
                        )
                        | (
                            LaneLaunchState::IdentityVerified,
                            LaneLaunchState::NoWorkerEffect
                                | LaneLaunchState::CommandSubmitAmbiguous
                                | LaneLaunchState::SessionCreationAmbiguous
                        )
                );
            if !valid {
                return Err("invalid lane launch transition".into());
            }
            lane.launch_state = input.launch_state;
            lane.last_failure = input.failure_category;
            Ok(())
        },
    )
    .await
}

pub async fn update_lane_workflow(
    registry: &SharedTmuxRegistry,
    path: &Path,
    input: LaneWorkflowMutation,
) -> Result<LaneSnapshot> {
    if !valid_id(&input.generation_id) {
        return Err("invalid lane workflow generation".into());
    }
    {
        let snapshot = registry.read().await;
        let lane = snapshot
            .get(&input.session)
            .and_then(|registration| registration.lane.as_ref())
            .ok_or("lane session not found")?;
        if lane.revision == input.expected_revision.saturating_add(1)
            && lane.generation_id == input.generation_id
            && lane.workflow == input.workflow
            && lane.quiesced == input.quiesced
        {
            return Ok(lane_snapshot(&input.session, lane, None));
        }
    }
    mutate_lane(
        registry,
        path,
        &input.session,
        input.expected_revision,
        |lane| {
            if lane.generation_id != input.generation_id || lane.workflow == LaneWorkflow::Retired {
                return Err("lane generation conflict or retired".into());
            }
            if input.workflow == LaneWorkflow::Retired {
                return Err("use daemon retirement endpoint".into());
            }
            lane.workflow = input.workflow;
            lane.quiesced = input.quiesced;
            Ok(())
        },
    )
    .await
}

async fn mutate_lane<F>(
    registry: &SharedTmuxRegistry,
    path: &Path,
    session: &str,
    expected_revision: u64,
    mutate: F,
) -> Result<LaneSnapshot>
where
    F: FnOnce(&mut LaneEvidence) -> Result<()>,
{
    let _mutation = REGISTRY_MUTATION_LOCK.lock().await;
    let mut next = registry.read().await.clone();
    let registration = next.get_mut(session).ok_or("lane session not found")?;
    let lane = registration
        .lane
        .as_mut()
        .ok_or("session has no lane evidence")?;
    if lane.revision != expected_revision {
        return Err("lane revision conflict".into());
    }
    mutate(lane)?;
    lane.revision = lane
        .revision
        .checked_add(1)
        .ok_or("lane revision exhausted")?;
    let snapshot = lane_snapshot(session, lane, None);
    save_durable_tmux_registry(path, &next).await?;
    *registry.write().await = next;
    Ok(snapshot)
}

pub async fn tmux_registry_diagnostics(
    registry: &SharedTmuxRegistry,
    registry_state: TmuxRegistryStateDiagnostics,
) -> TmuxRegistryDiagnostics {
    let snapshot = registry.read().await;
    let registered_count = snapshot.len();
    let durable_runtime_count = durable_runtime_entries(&snapshot).len();
    let config_projected_count = registered_count.saturating_sub(durable_runtime_count);
    drop(snapshot);

    let live_probe = match list_tmux_sessions().await {
        Ok(sessions) => {
            let mut sample = sessions.iter().take(5).cloned().collect::<Vec<_>>();
            sample.sort();
            TmuxLiveProbeDiagnostics {
                ok: true,
                count: sessions.len(),
                sample,
                error: None,
            }
        }
        Err(error) => TmuxLiveProbeDiagnostics {
            ok: false,
            count: 0,
            sample: Vec::new(),
            error: Some(error.to_string()),
        },
    };

    TmuxRegistryDiagnostics {
        registered_count,
        durable_runtime_count,
        config_projected_count,
        live_probe,
        registry_state,
    }
}

pub async fn list_active_tmux_registrations(
    config: &AppConfig,
    registry: &SharedTmuxRegistry,
    registry_state_path: &Path,
) -> Result<Vec<RegisteredTmuxSession>> {
    match list_tmux_sessions().await {
        Ok(available_sessions) => {
            sync_active_config_registrations(config, registry, &available_sessions).await;
            // Capture a single atomic snapshot of the registry after config
            // reconciliation so the generation captured for each candidate
            // matches the liveness observation.
            let snapshot = registry.read().await;
            let retired_lane_candidates: Vec<AbsentRegistrationCandidate> = snapshot
                .iter()
                .filter(|(session, registration)| {
                    !available_sessions.contains(*session)
                        && registration.lane.as_ref().is_some_and(|lane| {
                            lane.workflow == LaneWorkflow::Retired && lane.quiesced
                        })
                })
                .map(|(session, registration)| AbsentRegistrationCandidate {
                    session: session.clone(),
                    registration_generation: registration.registration_generation,
                })
                .collect();
            let mut orphaned_candidates: Vec<AbsentRegistrationCandidate> = snapshot
                .iter()
                .filter(|(session, registration)| {
                    !available_sessions.contains(*session)
                        && registration.registration_source != RegistrationSource::ConfigMonitor
                        && registration.lane.is_none()
                })
                .map(|(session, registration)| AbsentRegistrationCandidate {
                    session: session.clone(),
                    registration_generation: registration.registration_generation,
                })
                .collect();
            orphaned_candidates.extend(retired_lane_candidates);
            drop(snapshot);
            prune_absent_dynamic_registrations(registry, registry_state_path, &orphaned_candidates)
                .await?;
        }
        Err(error) => {
            telemetry::emit(source_record(
                telemetry::event_name::SOURCE_DEGRADED,
                "source_poll_failed",
                None,
                Some(error.to_string()),
            ));
            eprintln!("clawhip source tmux list-sessions failed: {error}");
        }
    }
    let snapshot = registry.read().await;
    Ok(sorted_registry_snapshot(&snapshot))
}

async fn poll_tmux(
    config: &AppConfig,
    registry: &SharedTmuxRegistry,
    registry_state_path: &Path,
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut TmuxMonitorState,
) -> Result<()> {
    let available_sessions = match list_tmux_sessions().await {
        Ok(sessions) => Some(sessions),
        Err(error) => {
            telemetry::emit(source_record(
                telemetry::event_name::SOURCE_DEGRADED,
                "source_poll_failed",
                None,
                Some(error.to_string()),
            ));
            eprintln!("clawhip source tmux list-sessions failed: {error}");
            None
        }
    };
    if let Some(available_sessions) = available_sessions.as_ref() {
        sync_active_config_registrations(config, registry, available_sessions).await;
    }
    let mut sessions = resolve_monitored_sessions(
        config
            .monitors
            .tmux
            .sessions
            .iter()
            .map(RegisteredTmuxSession::from)
            .collect(),
        available_sessions.as_ref(),
    );
    for (session, registration) in registry.read().await.iter() {
        sessions.insert(session.clone(), registration.clone());
    }

    let mut active_panes = HashSet::new();
    let mut sessions_to_unregister = Vec::new();
    let mut dynamic_prune_candidates = Vec::new();

    for (session_name, registration) in &sessions {
        let now = Instant::now();

        match session_exists(session_name).await {
            Ok(false) => {
                telemetry::emit(source_record(
                    telemetry::event_name::SOURCE_INVENTORY,
                    "source_missing",
                    Some(session_name),
                    None,
                ));
                let retired_lane = registration
                    .lane
                    .as_ref()
                    .is_some_and(|lane| lane.workflow == LaneWorkflow::Retired && lane.quiesced);
                if registration.registration_source != RegistrationSource::ConfigMonitor
                    && (registration.lane.is_none() || retired_lane)
                {
                    dynamic_prune_candidates.push(AbsentRegistrationCandidate {
                        session: session_name.clone(),
                        registration_generation: registration.registration_generation,
                    });
                } else {
                    sessions_to_unregister.push(session_name.clone());
                }
                state.pending_keyword_hits.remove(session_name);
                state.panes.retain(|_, pane| pane.session != *session_name);
                continue;
            }
            Err(error) => {
                telemetry::emit(source_record(
                    telemetry::event_name::SOURCE_DEGRADED,
                    "source_poll_failed",
                    Some(session_name),
                    Some(error.to_string()),
                ));
                eprintln!(
                    "clawhip source tmux has-session failed for {}: {error}",
                    session_name
                );
                continue;
            }
            Ok(true) => {}
        }

        if registration.active_wrapper_monitor {
            state.pending_keyword_hits.remove(session_name);
            continue;
        }

        flush_session_pending_keyword_hits(
            &mut state.pending_keyword_hits,
            session_name,
            registration,
            tx,
            now,
            false,
        )
        .await?;

        match snapshot_tmux_session(session_name).await {
            Ok(panes) => {
                for pane in panes {
                    let pane_key = format!("{}::{}", pane.session, pane.pane_id);
                    active_panes.insert(pane_key.clone());
                    let now = Instant::now();
                    let hash = content_hash(&pane.content);
                    let latest_line = last_nonempty_line(&pane.content);

                    if pane.pane_dead {
                        state.pending_keyword_hits.remove(session_name);
                    }

                    let hits = match state.panes.get_mut(&pane_key) {
                        None => {
                            state.panes.insert(
                                pane_key,
                                TmuxPaneState {
                                    session: pane.session,
                                    pane_name: pane.pane_name,
                                    snapshot: pane.content,
                                    content_hash: hash,
                                    last_change: now,
                                    last_stale_notification: None,
                                    pane_dead: pane.pane_dead,
                                },
                            );
                            None
                        }
                        Some(existing) => {
                            existing.pane_dead = pane.pane_dead;
                            if existing.content_hash != hash {
                                let hits = if pane.pane_dead {
                                    Vec::new()
                                } else {
                                    collect_keyword_hits_with_provenance(
                                        &existing.snapshot,
                                        &pane.content,
                                        &registration.keywords,
                                        KeywordMatchProvenance {
                                            pane_id: pane.pane_id.clone(),
                                            pane_name: pane.pane_name.clone(),
                                            cursor: None,
                                            source: KeywordMatchSource::FreshOutput,
                                        },
                                    )
                                };
                                existing.pane_name = pane.pane_name;
                                existing.snapshot = pane.content;
                                existing.content_hash = hash;
                                existing.last_change = now;
                                existing.last_stale_notification = None;
                                Some(hits)
                            } else {
                                if should_emit_stale(existing, now, registration.stale_minutes) {
                                    telemetry::emit(source_record(
                                        telemetry::event_name::SOURCE_INVENTORY,
                                        "stale_emitted",
                                        Some(session_name),
                                        None,
                                    ));
                                    tx.emit(tmux_stale_event(
                                        registration,
                                        existing.session.clone(),
                                        existing.pane_name.clone(),
                                        latest_line,
                                    ))
                                    .await?;
                                    existing.last_stale_notification = Some(now);
                                }
                                None
                            }
                        }
                    };

                    if let Some(hits) = hits {
                        push_session_pending_keyword_hits(
                            &mut state.pending_keyword_hits,
                            session_name,
                            now,
                            hits,
                        );
                    }
                }
            }
            Err(error) => {
                telemetry::emit(source_record(
                    telemetry::event_name::SOURCE_DEGRADED,
                    "source_snapshot_failed",
                    Some(session_name),
                    Some(error.to_string()),
                ));
                eprintln!(
                    "clawhip source tmux snapshot failed for {}: {error}",
                    session_name
                );
            }
        }
    }

    state.panes.retain(|key, _| active_panes.contains(key));

    if !sessions_to_unregister.is_empty() {
        remove_tmux_registrations(registry, registry_state_path, &sessions_to_unregister).await?;
    }
    if !dynamic_prune_candidates.is_empty() {
        prune_absent_dynamic_registrations(
            registry,
            registry_state_path,
            &dynamic_prune_candidates,
        )
        .await?;
    }

    state
        .pending_keyword_hits
        .retain(|session, _| sessions.contains_key(session));

    Ok(())
}

fn source_record(
    event_name: &str,
    reason_code: &str,
    session: Option<&str>,
    error: Option<String>,
) -> serde_json::Map<String, serde_json::Value> {
    let correlation = format!("source:tmux:{}", session.unwrap_or("inventory"));
    let mut record = telemetry::record(event_name, reason_code, correlation);
    record.insert("source".to_string(), serde_json::json!("tmux"));
    if let Some(session) = session {
        record.insert("session".to_string(), serde_json::json!(session));
    }
    if let Some(error) = error {
        record.insert("error".to_string(), serde_json::json!(error));
    }
    record
}

async fn sync_active_config_registrations(
    config: &AppConfig,
    registry: &SharedTmuxRegistry,
    available_sessions: &HashSet<String>,
) {
    let _mutation = REGISTRY_MUTATION_LOCK.lock().await;
    let existing_registry = registry.read().await.clone();
    let resolved = resolve_monitored_sessions(
        config
            .monitors
            .tmux
            .sessions
            .iter()
            .map(RegisteredTmuxSession::from)
            .collect(),
        Some(available_sessions),
    );
    let active_config = resolved
        .into_iter()
        .filter(|(session, _)| available_sessions.contains(session))
        .map(|(session, mut registration)| {
            if let Some(existing) = existing_registry.get(&session).filter(|existing| {
                !existing.active_wrapper_monitor
                    && existing.registration_source == RegistrationSource::ConfigMonitor
            }) {
                registration.registered_at = existing.registered_at.clone();
                registration.parent_process = existing.parent_process.clone();
            }
            (session, registration)
        })
        .collect();

    let mut write = registry.write().await;
    merge_active_config_registrations(&mut write, active_config);
}

fn merge_active_config_registrations(
    registry: &mut HashMap<String, RegisteredTmuxSession>,
    active_config: BTreeMap<String, RegisteredTmuxSession>,
) {
    let active_sessions: HashSet<String> = active_config.keys().cloned().collect();
    registry.retain(|session, registration| {
        registration.active_wrapper_monitor
            || registration.registration_source != RegistrationSource::ConfigMonitor
            || active_sessions.contains(session)
    });

    for (session, mut registration) in active_config {
        if let Some(existing) = registry.get(&session) {
            if existing.active_wrapper_monitor {
                continue;
            }
            if existing
                .lane
                .as_ref()
                .is_some_and(|lane| lane.workflow != LaneWorkflow::Retired)
            {
                continue;
            }
            if existing.registration_source == RegistrationSource::ConfigMonitor {
                registration.registered_at = existing.registered_at.clone();
                registration.parent_process = existing.parent_process.clone();
            }
        }
        registry.insert(session, registration);
    }
}

fn sorted_registry_snapshot(
    registry: &HashMap<String, RegisteredTmuxSession>,
) -> Vec<RegisteredTmuxSession> {
    let mut sessions: BTreeMap<String, RegisteredTmuxSession> = BTreeMap::new();
    for (session, registration) in registry {
        sessions.insert(session.clone(), registration.clone());
    }
    sessions.into_values().collect()
}

fn resolve_monitored_sessions(
    configured_sessions: Vec<RegisteredTmuxSession>,
    available_sessions: Option<&HashSet<String>>,
) -> BTreeMap<String, RegisteredTmuxSession> {
    let mut resolved: BTreeMap<String, (MonitorSpecificity, RegisteredTmuxSession)> =
        BTreeMap::new();

    for registration in configured_sessions {
        let specificity = MonitorSpecificity::for_pattern(&registration.session);
        let matched_sessions = available_sessions
            .into_iter()
            .flat_map(|sessions| sessions.iter())
            .filter(|session| glob_match(&registration.session, session))
            .cloned()
            .collect::<Vec<_>>();

        if matched_sessions.is_empty() {
            if !is_session_pattern(&registration.session) {
                insert_resolved_session(
                    &mut resolved,
                    registration.session.clone(),
                    specificity,
                    registration,
                );
            }
            continue;
        }

        for session in matched_sessions {
            let mut registration = registration.clone();
            registration.session = session.clone();
            insert_resolved_session(&mut resolved, session, specificity, registration);
        }
    }

    resolved
        .into_iter()
        .map(|(session, (_, registration))| (session, registration))
        .collect()
}

fn is_session_pattern(session: &str) -> bool {
    session.contains('*')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MonitorSpecificity {
    exact_match: bool,
    literal_chars: usize,
    wildcard_count: usize,
}

impl MonitorSpecificity {
    fn for_pattern(pattern: &str) -> Self {
        Self {
            exact_match: !is_session_pattern(pattern),
            literal_chars: pattern.chars().filter(|ch| *ch != '*').count(),
            wildcard_count: pattern.chars().filter(|ch| *ch == '*').count(),
        }
    }

    fn outranks(self, other: Self) -> bool {
        if self.exact_match != other.exact_match {
            return self.exact_match;
        }
        if self.literal_chars != other.literal_chars {
            return self.literal_chars > other.literal_chars;
        }

        self.wildcard_count < other.wildcard_count
    }
}

fn insert_resolved_session(
    resolved: &mut BTreeMap<String, (MonitorSpecificity, RegisteredTmuxSession)>,
    session: String,
    specificity: MonitorSpecificity,
    registration: RegisteredTmuxSession,
) {
    match resolved.get(&session) {
        Some((existing_specificity, _)) if !specificity.outranks(*existing_specificity) => {}
        _ => {
            resolved.insert(session, (specificity, registration));
        }
    }
}

fn should_emit_stale(pane: &TmuxPaneState, now: Instant, stale_minutes: u64) -> bool {
    if stale_minutes == 0 || pane.pane_dead {
        return false;
    }
    let stale_after = Duration::from_secs(stale_minutes * 60);
    now.duration_since(pane.last_change) >= stale_after
        && pane
            .last_stale_notification
            .map(|previous| now.duration_since(previous) >= stale_after)
            .unwrap_or(true)
}

fn tmux_keyword_event(
    registration: &RegisteredTmuxSession,
    session: String,
    hits: Vec<KeywordHit>,
) -> IncomingEvent {
    let event = if hits.len() <= 1 {
        match hits.into_iter().next() {
            Some(hit) => {
                let mut event = IncomingEvent::tmux_keyword(
                    session,
                    hit.keyword,
                    hit.line,
                    registration.channel.clone(),
                );
                add_keyword_hit_provenance(&mut event.payload, hit.provenance.as_ref());
                event
            }
            None => IncomingEvent::tmux_keyword(
                session,
                String::new(),
                String::new(),
                registration.channel.clone(),
            ),
        }
    } else if hits.iter().all(|hit| hit.provenance.is_none()) {
        IncomingEvent::tmux_keywords(
            session,
            hits.into_iter()
                .map(|hit| (hit.keyword, hit.line))
                .collect(),
            registration.channel.clone(),
        )
    } else {
        let hit_payloads = hits
            .into_iter()
            .map(|hit| {
                let mut payload = serde_json::json!({
                    "keyword": hit.keyword,
                    "line": hit.line,
                });
                add_keyword_hit_provenance(&mut payload, hit.provenance.as_ref());
                payload
            })
            .collect::<Vec<_>>();
        tmux_keyword_event_from_hit_payloads(session, registration.channel.clone(), hit_payloads)
    };

    event
        .with_routing_metadata(&registration.routing)
        .with_mention(registration.mention.clone())
        .with_format(registration.format.clone())
}

fn tmux_keyword_event_from_hit_payloads(
    session: String,
    channel: Option<String>,
    hit_payloads: Vec<serde_json::Value>,
) -> IncomingEvent {
    let hit_count = hit_payloads.len();
    let first_keyword = hit_payloads
        .first()
        .and_then(|hit| hit.get("keyword"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let first_line = hit_payloads
        .first()
        .and_then(|hit| hit.get("line"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();

    IncomingEvent {
        kind: "tmux.keyword".to_string(),
        channel,
        mention: None,
        format: None,
        template: None,
        payload: serde_json::json!({
            "session": session,
            "keyword": first_keyword,
            "line": first_line,
            "hit_count": hit_count,
            "hits": hit_payloads,
        }),
    }
}

fn add_keyword_hit_provenance(
    payload: &mut serde_json::Value,
    provenance: Option<&KeywordMatchProvenance>,
) {
    let Some(provenance) = provenance else {
        return;
    };
    let Some(object) = payload.as_object_mut() else {
        return;
    };

    object.insert("pane_id".to_string(), serde_json::json!(provenance.pane_id));
    object.insert(
        "pane_name".to_string(),
        serde_json::json!(provenance.pane_name),
    );
    if let Some(cursor) = provenance.cursor {
        object.insert("cursor".to_string(), serde_json::json!(cursor));
    }
    object.insert("source".to_string(), serde_json::json!("fresh-output"));
}

fn tmux_stale_event(
    registration: &RegisteredTmuxSession,
    session: String,
    pane: String,
    last_line: String,
) -> IncomingEvent {
    IncomingEvent::tmux_stale(
        session,
        pane,
        registration.stale_minutes,
        last_line,
        registration.channel.clone(),
    )
    .with_routing_metadata(&registration.routing)
    .with_mention(registration.mention.clone())
    .with_format(registration.format.clone())
}

async fn flush_pending_keyword_hits<E: EventEmitter>(
    pending_keyword_hits: &mut Option<PendingKeywordHits>,
    registration: &RegisteredTmuxSession,
    emitter: &E,
    session: &str,
    now: Instant,
    keyword_window: Duration,
    force: bool,
) -> Result<()> {
    let should_flush = pending_keyword_hits
        .as_ref()
        .map(|pending| force || pending.ready_to_flush(now, keyword_window))
        .unwrap_or(false);
    if !should_flush {
        return Ok(());
    }

    let Some(pending) = pending_keyword_hits.take() else {
        return Ok(());
    };
    let hits = pending.into_hits();
    if hits.is_empty() {
        return Ok(());
    }

    emitter
        .emit(tmux_keyword_event(registration, session.to_string(), hits))
        .await
}

async fn flush_session_pending_keyword_hits<E: EventEmitter>(
    pending_keyword_hits: &mut HashMap<String, PendingKeywordHits>,
    session: &str,
    registration: &RegisteredTmuxSession,
    emitter: &E,
    now: Instant,
    force: bool,
) -> Result<()> {
    let mut pending = pending_keyword_hits.remove(session);
    flush_pending_keyword_hits(
        &mut pending,
        registration,
        emitter,
        session,
        now,
        Duration::from_secs(registration.keyword_window_secs.max(1)),
        force,
    )
    .await?;
    if let Some(pending) = pending {
        pending_keyword_hits.insert(session.to_string(), pending);
    }
    Ok(())
}

fn push_pending_keyword_hits(
    pending_keyword_hits: &mut Option<PendingKeywordHits>,
    now: Instant,
    hits: Vec<crate::keyword_window::KeywordHit>,
) {
    if hits.is_empty() {
        return;
    }

    pending_keyword_hits
        .get_or_insert_with(|| PendingKeywordHits::new(now))
        .push(hits);
}

fn push_session_pending_keyword_hits(
    pending_keyword_hits: &mut HashMap<String, PendingKeywordHits>,
    session: &str,
    now: Instant,
    hits: Vec<crate::keyword_window::KeywordHit>,
) {
    if hits.is_empty() {
        return;
    }

    pending_keyword_hits
        .entry(session.to_string())
        .or_insert_with(|| PendingKeywordHits::new(now))
        .push(hits);
}

pub(crate) async fn session_exists(session: &str) -> Result<bool> {
    let output = Command::new(tmux_bin())
        .arg("has-session")
        .arg("-t")
        .arg(session)
        .output()
        .await?;
    Ok(output.status.success())
}

async fn list_tmux_sessions() -> Result<HashSet<String>> {
    let output = Command::new(tmux_bin())
        .arg("list-sessions")
        .arg("-F")
        .arg("#{session_name}")
        .output()
        .await?;
    if !output.status.success() {
        return Err(tmux_stderr(&output.stderr).into());
    }

    Ok(String::from_utf8(output.stdout)?
        .lines()
        .map(str::trim)
        .filter(|session| !session.is_empty())
        .map(ToString::to_string)
        .collect())
}

async fn snapshot_tmux_session(session: &str) -> Result<Vec<TmuxPaneSnapshot>> {
    let output = Command::new(tmux_bin())
        .arg("list-panes")
        .arg("-t")
        .arg(session)
        .arg("-F")
        .arg("#{pane_id}|#{session_name}|#{window_index}.#{pane_index}|#{pane_dead}|#{pane_title}")
        .output()
        .await?;
    if !output.status.success() {
        return Err(tmux_stderr(&output.stderr).into());
    }

    let mut panes = Vec::new();
    for line in String::from_utf8(output.stdout)?.lines() {
        let mut parts = line.splitn(5, '|');
        let pane_id = parts.next().unwrap_or_default().to_string();
        if pane_id.is_empty() {
            continue;
        }
        let session_name = parts.next().unwrap_or_default().to_string();
        let pane_name = parts.next().unwrap_or_default().to_string();
        let pane_dead = parts.next().unwrap_or_default() == "1";
        let capture = Command::new(tmux_bin())
            .arg("capture-pane")
            .arg("-p")
            .arg("-t")
            .arg(&pane_id)
            .arg("-S")
            .arg("-200")
            .output()
            .await?;
        if !capture.status.success() {
            return Err(tmux_stderr(&capture.stderr).into());
        }
        panes.push(TmuxPaneSnapshot {
            pane_id,
            session: session_name,
            pane_name,
            content: String::from_utf8(capture.stdout)?,
            pane_dead,
        });
    }
    Ok(panes)
}

pub(crate) fn content_hash(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn last_nonempty_line(content: &str) -> String {
    content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("<no output>")
        .trim()
        .to_string()
}

pub(crate) fn tmux_bin() -> String {
    std::env::var("CLAWHIP_TMUX_BIN").unwrap_or_else(|_| "tmux".to_string())
}

fn tmux_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).trim().to_string()
}

fn default_keyword_window_secs() -> u64 {
    30
}

pub fn current_timestamp_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventBody, compat::from_incoming_event};
    use crate::keyword_window::{KeywordHit, collect_keyword_hits};

    fn registration(keywords: Vec<&str>) -> RegisteredTmuxSession {
        RegisteredTmuxSession {
            session: "issue-24".into(),
            channel: Some("alerts".into()),
            mention: Some("<@123>".into()),
            routing: RoutingMetadata::default(),
            keywords: keywords.into_iter().map(str::to_string).collect(),
            keyword_window_secs: 30,
            stale_minutes: 15,
            format: Some(MessageFormat::Compact),
            registered_at: "2026-04-02T00:00:00Z".into(),
            registration_source: RegistrationSource::ConfigMonitor,
            parent_process: None,
            registration_generation: 0,
            active_wrapper_monitor: false,
            lane: None,
        }
    }

    fn lane_input(session: &str, generation_id: &str) -> LaneRegistrationInput {
        let mut registration = registration(Vec::new());
        registration.session = session.into();
        LaneRegistrationInput {
            registration,
            generation_id: generation_id.into(),
            kickoff_operation_id: "k".into(),
            launch_operation_id: "l".into(),
            executor_id: "e".into(),
            worker_effect_kind: WorkerEffectKind::CommandSubmission,
            thread_id: Some("12345678901234567890".into()),
            expect_absent_or_retired: true,
        }
    }

    #[test]
    fn verification_reason_outcome_pairs_are_sanitized() {
        let input = |outcome: &str, reason: Option<&str>| LaneVerificationMutation {
            session: "lane".into(),
            expected_revision: 0,
            checked_at: "2026-07-11T00:00:00Z".into(),
            outcome: outcome.into(),
            reason: reason.map(BoundedReason::new),
            visibility: LaneVisibility::Unverified,
            generation_id: "g".into(),
        };
        assert!(valid_verification(&input(
            "unverified",
            Some("unauthorized")
        )));
        assert!(valid_verification(&input("unverified", Some("timeout"))));
        assert!(valid_verification(&input("unreachable", Some("forbidden"))));
        assert!(valid_verification(&input(
            "message-observed-unverified",
            Some("kickoff-receipt-missing")
        )));
        assert!(!valid_verification(&input(
            "unverified",
            Some("private-target:secret")
        )));
    }

    #[test]
    fn delivery_categories_are_partitioned_by_disposition() {
        let input = |disposition: Option<&str>, category: Option<&str>| LaneDeliveryMutation {
            session: "lane".into(),
            expected_revision: 0,
            generation_id: "g".into(),
            workflow: None,
            message_id: None,
            kind: Some("progress".into()),
            delivered_at: None,
            visibility: LaneVisibility::DeliveryFailed,
            error_category: category.map(BoundedCategory::new),
            disposition: disposition.map(str::to_owned),
            initial_kickoff: false,
            kickoff_operation_id: None,
        };
        assert!(valid_delivery(&input(
            Some("definitive-failure"),
            Some("forbidden")
        )));
        assert!(valid_delivery(&input(
            Some("definitive-failure"),
            Some("transport")
        )));
        let mut ambiguous = input(Some("ambiguous-acceptance"), Some("timeout"));
        ambiguous.visibility = LaneVisibility::Unverified;
        assert!(valid_delivery(&ambiguous));
        assert!(!valid_delivery(&input(
            Some("definitive-failure"),
            Some("timeout")
        )));
        ambiguous.error_category = Some(BoundedCategory::new("forbidden"));
        assert!(!valid_delivery(&ambiguous));
        assert!(!valid_delivery(&input(None, None)));
        let mut initial_empty = input(None, None);
        initial_empty.initial_kickoff = true;
        initial_empty.kind = Some("kickoff".into());
        initial_empty.kickoff_operation_id = Some("k".into());
        assert!(!valid_delivery(&initial_empty));
    }

    #[test]
    fn success_and_pending_launch_states_reject_categories() {
        let category = BoundedCategory::new("arbitrary");
        for state in [
            LaneLaunchState::Ready,
            LaneLaunchState::Claimed,
            LaneLaunchState::IdentityVerified,
            LaneLaunchState::Launched,
        ] {
            assert!(terminal_category_is_valid(
                WorkerEffectKind::CommandSubmission,
                state,
                None
            ));
            assert!(!terminal_category_is_valid(
                WorkerEffectKind::CommandSubmission,
                state,
                Some(&category)
            ));
        }
        assert!(!terminal_category_is_valid(
            WorkerEffectKind::SessionCreation,
            LaneLaunchState::NoWorkerEffect,
            Some(&category)
        ));
        assert!(!terminal_category_is_valid(
            WorkerEffectKind::CommandSubmission,
            LaneLaunchState::NoWorkerEffect,
            Some(&BoundedCategory::new("identity-marker-mismatch"))
        ));
    }

    #[test]
    fn command_session_creation_ambiguity_has_valid_terminal_categories_and_snapshot() {
        for category in [
            "owner-aborted-before-r2",
            "identity-marker-set-failed",
            "identity-marker-read-failed",
            "identity-marker-mismatch",
            "r1-to-r2-persistence-failed-after-create",
        ] {
            assert!(terminal_category_is_valid(
                WorkerEffectKind::CommandSubmission,
                LaneLaunchState::SessionCreationAmbiguous,
                Some(&BoundedCategory::new(category)),
            ));
        }

        let lane = LaneEvidence {
            lane_version: 1,
            generation_id: "g".into(),
            kickoff_operation_id: "k".into(),
            launch_operation_id: "l".into(),
            executor_id: "e".into(),
            worker_effect_kind: WorkerEffectKind::CommandSubmission,
            launch_state: LaneLaunchState::SessionCreationAmbiguous,
            workflow: LaneWorkflow::Active,
            revision: 0,
            quiesced: false,
            thread_id: None,
            kickoff_message_id: None,
            kickoff_delivered_at: None,
            visibility: None,
            verification: None,
            last_failure: Some(BoundedCategory::new("identity-marker-mismatch")),
            latest_update_message_id: None,
            latest_update_kind: None,
            latest_update_delivered_at: None,
            delivery_retry_count: 0,
            delivery_disposition: None,
        };
        let snapshot = lane_snapshot("lane", &lane, None);
        assert_eq!(
            snapshot.derived_status.as_deref(),
            Some("session-creation-ambiguous-blocked")
        );
        assert_eq!(
            snapshot.exit_category.as_ref().map(BoundedCategory::as_str),
            Some("identity-marker-mismatch")
        );
        assert_eq!(snapshot.worker_started, None);
    }

    #[tokio::test]
    async fn command_session_creation_ambiguity_transition_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        register_lane_registration(&registry, &path, lane_input("lane", "g"))
            .await
            .unwrap();
        let claimed = claim_lane(&registry, &path, "lane", "g", "e", 0)
            .await
            .unwrap();
        let snapshot = update_lane_evidence(
            &registry,
            &path,
            LaneEvidenceMutation {
                session: "lane".into(),
                expected_revision: claimed.revision,
                generation_id: "g".into(),
                launch_operation_id: "l".into(),
                launch_state: LaneLaunchState::SessionCreationAmbiguous,
                failure_category: Some(BoundedCategory::new("identity-marker-mismatch")),
                executor_id: "e".into(),
                worker_effect_kind: WorkerEffectKind::CommandSubmission,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            snapshot.durable_launch_state,
            LaneLaunchState::SessionCreationAmbiguous
        );
        assert_eq!(
            snapshot.exit_category.as_ref().map(BoundedCategory::as_str),
            Some("identity-marker-mismatch")
        );
    }

    #[tokio::test]
    async fn lane_registration_constructs_canonical_r0_and_rejects_legacy_collision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        let detail = register_lane_registration(&registry, &path, lane_input("r0", "g0"))
            .await
            .unwrap();
        assert_eq!(detail.snapshot.durable_launch_state, LaneLaunchState::Ready);
        assert_eq!(detail.snapshot.revision, 0);
        assert_eq!(detail.snapshot.workflow, LaneWorkflow::Active);
        assert_eq!(detail.snapshot.visibility, Some(LaneVisibility::Unverified));
        assert_eq!(detail.kickoff_message_id, None);
        registry
            .write()
            .await
            .insert("legacy".into(), registration(Vec::new()));
        assert!(
            register_lane_registration(&registry, &path, lane_input("legacy", "g1"))
                .await
                .is_err()
        );
    }

    #[test]
    fn ready_kickoff_failure_snapshot_retains_canonical_reason_and_json_null_worker() {
        let mut registration = registration(Vec::new());
        registration.lane = Some(LaneEvidence {
            lane_version: 1,
            generation_id: "g".into(),
            kickoff_operation_id: "k".into(),
            launch_operation_id: "l".into(),
            executor_id: "e".into(),
            worker_effect_kind: WorkerEffectKind::CommandSubmission,
            launch_state: LaneLaunchState::Ready,
            workflow: LaneWorkflow::Active,
            revision: 0,
            quiesced: false,
            thread_id: Some("thread".into()),
            kickoff_message_id: None,
            kickoff_delivered_at: None,
            visibility: Some(LaneVisibility::DeliveryFailed),
            verification: None,
            last_failure: Some(BoundedCategory::new("missing-message-id")),
            latest_update_message_id: None,
            latest_update_kind: None,
            latest_update_delivered_at: None,
            delivery_retry_count: 1,
            delivery_disposition: Some("definitive-failure".into()),
        });
        let snapshot = lane_snapshot("lane", registration.lane.as_ref().unwrap(), None);
        assert_eq!(snapshot.derived_status.as_deref(), Some("kickoff-failed"));
        assert_eq!(
            snapshot
                .observation_reason
                .as_ref()
                .map(BoundedReason::as_str),
            Some("missing-message-id")
        );
        assert_eq!(
            snapshot.exit_category.as_ref().map(BoundedCategory::as_str),
            Some("missing-message-id")
        );
        assert_eq!(snapshot.worker_started, None);
        let json = serde_json::to_value(snapshot).unwrap();
        assert_eq!(json["worker_started"], serde_json::Value::Null);
    }

    #[test]
    fn committed_terminal_snapshots_serialize_exact_categories_and_null_worker() {
        for (state, kind, category, status) in [
            (
                LaneLaunchState::SessionCreationAmbiguous,
                WorkerEffectKind::CommandSubmission,
                "identity-marker-mismatch",
                "session-creation-ambiguous-blocked",
            ),
            (
                LaneLaunchState::SessionCreationAmbiguous,
                WorkerEffectKind::SessionCreation,
                "owner-aborted-before-r2",
                "session-creation-ambiguous-blocked",
            ),
            (
                LaneLaunchState::NoWorkerEffect,
                WorkerEffectKind::CommandSubmission,
                "launch-failed-no-worker-effect",
                "launch-failed-no-worker-effect",
            ),
        ] {
            let lane = LaneEvidence {
                lane_version: 1,
                generation_id: "g".into(),
                kickoff_operation_id: "k".into(),
                launch_operation_id: "l".into(),
                executor_id: "e".into(),
                worker_effect_kind: kind,
                launch_state: state,
                workflow: LaneWorkflow::Active,
                revision: 0,
                quiesced: false,
                thread_id: None,
                kickoff_message_id: None,
                kickoff_delivered_at: None,
                visibility: None,
                verification: None,
                last_failure: Some(BoundedCategory::new(category)),
                latest_update_message_id: None,
                latest_update_kind: None,
                latest_update_delivered_at: None,
                delivery_retry_count: 0,
                delivery_disposition: None,
            };
            let value = serde_json::to_value(lane_snapshot("lane", &lane, None)).unwrap();
            assert_eq!(value["derived_status"], status);
            assert_eq!(value["observation_reason"], category);
            assert_eq!(value["exit_category"], category);
            assert_eq!(value["worker_started"], serde_json::Value::Null);
            assert_eq!(value["evidence_commit"], true);
            assert_eq!(value["repair_needed"], false);
            assert_eq!(value["healthy"], false);
        }
    }

    #[test]
    fn ready_kickoff_ambiguity_snapshot_retains_canonical_reason() {
        let mut registration = registration(Vec::new());
        registration.lane = Some(LaneEvidence {
            lane_version: 1,
            generation_id: "g".into(),
            kickoff_operation_id: "k".into(),
            launch_operation_id: "l".into(),
            executor_id: "e".into(),
            worker_effect_kind: WorkerEffectKind::CommandSubmission,
            launch_state: LaneLaunchState::Ready,
            workflow: LaneWorkflow::Active,
            revision: 0,
            quiesced: false,
            thread_id: Some("thread".into()),
            kickoff_message_id: None,
            kickoff_delivered_at: None,
            visibility: Some(LaneVisibility::Unreachable),
            verification: None,
            last_failure: Some(BoundedCategory::new("transport-failed")),
            latest_update_message_id: None,
            latest_update_kind: None,
            latest_update_delivered_at: None,
            delivery_retry_count: 1,
            delivery_disposition: Some("ambiguous-acceptance".into()),
        });
        let snapshot = lane_snapshot("lane", registration.lane.as_ref().unwrap(), None);
        assert_eq!(
            snapshot.derived_status.as_deref(),
            Some("kickoff-ambiguous")
        );
        assert_eq!(
            snapshot
                .observation_reason
                .as_ref()
                .map(BoundedReason::as_str),
            Some("transport-failed")
        );
        assert!(snapshot.evidence_commit && snapshot.repair_needed && !snapshot.healthy);
    }

    #[tokio::test]
    async fn lane_detail_is_registry_only_and_reports_unknown_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        register_lane_registration(
            &registry,
            &dir.path().join("registry.json"),
            lane_input("no-tmux-probe", "g"),
        )
        .await
        .unwrap();
        let detail = lane_detail_for_session(&registry, "no-tmux-probe")
            .await
            .unwrap();
        assert!(!detail.snapshot.healthy);
        assert_eq!(detail.snapshot.derived_status.as_deref(), Some("pending"));
    }

    #[tokio::test]
    async fn retired_lane_replaces_but_generation_fences_and_retired_writes_reject() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        register_lane_registration(&registry, &path, lane_input("lane", "old"))
            .await
            .unwrap();
        registry
            .write()
            .await
            .get_mut("lane")
            .unwrap()
            .lane
            .as_mut()
            .unwrap()
            .workflow = LaneWorkflow::Retired;
        assert_eq!(
            register_lane_registration(&registry, &path, lane_input("lane", "new"))
                .await
                .unwrap()
                .snapshot
                .generation_id,
            "new"
        );
        assert!(
            record_lane_verification(
                &registry,
                &path,
                LaneVerificationMutation {
                    session: "lane".into(),
                    expected_revision: 0,
                    generation_id: "old".into(),
                    checked_at: "now".into(),
                    outcome: "visible".into(),
                    reason: None,
                    visibility: LaneVisibility::Visible
                }
            )
            .await
            .is_err()
        );
        assert!(
            update_lane_workflow(
                &registry,
                &path,
                LaneWorkflowMutation {
                    session: "lane".into(),
                    generation_id: "old".into(),
                    expected_revision: 0,
                    workflow: LaneWorkflow::NeedsReview,
                    quiesced: false
                }
            )
            .await
            .is_err()
        );
        registry
            .write()
            .await
            .get_mut("lane")
            .unwrap()
            .lane
            .as_mut()
            .unwrap()
            .workflow = LaneWorkflow::Retired;
        assert!(
            record_lane_delivery(
                &registry,
                &path,
                LaneDeliveryMutation {
                    session: "lane".into(),
                    expected_revision: 0,
                    generation_id: "new".into(),
                    workflow: None,
                    message_id: None,
                    kind: None,
                    delivered_at: None,
                    visibility: LaneVisibility::Unverified,
                    error_category: None,
                    disposition: None,
                    initial_kickoff: false,
                    kickoff_operation_id: None,
                }
            )
            .await
            .is_err()
        );
        assert!(
            record_lane_delivery(
                &registry,
                &path,
                LaneDeliveryMutation {
                    session: "lane".into(),
                    expected_revision: 0,
                    generation_id: "new".into(),
                    workflow: Some(LaneWorkflow::Retired),
                    message_id: Some("12345678901234567890".into()),
                    kind: Some("progress".into()),
                    delivered_at: Some("2026-07-11T00:00:00Z".into()),
                    visibility: LaneVisibility::Visible,
                    error_category: None,
                    disposition: Some("accepted".into()),
                    initial_kickoff: false,
                    kickoff_operation_id: None,
                }
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn claim_response_loss_replay_and_retired_claim_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        register_lane_registration(&registry, &path, lane_input("lane", "g"))
            .await
            .unwrap();
        assert_eq!(
            claim_lane(&registry, &path, "lane", "g", "e", 0)
                .await
                .unwrap()
                .revision,
            1
        );
        assert_eq!(
            claim_lane(&registry, &path, "lane", "g", "e", 0)
                .await
                .unwrap()
                .revision,
            1
        );
        registry
            .write()
            .await
            .get_mut("lane")
            .unwrap()
            .lane
            .as_mut()
            .unwrap()
            .workflow = LaneWorkflow::Retired;
        assert!(
            claim_lane(&registry, &path, "lane", "g", "e", 1)
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn evidence_replay_is_idempotent_and_immutable_facts_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        register_lane_registration(&registry, &path, lane_input("lane", "g"))
            .await
            .unwrap();
        let replay = LaneEvidenceMutation {
            session: "lane".into(),
            expected_revision: 0,
            generation_id: "g".into(),
            launch_operation_id: "l".into(),
            launch_state: LaneLaunchState::Ready,
            failure_category: None,
            executor_id: "e".into(),
            worker_effect_kind: WorkerEffectKind::CommandSubmission,
        };
        assert_eq!(
            update_lane_evidence(&registry, &path, replay.clone())
                .await
                .unwrap()
                .revision,
            0
        );
        let claimed = claim_lane(&registry, &path, "lane", "g", "e", 0)
            .await
            .unwrap();
        let transition = LaneEvidenceMutation {
            expected_revision: claimed.revision,
            launch_state: LaneLaunchState::IdentityVerified,
            ..replay.clone()
        };
        assert_eq!(
            update_lane_evidence(&registry, &path, transition.clone())
                .await
                .unwrap()
                .revision,
            2
        );
        assert_eq!(
            update_lane_evidence(&registry, &path, transition)
                .await
                .unwrap()
                .revision,
            2
        );
        assert!(
            update_lane_evidence(
                &registry,
                &path,
                LaneEvidenceMutation {
                    executor_id: "other".into(),
                    ..replay
                }
            )
            .await
            .is_err()
        );
    }

    fn keyword_hit(keyword: &str, line: &str) -> KeywordHit {
        KeywordHit {
            keyword: keyword.into(),
            line: line.into(),
            provenance: None,
        }
    }

    #[test]
    fn keyword_hits_only_emit_for_new_lines() {
        let hits = collect_keyword_hits(
            "done
all good",
            "done
all good
error: failed
PR created #7",
            &["error".into(), "PR created".into()],
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].keyword, "error");
        assert_eq!(hits[1].keyword, "PR created");
    }

    #[test]
    fn tmux_keyword_event_inherits_channel_format_and_mention() {
        let mut registration = registration(vec!["error"]);
        registration.format = Some(MessageFormat::Alert);

        let event = tmux_keyword_event(
            &registration,
            "issue-24".into(),
            vec![keyword_hit("error", "boom")],
        );

        assert_eq!(event.channel.as_deref(), Some("alerts"));
        assert_eq!(event.mention.as_deref(), Some("<@123>"));
        assert!(matches!(event.format, Some(MessageFormat::Alert)));
        assert_eq!(event.payload["session"], "issue-24");
        assert_eq!(event.payload["keyword"], "error");
        assert_eq!(event.payload["line"], "boom");
        assert_eq!(event.payload["hit_count"], serde_json::Value::Null);
    }

    #[test]
    fn tmux_keyword_event_includes_match_provenance() {
        let registration = registration(vec!["error"]);
        let event = tmux_keyword_event(
            &registration,
            "issue-24".into(),
            vec![KeywordHit {
                keyword: "error".into(),
                line: "error: failed".into(),
                provenance: Some(KeywordMatchProvenance {
                    pane_id: "%3".into(),
                    pane_name: "0.1".into(),
                    cursor: Some(42),
                    source: KeywordMatchSource::FreshOutput,
                }),
            }],
        );

        assert_eq!(event.payload["pane_id"], "%3");
        assert_eq!(event.payload["pane_name"], "0.1");
        assert_eq!(event.payload["cursor"], 42);
        assert_eq!(event.payload["source"], "fresh-output");
    }

    #[test]
    fn tmux_keyword_event_carries_registered_routing_metadata() {
        let mut registration = registration(vec!["error"]);
        registration.routing = RoutingMetadata {
            project: Some("clawhip".into()),
            repo_name: Some("clawhip".into()),
            worktree_path: Some("/repo/clawhip.worktrees/issue-152".into()),
            ..RoutingMetadata::default()
        };

        let event = tmux_keyword_event(
            &registration,
            "clawhip-issue-152".into(),
            vec![keyword_hit("error", "boom")],
        );

        assert_eq!(event.payload["project"], "clawhip");
        assert_eq!(event.payload["repo_name"], "clawhip");
        assert_eq!(
            event.payload["worktree_path"],
            "/repo/clawhip.worktrees/issue-152"
        );
    }

    #[test]
    fn tmux_keyword_event_uses_aggregated_body_for_multi_hit_windows() {
        let mut registration = registration(vec!["error", "complete"]);
        registration.format = Some(MessageFormat::Alert);

        let event = tmux_keyword_event(
            &registration,
            "issue-24".into(),
            vec![
                keyword_hit("error", "boom"),
                keyword_hit("complete", "done"),
            ],
        );

        match from_incoming_event(&event).unwrap().body {
            EventBody::TmuxKeywordAggregated(body) => {
                assert_eq!(body.session, "issue-24");
                assert_eq!(body.hit_count, 2);
                assert_eq!(body.hits.len(), 2);
            }
            other => panic!("expected aggregated tmux keyword body, got {other:?}"),
        }
    }

    #[test]
    fn tmux_stale_event_inherits_channel_format_and_mention() {
        let mut registration = registration(vec!["error"]);
        registration.format = Some(MessageFormat::Inline);

        let event = tmux_stale_event(
            &registration,
            "issue-24".into(),
            "0.0".into(),
            "waiting".into(),
        );

        assert_eq!(event.channel.as_deref(), Some("alerts"));
        assert_eq!(event.mention.as_deref(), Some("<@123>"));
        assert!(matches!(event.format, Some(MessageFormat::Inline)));
        assert_eq!(event.payload["session"], "issue-24");
        assert_eq!(event.payload["pane"], "0.0");
        assert_eq!(event.payload["minutes"], 15);
        assert_eq!(event.payload["last_line"], "waiting");
    }

    #[test]
    fn config_monitor_registration_sets_audit_defaults() {
        let monitor = TmuxSessionMonitor {
            session: "issue-*".into(),
            channel: Some("alerts".into()),
            channel_name: None,
            mention: None,
            keywords: vec!["panic".into()],
            keyword_window_secs: 30,
            stale_minutes: 10,
            format: None,
        };

        let registration = RegisteredTmuxSession::from(&monitor);

        assert!(matches!(
            registration.registration_source,
            RegistrationSource::ConfigMonitor
        ));
        assert!(!registration.registered_at.is_empty());
        assert!(registration.parent_process.is_none());
    }

    #[test]
    fn merge_active_config_registrations_preserves_existing_timestamps_and_prunes_inactive_ones() {
        let mut registry = HashMap::from([
            (
                "issue-105".into(),
                RegisteredTmuxSession {
                    session: "issue-105".into(),
                    channel: Some("alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["error".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
            ),
            (
                "wrapper".into(),
                RegisteredTmuxSession {
                    session: "wrapper".into(),
                    channel: Some("alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T01:00:00Z".into(),
                    registration_source: RegistrationSource::CliWatch,
                    parent_process: Some(ParentProcessInfo {
                        pid: 42,
                        name: Some("codex".into()),
                    }),
                    registration_generation: 0,
                    active_wrapper_monitor: true,
                    lane: None,
                },
            ),
            (
                "stale-config".into(),
                RegisteredTmuxSession {
                    session: "stale-config".into(),
                    channel: Some("alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T02:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
            ),
        ]);

        merge_active_config_registrations(
            &mut registry,
            BTreeMap::from([(
                "issue-105".into(),
                RegisteredTmuxSession {
                    session: "issue-105".into(),
                    channel: Some("alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["error".into(), "complete".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T09:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
            )]),
        );

        assert_eq!(registry.len(), 2);
        assert_eq!(registry["issue-105"].registered_at, "2026-04-02T00:00:00Z");
        assert_eq!(registry["issue-105"].keywords, vec!["error", "complete"]);
        assert!(registry.contains_key("wrapper"));
        assert!(!registry.contains_key("stale-config"));
    }

    #[test]
    fn merge_active_config_registrations_skips_active_wrapper_monitor_sessions() {
        let mut registry = HashMap::from([(
            "issue-226".into(),
            RegisteredTmuxSession {
                session: "issue-226".into(),
                channel: Some("wrapper-alerts".into()),
                mention: None,
                routing: RoutingMetadata::default(),
                keywords: vec!["wrapper-keyword".into()],
                keyword_window_secs: 30,
                stale_minutes: 10,
                format: None,
                registered_at: "2026-04-02T01:00:00Z".into(),
                registration_source: RegistrationSource::CliNew,
                parent_process: Some(ParentProcessInfo {
                    pid: 42,
                    name: Some("codex".into()),
                }),
                registration_generation: 0,
                active_wrapper_monitor: true,
                lane: None,
            },
        )]);

        merge_active_config_registrations(
            &mut registry,
            BTreeMap::from([(
                "issue-226".into(),
                RegisteredTmuxSession {
                    session: "issue-226".into(),
                    channel: Some("config-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["config-keyword".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T09:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
            )]),
        );

        let registration = registry.get("issue-226").expect("wrapper registration");
        assert!(registration.active_wrapper_monitor);
        assert!(matches!(
            registration.registration_source,
            RegistrationSource::CliNew
        ));
        assert_eq!(registration.channel.as_deref(), Some("wrapper-alerts"));
        assert_eq!(registration.keywords, vec!["wrapper-keyword"]);
    }

    #[test]
    fn registered_tmux_session_deserializes_without_new_audit_fields() {
        let registration: RegisteredTmuxSession = serde_json::from_value(serde_json::json!({
            "session": "issue-24",
            "channel": "alerts",
            "mention": "<@123>",
            "keywords": ["panic"],
            "keyword_window_secs": 30,
            "stale_minutes": 10,
            "format": "compact",
            "active_wrapper_monitor": false
        }))
        .unwrap();

        assert!(matches!(
            registration.registration_source,
            RegistrationSource::ConfigMonitor
        ));
        assert!(registration.parent_process.is_none());
        assert!(!registration.registered_at.is_empty());
    }

    #[test]
    fn default_registry_state_path_sits_beside_cron_state() {
        let path = default_registry_state_path(Path::new("/tmp/clawhip/cron-state.json"));
        assert_eq!(path, PathBuf::from("/tmp/clawhip/tmux-watch-registry.json"));
    }

    #[tokio::test]
    async fn runtime_registry_persistence_filters_config_entries_and_normalizes_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmux-watch-registry.json");
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));

        let mut runtime = registration(vec!["panic"]);
        runtime.session = "runtime".into();
        runtime.registration_source = RegistrationSource::ConfigMonitor;
        register_runtime_tmux_registration(&registry, &path, runtime)
            .await
            .unwrap();

        let mut config = registration(vec!["warn"]);
        config.session = "config".into();
        config.registration_source = RegistrationSource::ConfigMonitor;
        registry.write().await.insert("config".into(), config);

        let snapshot = registry.read().await.clone();
        save_durable_tmux_registry(&path, &snapshot).await.unwrap();
        let loaded: BTreeMap<String, StoredTmuxRegistration> =
            serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded["runtime"].registration.registration_source,
            RegistrationSource::CliWatch
        );
        assert!(loaded["runtime"].lane.is_none());
        assert!(!loaded.contains_key("config"));
    }

    #[tokio::test]
    async fn concurrent_runtime_registrations_preserve_all_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmux-watch-registry.json");
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));

        let mut first = registration(vec!["panic"]);
        first.session = "runtime-a".into();
        first.registration_source = RegistrationSource::CliWatch;
        let mut second = registration(vec!["warn"]);
        second.session = "runtime-b".into();
        second.registration_source = RegistrationSource::CliNew;

        let first_register = register_runtime_tmux_registration(&registry, &path, first);
        let second_register = register_runtime_tmux_registration(&registry, &path, second);
        let (first_count, second_count) = tokio::join!(first_register, second_register);
        first_count.unwrap();
        second_count.unwrap();

        let snapshot = registry.read().await;
        assert!(snapshot.contains_key("runtime-a"));
        assert!(snapshot.contains_key("runtime-b"));
        drop(snapshot);

        let loaded: BTreeMap<String, StoredTmuxRegistration> =
            serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
        assert!(loaded.contains_key("runtime-a"));
        assert!(loaded.contains_key("runtime-b"));
        assert!(loaded["runtime-a"].lane.is_none());
        assert!(loaded["runtime-b"].lane.is_none());
    }

    #[tokio::test]
    async fn failed_register_save_leaves_registry_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("missing-parent")
            .join("tmux-watch-registry.json");
        tokio::fs::write(dir.path().join("missing-parent"), b"not a directory")
            .await
            .unwrap();
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        let mut runtime = registration(vec!["panic"]);
        runtime.session = "runtime".into();
        runtime.registration_source = RegistrationSource::CliWatch;

        let result = register_runtime_tmux_registration(&registry, &path, runtime).await;

        assert!(result.is_err());
        assert!(registry.read().await.is_empty());
    }

    #[tokio::test]
    async fn invalid_registry_state_is_ignored_fail_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmux-watch-registry.json");
        tokio::fs::write(&path, b"not json").await.unwrap();
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));

        let diagnostics = load_tmux_registry_state(&path, &registry).await;

        assert_eq!(diagnostics.status, TmuxRegistryStateStatus::IgnoredInvalid);
        assert_eq!(diagnostics.ignored, 1);
        assert!(registry.read().await.is_empty());
    }

    #[tokio::test]
    async fn flush_pending_keyword_hits_aggregates_unique_hits() {
        let (tx, mut rx) = mpsc::channel(1);
        let registration = RegisteredTmuxSession {
            format: Some(MessageFormat::Compact),
            mention: None,
            routing: RoutingMetadata::default(),
            ..registration(vec!["error", "complete"])
        };
        let start = Instant::now();
        let mut pending_keyword_hits = Some({
            let mut pending = PendingKeywordHits::new(start);
            pending.push(vec![
                KeywordHit {
                    keyword: "error".into(),
                    line: "error: failed".into(),
                    provenance: None,
                },
                KeywordHit {
                    keyword: "error".into(),
                    line: "error: failed".into(),
                    provenance: None,
                },
                KeywordHit {
                    keyword: "complete".into(),
                    line: "complete".into(),
                    provenance: None,
                },
            ]);
            pending
        });

        flush_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration,
            &tx,
            &registration.session,
            start + Duration::from_secs(30),
            Duration::from_secs(30),
            false,
        )
        .await
        .unwrap();

        assert!(pending_keyword_hits.is_none());
        let event = rx.recv().await.unwrap();
        assert_eq!(event.canonical_kind(), "tmux.keyword");
        assert_eq!(event.payload["hit_count"], 2);
    }

    #[tokio::test]
    async fn flush_pending_keyword_hits_clears_window_after_send_attempt() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let registration = RegisteredTmuxSession {
            format: Some(MessageFormat::Compact),
            mention: None,
            routing: RoutingMetadata::default(),
            ..registration(vec!["error", "complete"])
        };
        let start = Instant::now();
        let mut pending_keyword_hits = Some({
            let mut pending = PendingKeywordHits::new(start);
            pending.push(vec![KeywordHit {
                keyword: "error".into(),
                line: "boom".into(),
                provenance: None,
            }]);
            pending
        });

        let result = flush_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration,
            &tx,
            &registration.session,
            start + Duration::from_secs(30),
            Duration::from_secs(30),
            false,
        )
        .await;

        assert!(result.is_err());
        assert!(pending_keyword_hits.is_none());
    }

    #[tokio::test]
    async fn identical_keyword_lines_can_emit_again_after_window_flush() {
        let (tx, mut rx) = mpsc::channel(4);
        let registration = RegisteredTmuxSession {
            format: Some(MessageFormat::Compact),
            mention: None,
            routing: RoutingMetadata::default(),
            ..registration(vec!["error"])
        };
        let start = Instant::now();
        let mut snapshot = "done".to_string();
        let mut pending_keyword_hits = None;

        let first_snapshot = "done
error: failed";
        let first_hits = collect_keyword_hits(&snapshot, first_snapshot, &registration.keywords);
        push_pending_keyword_hits(&mut pending_keyword_hits, start, first_hits);
        snapshot = first_snapshot.into();

        flush_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration,
            &tx,
            &registration.session,
            start + Duration::from_secs(30),
            Duration::from_secs(30),
            false,
        )
        .await
        .unwrap();

        let first_event = rx.recv().await.unwrap();
        assert_eq!(first_event.payload["hit_count"], serde_json::Value::Null);
        assert_eq!(first_event.payload["keyword"], "error");
        assert_eq!(first_event.payload["line"], "error: failed");

        let second_snapshot = "done
error: failed
error: failed";
        let second_hits = collect_keyword_hits(&snapshot, second_snapshot, &registration.keywords);
        push_pending_keyword_hits(
            &mut pending_keyword_hits,
            start + Duration::from_secs(31),
            second_hits,
        );

        flush_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration,
            &tx,
            &registration.session,
            start + Duration::from_secs(61),
            Duration::from_secs(30),
            false,
        )
        .await
        .unwrap();

        let second_event = rx.recv().await.unwrap();
        assert_eq!(second_event.payload["hit_count"], serde_json::Value::Null);
        assert_eq!(second_event.payload["keyword"], "error");
        assert_eq!(second_event.payload["line"], "error: failed");
    }

    #[tokio::test]
    async fn session_keyword_hits_aggregate_across_panes_and_dedup_within_window() {
        let (tx, mut rx) = mpsc::channel(1);
        let registration = RegisteredTmuxSession {
            format: Some(MessageFormat::Compact),
            mention: None,
            routing: RoutingMetadata::default(),
            ..registration(vec!["error", "complete"])
        };
        let start = Instant::now();
        let mut pending_keyword_hits = HashMap::new();

        push_session_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration.session,
            start,
            vec![KeywordHit {
                keyword: "error".into(),
                line: "error: failed".into(),
                provenance: None,
            }],
        );
        push_session_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration.session,
            start + Duration::from_secs(5),
            vec![
                KeywordHit {
                    keyword: "error".into(),
                    line: "error: failed".into(),
                    provenance: None,
                },
                KeywordHit {
                    keyword: "complete".into(),
                    line: "build complete".into(),
                    provenance: None,
                },
            ],
        );

        flush_session_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration.session,
            &registration,
            &tx,
            start + Duration::from_secs(30),
            false,
        )
        .await
        .unwrap();

        assert!(pending_keyword_hits.is_empty());
        let event = rx.recv().await.unwrap();
        match from_incoming_event(&event).unwrap().body {
            EventBody::TmuxKeywordAggregated(body) => {
                assert_eq!(body.hit_count, 2);
                assert_eq!(body.hits.len(), 2);
            }
            other => panic!("expected aggregated tmux keyword body, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_keyword_hits_flush_when_window_expires() {
        let (tx, mut rx) = mpsc::channel(1);
        let registration = RegisteredTmuxSession {
            format: Some(MessageFormat::Compact),
            mention: None,
            routing: RoutingMetadata::default(),
            ..registration(vec!["error"])
        };
        let start = Instant::now();
        let mut pending_keyword_hits = HashMap::new();
        push_session_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration.session,
            start,
            vec![KeywordHit {
                keyword: "error".into(),
                line: "error: failed".into(),
                provenance: None,
            }],
        );

        flush_session_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration.session,
            &registration,
            &tx,
            start + Duration::from_secs(29),
            false,
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());
        assert!(pending_keyword_hits.contains_key(&registration.session));

        flush_session_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration.session,
            &registration,
            &tx,
            start + Duration::from_secs(30),
            false,
        )
        .await
        .unwrap();

        assert!(pending_keyword_hits.is_empty());
        let event = rx.recv().await.unwrap();
        assert_eq!(event.payload["keyword"], "error");
        assert_eq!(event.payload["line"], "error: failed");
    }

    #[test]
    fn resolve_monitored_sessions_expands_glob_patterns_to_actual_sessions() {
        let available_sessions = HashSet::from([
            "rcc-api".to_string(),
            "rcc-web".to_string(),
            "other".to_string(),
        ]);
        let resolved = resolve_monitored_sessions(
            vec![RegisteredTmuxSession {
                session: "rcc-*".into(),
                channel: Some("alerts".into()),
                mention: None,
                routing: RoutingMetadata::default(),
                keywords: vec!["panic".into()],
                keyword_window_secs: 30,
                stale_minutes: 10,
                format: None,
                registered_at: "2026-04-02T00:00:00Z".into(),
                registration_source: RegistrationSource::ConfigMonitor,
                parent_process: None,
                registration_generation: 0,
                active_wrapper_monitor: false,
                lane: None,
            }],
            Some(&available_sessions),
        );

        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved["rcc-api"].session, "rcc-api");
        assert_eq!(resolved["rcc-api"].channel.as_deref(), Some("alerts"));
        assert_eq!(resolved["rcc-api"].keywords, vec!["panic"]);
        assert_eq!(resolved["rcc-web"].session, "rcc-web");
        assert_eq!(resolved["rcc-web"].channel.as_deref(), Some("alerts"));
    }

    #[test]
    fn resolve_monitored_sessions_keeps_keywords_isolated_per_actual_session() {
        let available_sessions = HashSet::from(["rcc-prod".to_string(), "omx-prod".to_string()]);
        let resolved = resolve_monitored_sessions(
            vec![
                RegisteredTmuxSession {
                    session: "rcc-*".into(),
                    channel: Some("rcc-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
                RegisteredTmuxSession {
                    session: "omx-*".into(),
                    channel: Some("omx-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["error".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
            ],
            Some(&available_sessions),
        );

        assert_eq!(resolved["rcc-prod"].keywords, vec!["panic"]);
        assert_eq!(resolved["rcc-prod"].channel.as_deref(), Some("rcc-alerts"));
        assert_eq!(resolved["omx-prod"].keywords, vec!["error"]);
        assert_eq!(resolved["omx-prod"].channel.as_deref(), Some("omx-alerts"));
    }

    #[test]
    fn resolve_monitored_sessions_keeps_exact_sessions_when_listing_is_unavailable() {
        let resolved = resolve_monitored_sessions(
            vec![
                RegisteredTmuxSession {
                    session: "exact-session".into(),
                    channel: Some("alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
                RegisteredTmuxSession {
                    session: "rcc-*".into(),
                    channel: Some("alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
            ],
            None,
        );

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved["exact-session"].session, "exact-session");
    }

    #[test]
    fn resolve_monitored_sessions_prefers_exact_match_over_glob_overlap() {
        let available_sessions = HashSet::from(["rcc-api".to_string()]);
        let resolved = resolve_monitored_sessions(
            vec![
                RegisteredTmuxSession {
                    session: "*".into(),
                    channel: Some("default-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["error".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
                RegisteredTmuxSession {
                    session: "rcc-api".into(),
                    channel: Some("rcc-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
            ],
            Some(&available_sessions),
        );

        assert_eq!(resolved["rcc-api"].channel.as_deref(), Some("rcc-alerts"));
        assert_eq!(resolved["rcc-api"].keywords, vec!["panic"]);
    }

    #[test]
    fn resolve_monitored_sessions_prefers_more_specific_glob_over_broader_glob() {
        let available_sessions = HashSet::from(["rcc-api".to_string(), "omx-api".to_string()]);
        let resolved = resolve_monitored_sessions(
            vec![
                RegisteredTmuxSession {
                    session: "*".into(),
                    channel: Some("default-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["error".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
                RegisteredTmuxSession {
                    session: "rcc-*".into(),
                    channel: Some("rcc-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
            ],
            Some(&available_sessions),
        );

        assert_eq!(resolved["rcc-api"].channel.as_deref(), Some("rcc-alerts"));
        assert_eq!(resolved["rcc-api"].keywords, vec!["panic"]);
        assert_eq!(
            resolved["omx-api"].channel.as_deref(),
            Some("default-alerts")
        );
        assert_eq!(resolved["omx-api"].keywords, vec!["error"]);
    }

    #[test]
    fn resolve_monitored_sessions_breaks_same_literal_ties_with_fewer_wildcards() {
        let available_sessions = HashSet::from(["abc-prod".to_string()]);
        let resolved = resolve_monitored_sessions(
            vec![
                RegisteredTmuxSession {
                    session: "*abc*".into(),
                    channel: Some("broad-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["error".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
                RegisteredTmuxSession {
                    session: "abc*".into(),
                    channel: Some("specific-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    registration_generation: 0,
                    active_wrapper_monitor: false,
                    lane: None,
                },
            ],
            Some(&available_sessions),
        );

        assert_eq!(
            resolved["abc-prod"].channel.as_deref(),
            Some("specific-alerts")
        );
        assert_eq!(resolved["abc-prod"].keywords, vec!["panic"]);
    }

    #[test]
    fn stale_minutes_zero_disables_stale_detection() {
        let pane = TmuxPaneState {
            session: "test".into(),
            pane_name: "0.0".into(),
            snapshot: String::new(),
            content_hash: 0,
            last_change: Instant::now() - Duration::from_secs(3600),
            last_stale_notification: None,
            pane_dead: false,
        };
        // stale_minutes=0 should never emit, even after 1 hour idle
        assert!(!should_emit_stale(&pane, Instant::now(), 0));
    }

    #[test]
    fn stale_minutes_nonzero_still_emits() {
        let pane = TmuxPaneState {
            session: "test".into(),
            pane_name: "0.0".into(),
            snapshot: String::new(),
            content_hash: 0,
            last_change: Instant::now() - Duration::from_secs(3600),
            last_stale_notification: None,
            pane_dead: false,
        };
        // stale_minutes=1 should emit after 1 hour idle
        assert!(should_emit_stale(&pane, Instant::now(), 1));
    }

    #[test]
    fn pane_dead_suppresses_stale_alert() {
        let pane = TmuxPaneState {
            session: "test".into(),
            pane_name: "0.0".into(),
            snapshot: String::new(),
            content_hash: 0,
            last_change: Instant::now() - Duration::from_secs(3600),
            last_stale_notification: None,
            pane_dead: true,
        };
        // Dead pane should never emit stale, even after 1 hour idle
        assert!(!should_emit_stale(&pane, Instant::now(), 1));
    }

    #[tokio::test]
    async fn prune_absent_dynamic_removes_dead_cliwatch_no_lane() {
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        let path = std::env::temp_dir().join(format!(
            "clawhip-test-prune-cliwatch-{}.json",
            std::process::id()
        ));
        let _ = tokio::fs::remove_file(&path).await;
        let registration = RegisteredTmuxSession {
            session: "dead-watch".into(),
            channel: Some("alerts".into()),
            mention: None,
            routing: RoutingMetadata::default(),
            keywords: vec!["error".into()],
            keyword_window_secs: 30,
            stale_minutes: 10,
            format: None,
            registered_at: "2026-07-29T00:00:00Z".into(),
            registration_source: RegistrationSource::CliWatch,
            parent_process: None,
            registration_generation: 0,
            active_wrapper_monitor: true,

            lane: None,
        };
        registry
            .write()
            .await
            .insert("dead-watch".into(), registration);
        let candidates = vec![AbsentRegistrationCandidate {
            session: "dead-watch".into(),
            registration_generation: 0,
        }];
        let removed = prune_absent_dynamic_registrations(&registry, &path, &candidates)
            .await
            .unwrap();
        assert_eq!(removed, 1);
        assert!(registry.read().await.get("dead-watch").is_none());
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn prune_absent_dynamic_preserves_config_monitor() {
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        let path = std::env::temp_dir().join(format!(
            "clawhip-test-prune-config-{}.json",
            std::process::id()
        ));
        let _ = tokio::fs::remove_file(&path).await;
        let registration = RegisteredTmuxSession {
            session: "config-mon".into(),
            channel: Some("alerts".into()),
            mention: None,
            routing: RoutingMetadata::default(),
            keywords: vec!["error".into()],
            keyword_window_secs: 30,
            stale_minutes: 10,
            format: None,
            registered_at: "2026-07-29T00:00:00Z".into(),
            registration_source: RegistrationSource::ConfigMonitor,
            parent_process: None,
            registration_generation: 0,
            active_wrapper_monitor: false,
            lane: None,
        };
        registry
            .write()
            .await
            .insert("config-mon".into(), registration);
        let candidates = vec![AbsentRegistrationCandidate {
            session: "config-mon".into(),
            registration_generation: 0,
        }];
        let removed = prune_absent_dynamic_registrations(&registry, &path, &candidates)
            .await
            .unwrap();
        assert_eq!(removed, 0);
        assert!(registry.read().await.get("config-mon").is_some());
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn prune_absent_dynamic_skips_newer_registration() {
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        let path = std::env::temp_dir().join(format!(
            "clawhip-test-prune-race-{}.json",
            std::process::id()
        ));
        let _ = tokio::fs::remove_file(&path).await;
        // The current registration has generation 42 (re-registered).
        let registration = RegisteredTmuxSession {
            session: "re-reg".into(),
            channel: Some("alerts".into()),
            mention: None,
            routing: RoutingMetadata::default(),
            keywords: vec!["error".into()],
            keyword_window_secs: 30,
            stale_minutes: 10,
            format: None,
            registered_at: "2026-07-29T00:00:00Z".into(),
            registration_source: RegistrationSource::CliNew,
            parent_process: None,
            registration_generation: 42,
            active_wrapper_monitor: false,
            lane: None,
        };
        registry.write().await.insert("re-reg".into(), registration);
        // Stale candidate has generation 5 (old observation).
        let candidates = vec![AbsentRegistrationCandidate {
            session: "re-reg".into(),
            registration_generation: 5,
        }];
        let removed = prune_absent_dynamic_registrations(&registry, &path, &candidates)
            .await
            .unwrap();
        assert_eq!(removed, 0);
        assert!(registry.read().await.get("re-reg").is_some());
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn prune_absent_dynamic_preserves_lane_bearing_entry() {
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        let path = std::env::temp_dir().join(format!(
            "clawhip-test-prune-lane-{}.json",
            std::process::id()
        ));
        let _ = tokio::fs::remove_file(&path).await;
        let mut registration = RegisteredTmuxSession {
            session: "lane-session".into(),
            channel: Some("alerts".into()),
            mention: None,
            routing: RoutingMetadata::default(),
            keywords: vec!["error".into()],
            keyword_window_secs: 30,
            stale_minutes: 10,
            format: None,
            registered_at: "2026-07-29T00:00:00Z".into(),
            registration_source: RegistrationSource::CliNew,
            parent_process: None,
            registration_generation: 1,
            active_wrapper_monitor: false,
            lane: None,
        };
        registration.lane = Some(LaneEvidence {
            lane_version: 1,
            generation_id: "gen-abc123def".into(),
            kickoff_operation_id: "kick-abc123def".into(),
            launch_operation_id: "launch-abc123de".into(),
            executor_id: "exec-abc123def".into(),
            worker_effect_kind: WorkerEffectKind::CommandSubmission,
            launch_state: LaneLaunchState::Launched,
            workflow: LaneWorkflow::Active,
            revision: 2,
            quiesced: false,
            thread_id: None,
            kickoff_message_id: None,
            kickoff_delivered_at: None,
            visibility: Some(LaneVisibility::Visible),
            verification: None,
            last_failure: None,
            latest_update_message_id: None,
            latest_update_kind: None,
            latest_update_delivered_at: None,
            delivery_retry_count: 0,
            delivery_disposition: None,
        });
        registry
            .write()
            .await
            .insert("lane-session".into(), registration);
        let candidates = vec![AbsentRegistrationCandidate {
            session: "lane-session".into(),
            registration_generation: 1,
        }];
        let removed = prune_absent_dynamic_registrations(&registry, &path, &candidates)
            .await
            .unwrap();
        assert_eq!(removed, 0);
        assert!(registry.read().await.get("lane-session").is_some());
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn remove_tmux_registrations_requires_quiesced_retirement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        register_lane_registration(&registry, &path, lane_input("retained-lane", "g"))
            .await
            .unwrap();
        {
            let mut snapshot = registry.write().await;
            let lane = snapshot
                .get_mut("retained-lane")
                .and_then(|registration| registration.lane.as_mut())
                .unwrap();
            lane.workflow = LaneWorkflow::Retired;
            assert!(!lane.quiesced);
        }

        assert_eq!(
            remove_tmux_registrations(&registry, &path, &["retained-lane".into()])
                .await
                .unwrap(),
            0
        );
        assert!(registry.read().await.contains_key("retained-lane"));

        registry
            .write()
            .await
            .get_mut("retained-lane")
            .and_then(|registration| registration.lane.as_mut())
            .unwrap()
            .quiesced = true;
        assert_eq!(
            remove_tmux_registrations(&registry, &path, &["retained-lane".into()])
                .await
                .unwrap(),
            1
        );
        assert!(!registry.read().await.contains_key("retained-lane"));
    }

    #[tokio::test]
    async fn prune_absent_dynamic_removes_retired_quiesced_lane() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.json");
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        register_lane_registration(&registry, &path, lane_input("retired-lane", "g2"))
            .await
            .unwrap();
        let generation = {
            let mut snapshot = registry.write().await;
            let registration = snapshot.get_mut("retired-lane").unwrap();
            let generation = registration.registration_generation;
            let lane = registration.lane.as_mut().unwrap();
            lane.workflow = LaneWorkflow::Retired;
            lane.quiesced = true;
            generation
        };

        let removed = prune_absent_dynamic_registrations(
            &registry,
            &path,
            &[AbsentRegistrationCandidate {
                session: "retired-lane".into(),
                registration_generation: generation,
            }],
        )
        .await
        .unwrap();
        assert_eq!(removed, 1);
        assert!(!registry.read().await.contains_key("retired-lane"));
    }

    #[tokio::test]
    async fn prune_preserves_entries_on_durable_save_failure() {
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        // A path inside /proc or a directory should fail on write.
        let path = std::path::PathBuf::from("/dev/null/impossible-path.json");
        let registration = RegisteredTmuxSession {
            session: "fail-save".into(),
            channel: Some("alerts".into()),
            mention: None,
            routing: RoutingMetadata::default(),
            keywords: vec!["error".into()],
            keyword_window_secs: 30,
            stale_minutes: 10,
            format: None,
            registered_at: "2026-07-29T00:00:00Z".into(),
            registration_source: RegistrationSource::CliWatch,
            parent_process: None,
            registration_generation: 1,
            active_wrapper_monitor: false,
            lane: None,
        };
        registry
            .write()
            .await
            .insert("fail-save".into(), registration);
        let candidates = vec![AbsentRegistrationCandidate {
            session: "fail-save".into(),
            registration_generation: 1,
        }];
        let result = prune_absent_dynamic_registrations(&registry, &path, &candidates).await;
        assert!(result.is_err(), "prune should fail on durable save error");
        // Registry must remain unchanged: save-before-swap invariant.
        assert!(registry.read().await.get("fail-save").is_some());
    }

    #[tokio::test]
    async fn mint_registration_generation_is_monotonic() {
        let a = mint_registration_generation();
        let b = mint_registration_generation();
        let c = mint_registration_generation();
        assert!(b > a, "generation must be monotonically increasing");
        assert!(c > b, "generation must be monotonically increasing");
    }
}
