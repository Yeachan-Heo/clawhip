//! Typed GJC control-plane contract: requests, responses, error taxonomy,
//! and the narrow transport boundary owned by the #322 track.

use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Public contract schema version. Bumped only on breaking envelope change.
pub const GJC_CONTROL_SCHEMA: &str = "gjc-control/1";

// ---------------------------------------------------------------------------
// Identifiers
// ---------------------------------------------------------------------------

/// GJC session identifier. Opaque except for validation rules: non-empty,
/// bounded, no control characters. Never echo raw provider ids in errors.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SessionId(String);

impl SessionId {
    pub fn new(raw: impl Into<String>) -> std::result::Result<Self, GjcError> {
        let value = raw.into();
        let len = value.len();
        if len == 0 || len > 128 {
            return Err(GjcError::InvalidRequest {
                field: "session_id",
                reason: "must be 1..=128 bytes".into(),
            });
        }
        if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(GjcError::InvalidRequest {
                field: "session_id",
                reason: "control characters and whitespace are not allowed".into(),
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for SessionId {
    type Error = GjcError;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SessionId> for String {
    fn from(value: SessionId) -> Self {
        value.0
    }
}

/// Client-supplied idempotency key for mutation verbs. Identical keys are
/// replayed from the command registry instead of issuing a second command.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    pub fn new(raw: impl Into<String>) -> std::result::Result<Self, GjcError> {
        let value = raw.into();
        let len = value.len();
        if !(8..=128).contains(&len) {
            return Err(GjcError::InvalidRequest {
                field: "idempotency_key",
                reason: "must be 8..=128 bytes".into(),
            });
        }
        if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(GjcError::InvalidRequest {
                field: "idempotency_key",
                reason: "control characters and whitespace are not allowed".into(),
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for IdempotencyKey {
    type Error = GjcError;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<IdempotencyKey> for String {
    fn from(value: IdempotencyKey) -> Self {
        value.0
    }
}

/// Server-issued identifier of an accepted control command.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommandId(String);

impl CommandId {
    pub fn new(raw: impl Into<String>) -> std::result::Result<Self, GjcError> {
        let value = raw.into();
        if value.is_empty() || value.len() > 128 {
            return Err(GjcError::InvalidRequest {
                field: "command_id",
                reason: "must be 1..=128 bytes".into(),
            });
        }
        if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(GjcError::InvalidRequest {
                field: "command_id",
                reason: "control characters and whitespace are not allowed".into(),
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for CommandId {
    type Error = GjcError;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<CommandId> for String {
    fn from(value: CommandId) -> Self {
        value.0
    }
}

/// Server-issued identifier of a turn created by an accepted prompt.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TurnId(String);

impl TurnId {
    pub fn new(raw: impl Into<String>) -> std::result::Result<Self, GjcError> {
        let value = raw.into();
        if value.is_empty() || value.len() > 128 {
            return Err(GjcError::InvalidRequest {
                field: "turn_id",
                reason: "must be 1..=128 bytes".into(),
            });
        }
        if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err(GjcError::InvalidRequest {
                field: "turn_id",
                reason: "control characters and whitespace are not allowed".into(),
            });
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for TurnId {
    type Error = GjcError;
    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<TurnId> for String {
    fn from(value: TurnId) -> Self {
        value.0
    }
}

// ---------------------------------------------------------------------------
// Transport boundary (owned by #322)
// ---------------------------------------------------------------------------

/// A single correlated transport exchange. The control plane never opens
/// sockets itself; it issues one request and requires one unambiguously
/// correlated reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GjcRequest {
    /// Correlation id echoed verbatim by the peer.
    pub correlation_id: String,
    /// Dotted method name, e.g. `session.get`.
    pub method: String,
    /// Method parameters (public-safe; no tokens ever cross this boundary).
    pub params: Value,
    /// Bounded exchange budget in milliseconds; the transport must clamp
    /// its request timeout to at most this value.
    pub timeout_ms: u64,
}

impl GjcRequest {
    pub fn new(
        correlation_id: impl Into<String>,
        method: &str,
        params: Value,
        timeout_ms: u64,
    ) -> Self {
        Self {
            correlation_id: correlation_id.into(),
            method: method.into(),
            params,
            timeout_ms,
        }
    }
}

/// Peer reply. `correlation_id` must match the request exactly or the
/// exchange fails closed as an ambiguous ack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GjcResponse {
    pub correlation_id: String,
    pub result: Value,
}

/// Narrow transport boundary implemented by the #322 transport track.
///
/// Implementations MUST:
/// - send exactly one request and resolve with at most one reply;
/// - return [`GjcError::Timeout`] instead of an unbounded wait;
/// - never surface endpoint metadata or tokens in errors.
#[async_trait]
pub trait GjcTransport: Send + Sync {
    async fn round_trip(&self, request: GjcRequest) -> std::result::Result<GjcResponse, GjcError>;

    fn endpoint_generation(&self) -> Option<u64> {
        None
    }
}

// ---------------------------------------------------------------------------
// Error taxonomy
// ---------------------------------------------------------------------------

/// Fail-closed error taxonomy for the GJC control plane. Every variant maps
/// to a stable machine-readable `error_code` plus an HTTP status; none carry
/// secrets, tokens, or endpoint internals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GjcError {
    /// The #322 transport layer has no usable implementation/endpoint.
    TransportUnavailable,
    /// Transport round trip exceeded its bounded timeout.
    Timeout { method: String },
    /// Peer reply correlation did not match the request.
    AmbiguousAck { method: String },
    /// Peer returned a malformed, oversized, or unexpected envelope.
    InvalidPeerReply { method: String, reason: String },
    /// The requested capability is not present on this build/daemon.
    MissingCapability { capability: String },
    /// The peer is reachable but its endpoint metadata is stale.
    StaleEndpoint { capability: String },
    /// Expected session id did not match the authoritative session.
    SessionMismatch { expected: String },
    /// The referenced session does not exist (or is not visible here).
    SessionNotFound { session_id: String },
    /// Request failed local validation before any transport call.
    InvalidRequest { field: &'static str, reason: String },
}

impl GjcError {
    pub fn error_code(&self) -> &'static str {
        match self {
            GjcError::TransportUnavailable => "transport_unavailable",
            GjcError::Timeout { .. } => "timeout",
            GjcError::AmbiguousAck { .. } => "ambiguous_ack",
            GjcError::InvalidPeerReply { .. } => "invalid_peer_reply",
            GjcError::MissingCapability { .. } => "missing_capability",
            GjcError::StaleEndpoint { .. } => "stale_endpoint",
            GjcError::SessionMismatch { .. } => "session_mismatch",
            GjcError::SessionNotFound { .. } => "session_not_found",
            GjcError::InvalidRequest { .. } => "invalid_request",
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            GjcError::TransportUnavailable => 503,
            GjcError::Timeout { .. } => 504,
            GjcError::AmbiguousAck { .. } | GjcError::InvalidPeerReply { .. } => 502,
            GjcError::MissingCapability { .. } => 501,
            GjcError::StaleEndpoint { .. } => 409,
            GjcError::SessionMismatch { .. } => 409,
            GjcError::SessionNotFound { .. } => 404,
            GjcError::InvalidRequest { .. } => 400,
        }
    }

    /// Public-safe JSON body: stable code + message, never internals.
    pub fn public_body(&self) -> Value {
        let mut body = serde_json::json!({
            "schema": GJC_CONTROL_SCHEMA,
            "ok": false,
            "error_code": self.error_code(),
            "error": self.public_message(),
        });
        match self {
            GjcError::MissingCapability { capability } | GjcError::StaleEndpoint { capability } => {
                body["capability"] = Value::String(capability.clone());
            }
            GjcError::SessionMismatch { expected } => {
                body["expected_session_id"] = Value::String(expected.clone());
            }
            _ => {}
        }
        body
    }

    pub fn public_message(&self) -> String {
        match self {
            GjcError::TransportUnavailable => {
                "gjc transport is not available on this daemon".into()
            }
            GjcError::Timeout { method } => {
                format!("gjc request timed out: {method}")
            }
            GjcError::AmbiguousAck { method } => {
                format!("gjc reply correlation failed for {method}")
            }
            GjcError::InvalidPeerReply { method, reason } => {
                let _ = reason;
                format!("gjc reply rejected for {method}")
            }
            GjcError::MissingCapability { capability } => {
                format!("required gjc capability is missing: {capability}")
            }
            GjcError::StaleEndpoint { capability } => {
                format!("gjc endpoint metadata is stale for capability: {capability}")
            }
            GjcError::SessionMismatch { .. } => {
                "gjc session id did not match the expected session".into()
            }
            GjcError::SessionNotFound { .. } => "gjc session not found".into(),
            GjcError::InvalidRequest { field, reason } => {
                format!("invalid gjc request field `{field}`: {reason}")
            }
        }
    }
}

impl fmt::Display for GjcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.error_code(), self.public_message())
    }
}

impl std::error::Error for GjcError {}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// Capability keys advertised by the daemon and required by verbs.
pub const CAP_SESSION_QUERY: &str = "session.query";
pub const CAP_SESSION_CONTROL: &str = "session.control";
pub const CAP_MODEL_SELECTION: &str = "session.model_selection";
pub const CAP_WORKFLOW_GATES: &str = "workflow.gates";
pub const CAP_ASK_ANSWERS: &str = "ask.answers";
/// Pseudo-capability used by `stale_endpoint` when endpoint metadata
/// itself is no longer trustworthy.
pub const CAP_ENDPOINT: &str = "endpoint";

/// Capabilities this control plane can exercise given a transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub schema: String,
    pub transport_implemented: bool,
    pub capabilities: Vec<String>,
}

impl Capabilities {
    pub fn for_transport(implemented: bool) -> Self {
        let capabilities = if implemented {
            vec![
                CAP_SESSION_QUERY.to_string(),
                CAP_SESSION_CONTROL.to_string(),
                CAP_MODEL_SELECTION.to_string(),
                CAP_WORKFLOW_GATES.to_string(),
                CAP_ASK_ANSWERS.to_string(),
            ]
        } else {
            // Fail closed: with no transport, no capability is exercisable.
            Vec::new()
        };
        Self {
            schema: GJC_CONTROL_SCHEMA.into(),
            transport_implemented: implemented,
            capabilities,
        }
    }

    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|c| c == capability)
    }

    /// Fail closed when a required capability is absent.
    pub fn require(&self, capability: &str) -> std::result::Result<(), GjcError> {
        if self.supports(capability) {
            Ok(())
        } else {
            Err(GjcError::MissingCapability {
                capability: capability.into(),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Query responses
// ---------------------------------------------------------------------------

/// Authoritative session metadata. Peer replies may omit optional fields;
/// only `session_id` is mandatory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub session_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub last_active_at: Option<String>,
    #[serde(default)]
    pub lane: Option<String>,
    /// Free-form provider metadata that has passed allow-list filtering.
    #[serde(default)]
    pub provider_metadata: BTreeMap<String, Value>,
}

/// Authoritative session statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionStats {
    pub turns_total: u64,
    pub turns_failed: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub queue_depth: u64,
    pub last_turn_started_at: Option<String>,
    pub last_turn_finished_at: Option<String>,
}

/// Currently selected model and profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionModelProfile {
    pub model: String,
    pub profile: Option<String>,
    pub updated_at: Option<String>,
}

/// A queued item ahead of or behind the active turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntry {
    pub position: u64,
    pub kind: String,
    pub summary: Option<String>,
    pub enqueued_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueSnapshot {
    #[serde(default)]
    pub depth: u64,
    pub entries: Vec<QueueEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GjcPromptStatus {
    /// Accepted, not yet dispatched to the model.
    Queued,
    /// Actively executing.
    Running,
    /// Terminal success.
    Succeeded,
    /// Terminal failure.
    Failed,
    /// Superseded by abort-and-prompt.
    Aborted,
}

impl GjcPromptStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, GjcPromptStatus::Succeeded | GjcPromptStatus::Failed)
    }
}

/// A workflow gate awaiting an answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowGate {
    pub gate_id: String,
    #[serde(default)]
    pub kind: Option<String>,
    pub workflow_id: Option<String>,
    pub state: WorkflowGateState,
    pub title: Option<String>,
    pub options: Vec<String>,
    pub raised_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowGateState {
    Ready,
    Answered,
    Cancelled,
}

/// Goal/todo state attached to a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GoalTodoSnapshot {
    #[serde(default)]
    pub goal: Option<Value>,
    pub todos: Vec<Value>,
}

/// Authoritative turn state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTurn {
    pub turn_id: String,
    pub status: GjcPromptStatus,
    #[serde(default)]
    pub prompt_accepted: bool,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// Terminal outcome payload; present only on terminal statuses.
    pub outcome: Option<SessionTurnOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTurnOutcome {
    pub status: GjcPromptStatus,
    pub summary: Option<String>,
    pub finished_at: Option<String>,
}

/// Union query result over the authoritative session surfaces. Each section
/// is requested by name; missing surfaces stay `None` instead of guessed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionQuery {
    #[serde(default)]
    pub revision: Option<u64>,
    pub metadata: Option<SessionMetadata>,
    pub stats: Option<SessionStats>,
    pub model_profile: Option<SessionModelProfile>,
    pub turn: Option<SessionTurn>,
    #[serde(skip)]
    pub turn_present: bool,
    pub queue: Option<QueueSnapshot>,
    pub workflow_gates: Option<Vec<WorkflowGate>>,
    #[serde(skip)]
    pub workflow_gates_present: bool,
    pub goal_todo: Option<GoalTodoSnapshot>,
}

// ---------------------------------------------------------------------------
// Mutation requests and receipts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GjcCommandKind {
    Prompt,
    Steer,
    AbortAndPrompt,
    WorkflowGateAnswer,
    AskAnswer,
    ModelSelection,
}

impl GjcCommandKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            GjcCommandKind::Prompt => "prompt",
            GjcCommandKind::Steer => "steer",
            GjcCommandKind::AbortAndPrompt => "abort_and_prompt",
            GjcCommandKind::WorkflowGateAnswer => "workflow_gate_answer",
            GjcCommandKind::AskAnswer => "ask_answer",
            GjcCommandKind::ModelSelection => "model_selection",
        }
    }

    pub fn required_capability(&self) -> &'static str {
        match self {
            GjcCommandKind::Prompt | GjcCommandKind::Steer | GjcCommandKind::AbortAndPrompt => {
                CAP_SESSION_CONTROL
            }
            GjcCommandKind::WorkflowGateAnswer => CAP_WORKFLOW_GATES,
            GjcCommandKind::AskAnswer => CAP_ASK_ANSWERS,
            GjcCommandKind::ModelSelection => CAP_MODEL_SELECTION,
        }
    }
}

/// Lifecycle of an accepted command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GjcCommandStatus {
    /// Validated, capability-checked, accepted; awaiting peer ack.
    Accepted,
    /// Peer acked; terminal receipt not yet available.
    Acked,
    /// Terminal outcome recorded.
    Completed,
    /// Terminal failure recorded.
    Failed,
}

impl GjcCommandStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, GjcCommandStatus::Completed | GjcCommandStatus::Failed)
    }

    pub fn can_transition_to(&self, next: GjcCommandStatus) -> bool {
        use GjcCommandStatus::*;
        matches!(
            (self, next),
            (Accepted, Acked)
                | (Accepted, Completed)
                | (Accepted, Failed)
                | (Acked, Acked)
                | (Acked, Completed)
                | (Acked, Failed)
        )
    }
}

/// Common envelope for every mutation verb.
#[derive(Debug, Clone)]
pub struct ControlRequestEnvelope {
    pub session: SessionId,
    /// When set, the command is rejected unless the authoritative session id
    /// matches exactly (fail closed on session mismatch).
    pub expected_session: Option<SessionId>,
    pub idempotency_key: IdempotencyKey,
    /// Bounded timeout for the peer exchange.
    pub timeout_ms: u64,
}

impl ControlRequestEnvelope {
    pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
    pub const MAX_TIMEOUT_MS: u64 = 60_000;

    pub fn validate(&self) -> std::result::Result<(), GjcError> {
        if self.timeout_ms == 0 || self.timeout_ms > Self::MAX_TIMEOUT_MS {
            return Err(GjcError::InvalidRequest {
                field: "timeout_ms",
                reason: format!("must be 1..={}", Self::MAX_TIMEOUT_MS),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct PromptRequest {
    pub envelope: ControlRequestEnvelope,
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub struct SteerRequest {
    pub envelope: ControlRequestEnvelope,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct AbortAndPromptRequest {
    pub envelope: ControlRequestEnvelope,
    /// Only abort the turns listed here; empty aborts all in-flight turns.
    pub turn_ids: Vec<TurnId>,
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub struct WorkflowGateAnswerRequest {
    pub envelope: ControlRequestEnvelope,
    pub gate_id: String,
    pub answer: WorkflowGateAnswer,
}

#[derive(Debug, Clone)]
pub struct WorkflowGateAnswer {
    pub option: String,
}

#[derive(Debug, Clone)]
pub struct AskAnswerRequest {
    pub envelope: ControlRequestEnvelope,
    pub ask_id: String,
    pub choices: Vec<AskChoice>,
}

#[derive(Debug, Clone)]
pub struct AskChoice {
    pub option: String,
}

#[derive(Debug, Clone)]
pub struct ModelSelectionRequest {
    pub envelope: ControlRequestEnvelope,
    pub model: Option<String>,
    pub profile: Option<String>,
}

/// Receipt for an accepted command. Replays of the same idempotency key
/// return the original receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandReceipt {
    pub schema: String,
    pub command_id: String,
    pub idempotency_key: String,
    pub kind: String,
    pub session_id: String,
    pub status: GjcCommandStatus,
    /// Present when the verb created a turn.
    pub turn_id: Option<String>,
    /// Terminal outcome; present only on terminal statuses.
    pub outcome: Option<Value>,
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Result alias local to the control plane
// ---------------------------------------------------------------------------

pub type GjcResult<T> = std::result::Result<T, GjcError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_rejects_empty_and_control_characters() {
        assert!(SessionId::new("").is_err());
        assert!(SessionId::new("a\nb").is_err());
        assert!(SessionId::new("a b").is_err());
        assert_eq!(SessionId::new("sess-1").unwrap().as_str(), "sess-1");
    }

    #[test]
    fn idempotency_key_enforces_minimum_length() {
        assert!(IdempotencyKey::new("short").is_err());
        assert!(IdempotencyKey::new("12345678").is_ok());
    }

    #[test]
    fn error_taxonomy_maps_codes_and_statuses() {
        assert_eq!(
            GjcError::TransportUnavailable.error_code(),
            "transport_unavailable"
        );
        assert_eq!(GjcError::TransportUnavailable.http_status(), 503);
        assert_eq!(GjcError::Timeout { method: "x".into() }.http_status(), 504);
        assert_eq!(
            GjcError::SessionMismatch {
                expected: "s".into()
            }
            .http_status(),
            409
        );
    }

    #[test]
    fn public_error_body_is_redacted() {
        let body = GjcError::StaleEndpoint {
            capability: "session.query".into(),
        }
        .public_body();
        assert_eq!(body["error_code"], "stale_endpoint");
        assert_eq!(body["capability"], "session.query");
        assert!(body.get("endpoint").is_none());
    }

    #[test]
    fn capabilities_fail_closed_without_transport() {
        let caps = Capabilities::for_transport(false);
        assert!(!caps.supports(CAP_SESSION_QUERY));
        assert!(!caps.supports(CAP_SESSION_CONTROL));
        assert!(caps.require(CAP_SESSION_CONTROL).is_err());
    }

    #[test]
    fn prompt_status_terminality_is_explicit() {
        use GjcPromptStatus::*;
        assert!(Succeeded.is_terminal());
        assert!(Failed.is_terminal());
        assert!(!Queued.is_terminal());
        assert!(!Running.is_terminal());
        assert!(!Aborted.is_terminal());
    }

    #[test]
    fn command_status_progression_is_forward_only() {
        use GjcCommandStatus::*;
        assert!(Accepted.can_transition_to(Acked));
        assert!(Acked.can_transition_to(Completed));
        assert!(!Completed.can_transition_to(Acked));
        assert!(!Failed.can_transition_to(Acked));
    }
}
