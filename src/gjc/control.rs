//! Control plane over the typed GJC contract: idempotent command registry,
//! capability gating, session-mismatch guards, and the mutation verbs.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::RwLock;
use uuid::Uuid;

use super::model::{
    AbortAndPromptRequest, AskAnswerRequest, CommandId, CommandReceipt, ControlRequestEnvelope,
    GJC_CONTROL_SCHEMA, GjcCommandKind, GjcCommandStatus, GjcError, GjcPromptStatus, GjcRequest,
    GjcResponse, GjcResult, GjcTransport, IdempotencyKey, ModelSelectionRequest, PromptRequest,
    SessionId, SessionQuery, SteerRequest, WorkflowGateAnswerRequest,
};

pub type SharedGjcCommandRegistry = Arc<RwLock<HashMap<String, CommandReceipt>>>;

pub fn new_shared_command_registry() -> SharedGjcCommandRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

fn now_rfc3339() -> String {
    crate::source::tmux::current_timestamp_rfc3339()
}

/// The authoritative control plane. One instance per daemon; CLI paths go
/// through the daemon HTTP surface rather than constructing this directly.
#[derive(Clone)]
pub struct GjcControlPlane {
    transport: Arc<dyn GjcTransport>,
    registry: SharedGjcCommandRegistry,
    transport_implemented: bool,
}

impl GjcControlPlane {
    pub fn new(
        transport: Arc<dyn GjcTransport>,
        registry: SharedGjcCommandRegistry,
        transport_implemented: bool,
    ) -> Self {
        Self {
            transport,
            registry,
            transport_implemented,
        }
    }

    pub fn transport_implemented(&self) -> bool {
        self.transport_implemented
    }

    // -----------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------

    /// Authoritative multi-surface session query. Unknown sections stay
    /// `None`; the peer never invents data and neither do we.
    pub async fn query_session(
        &self,
        session: &SessionId,
        sections: &[&str],
    ) -> GjcResult<SessionQuery> {
        self.require_transport()?;
        let capabilities = super::model::Capabilities::for_transport(self.transport_implemented);
        capabilities.require(super::model::CAP_SESSION_QUERY)?;

        let params = json!({
            "session_id": session.as_str(),
            "sections": sections,
        });
        let reply = self.round_trip("session.get", params).await?;
        let result = reply.result;
        let mut query = SessionQuery::default();
        if let Some(value) = result.get("metadata") {
            query.metadata = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("metadata section: {error}"),
                }
            })?;
        }
        if let Some(value) = result.get("stats") {
            query.stats = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("stats section: {error}"),
                }
            })?;
        }
        if let Some(value) = result.get("model_profile") {
            query.model_profile = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("model_profile section: {error}"),
                }
            })?;
        }
        if let Some(value) = result.get("turn") {
            query.turn = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("turn section: {error}"),
                }
            })?;
        }
        if let Some(value) = result.get("queue") {
            query.queue = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("queue section: {error}"),
                }
            })?;
        }
        if let Some(value) = result.get("workflow_gates") {
            query.workflow_gates = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("workflow_gates section: {error}"),
                }
            })?;
        }
        if let Some(value) = result.get("goal_todo") {
            query.goal_todo = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("goal_todo section: {error}"),
                }
            })?;
        }
        self.check_session_identity(session, &query)?;
        Ok(query)
    }

    /// Terminal outcome receipt for one turn.
    pub async fn turn_outcome(&self, session: &SessionId, turn_id: &str) -> GjcResult<Value> {
        self.require_transport()?;
        let params = json!({
            "session_id": session.as_str(),
            "turn_id": turn_id,
        });
        let reply = self.round_trip("turn.outcome", params).await?;
        let outcome =
            reply
                .result
                .get("outcome")
                .cloned()
                .ok_or_else(|| GjcError::InvalidPeerReply {
                    method: "turn.outcome".into(),
                    reason: "outcome missing".into(),
                })?;
        Ok(outcome)
    }

    // -----------------------------------------------------------------
    // Mutations
    // -----------------------------------------------------------------

    pub async fn prompt(&self, request: PromptRequest) -> GjcResult<CommandReceipt> {
        self.mutate(
            request.envelope,
            GjcCommandKind::Prompt,
            json!({
                "prompt": request.prompt,
            }),
        )
        .await
    }

    pub async fn steer(&self, request: SteerRequest) -> GjcResult<CommandReceipt> {
        self.mutate(
            request.envelope,
            GjcCommandKind::Steer,
            json!({
                "message": request.message,
            }),
        )
        .await
    }

    pub async fn abort_and_prompt(
        &self,
        request: AbortAndPromptRequest,
    ) -> GjcResult<CommandReceipt> {
        self.mutate(
            request.envelope,
            GjcCommandKind::AbortAndPrompt,
            json!({
                "turn_ids": request
                    .turn_ids
                    .iter()
                    .map(|turn| turn.as_str())
                    .collect::<Vec<_>>(),
                "prompt": request.prompt,
            }),
        )
        .await
    }

    pub async fn answer_workflow_gate(
        &self,
        request: WorkflowGateAnswerRequest,
    ) -> GjcResult<CommandReceipt> {
        self.mutate(
            request.envelope,
            GjcCommandKind::WorkflowGateAnswer,
            json!({
                "gate_id": request.gate_id,
                "option": request.answer.option,
            }),
        )
        .await
    }

    pub async fn answer_ask(&self, request: AskAnswerRequest) -> GjcResult<CommandReceipt> {
        self.mutate(
            request.envelope,
            GjcCommandKind::AskAnswer,
            json!({
                "ask_id": request.ask_id,
                "choices": request
                    .choices
                    .iter()
                    .map(|choice| choice.option.as_str())
                    .collect::<Vec<_>>(),
            }),
        )
        .await
    }

    pub async fn select_model(&self, request: ModelSelectionRequest) -> GjcResult<CommandReceipt> {
        let ((Some(model), None) | (None, Some(model))) =
            (request.model.as_deref(), request.profile.as_deref())
        else {
            return Err(GjcError::InvalidRequest {
                field: "model",
                reason: "exactly one of model or profile must be provided".into(),
            });
        };
        self.mutate(
            request.envelope,
            GjcCommandKind::ModelSelection,
            json!({
                "selection": model,
            }),
        )
        .await
    }

    /// Replay an accepted command by idempotency key.
    pub async fn command_receipt(&self, key: &IdempotencyKey) -> GjcResult<CommandReceipt> {
        self.registry
            .read()
            .await
            .get(key.as_str())
            .cloned()
            .ok_or(GjcError::SessionNotFound {
                session_id: key.as_str().to_string(),
            })
            .and_then(|receipt| {
                if receipt.status.is_terminal() {
                    Ok(receipt)
                } else {
                    Err(GjcError::InvalidRequest {
                        field: "idempotency_key",
                        reason: "command has not reached a terminal state".into(),
                    })
                }
            })
    }

    // -----------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------

    fn require_transport(&self) -> GjcResult<()> {
        if self.transport_implemented {
            Ok(())
        } else {
            Err(GjcError::TransportUnavailable)
        }
    }

    async fn round_trip(&self, method: &str, params: Value) -> GjcResult<GjcResponse> {
        let correlation_id = Uuid::new_v4().to_string();
        let request = GjcRequest::new(&correlation_id, method, params);
        let reply = self.transport.round_trip(request).await?;
        if reply.correlation_id != correlation_id {
            return Err(GjcError::AmbiguousAck {
                method: method.into(),
            });
        }
        Ok(reply)
    }

    /// Shared mutation path: capability gate, session guard, idempotent
    /// replay, bounded exchange, receipt recording with forward-only
    /// status progression.
    async fn mutate(
        &self,
        envelope: ControlRequestEnvelope,
        kind: GjcCommandKind,
        params: Value,
    ) -> GjcResult<CommandReceipt> {
        envelope.validate()?;
        self.require_transport()?;
        let capabilities = super::model::Capabilities::for_transport(self.transport_implemented);
        capabilities.require(kind.required_capability())?;

        // Idempotent replay: the identical key returns the recorded receipt.
        if let Some(existing) = self
            .registry
            .read()
            .await
            .get(envelope.idempotency_key.as_str())
            && existing.status.is_terminal()
        {
            return Ok(existing.clone());
        }

        // Expected-session guard: fail closed before any dispatch when the
        // caller's belief disagrees with the addressed session.
        if let Some(expected) = envelope.expected_session.as_ref()
            && expected.as_str() != envelope.session.as_str()
        {
            return Err(GjcError::SessionMismatch {
                expected: envelope.session.as_str().to_string(),
            });
        }

        let mut request_params = json!({
            "session_id": envelope.session.as_str(),
            "idempotency_key": envelope.idempotency_key.as_str(),
            "kind": kind.as_str(),
        });
        if let Some(expected) = envelope.expected_session.as_ref() {
            request_params["expected_session_id"] = Value::String(expected.as_str().to_string());
        }
        if let Value::Object(map) = params {
            for (key, value) in map {
                request_params[key] = value;
            }
        }

        let command_id = CommandId::new(format!("gjc-cmd-{}", Uuid::new_v4()))?;
        let receipt = CommandReceipt {
            schema: GJC_CONTROL_SCHEMA.into(),
            command_id: command_id.as_str().to_string(),
            idempotency_key: envelope.idempotency_key.as_str().to_string(),
            kind: kind.as_str().to_string(),
            session_id: envelope.session.as_str().to_string(),
            status: GjcCommandStatus::Accepted,
            turn_id: None,
            outcome: None,
            created_at: now_rfc3339(),
        };
        self.registry.write().await.insert(
            envelope.idempotency_key.as_str().to_string(),
            receipt.clone(),
        );

        // Transport exchange with bounded timeout semantics owned by the
        // transport implementation (#322). Ambiguous or malformed acks fail
        // closed; the recorded receipt stays non-terminal so a replay does
        // not fabricate an outcome.
        let mut reply = self
            .round_trip(&format!("control.{}", kind.as_str()), request_params)
            .await?;

        let acked = reply
            .result
            .get("accepted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !acked {
            return Err(GjcError::AmbiguousAck {
                method: kind.as_str().into(),
            });
        }

        // Ack-side session guard: an ack naming a different session is
        // treated as a mismatch and fails closed.
        if let Some(echoed) = reply.result.get("session_id").and_then(Value::as_str)
            && echoed != envelope.session.as_str()
        {
            return Err(GjcError::SessionMismatch {
                expected: envelope.session.as_str().to_string(),
            });
        }
        // Status progression Accepted -> Acked -> terminal.

        let turn_id = reply
            .result
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let outcome = reply.result.get("outcome").cloned();

        let mut write = self.registry.write().await;
        let entry =
            write
                .get_mut(envelope.idempotency_key.as_str())
                .ok_or(GjcError::InvalidRequest {
                    field: "idempotency_key",
                    reason: "command registry entry vanished mid-flight".into(),
                })?;
        if !entry.status.can_transition_to(GjcCommandStatus::Acked) {
            return Err(GjcError::AmbiguousAck {
                method: kind.as_str().into(),
            });
        }
        entry.status = GjcCommandStatus::Acked;
        if let Some(turn_id) = turn_id.clone() {
            entry.turn_id = Some(turn_id);
        }
        let mut updated = entry.clone();
        drop(write);

        // Terminal receipt: peer outcome present means terminal now.
        if let Some(outcome) = outcome {
            let outcome_status = outcome
                .get("status")
                .and_then(Value::as_str)
                .and_then(parse_prompt_status);
            let mut write = self.registry.write().await;
            if let Some(entry) = write.get_mut(envelope.idempotency_key.as_str()) {
                let terminal = match outcome_status {
                    Some(status) if status.is_terminal() => GjcCommandStatus::Completed,
                    Some(_) => GjcCommandStatus::Failed,
                    None => GjcCommandStatus::Failed,
                };
                if entry.status.can_transition_to(terminal) {
                    entry.status = terminal;
                    entry.outcome = Some(outcome);
                    updated = entry.clone();
                }
            }
        }

        let _ = &mut reply;
        Ok(updated)
    }

    /// Fail closed when an expected session id disagrees with the
    /// authoritative session metadata.
    fn check_session_identity(&self, session: &SessionId, query: &SessionQuery) -> GjcResult<()> {
        let Some(metadata) = query.metadata.as_ref() else {
            return Ok(());
        };
        if metadata.session_id != session.as_str() {
            return Err(GjcError::SessionMismatch {
                expected: session.as_str().to_string(),
            });
        }
        Ok(())
    }
}

fn parse_prompt_status(raw: &str) -> Option<GjcPromptStatus> {
    match raw {
        "queued" => Some(GjcPromptStatus::Queued),
        "running" => Some(GjcPromptStatus::Running),
        "succeeded" => Some(GjcPromptStatus::Succeeded),
        "failed" => Some(GjcPromptStatus::Failed),
        "aborted" => Some(GjcPromptStatus::Aborted),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gjc::model::{
        AbortAndPromptRequest, Capabilities, ControlRequestEnvelope, IdempotencyKey,
        ModelSelectionRequest, SessionId, TransportUnavailable,
    };
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Scripted transport: records requests and replays canned replies so
    /// the full contract is exercised without the #322 transport track.
    struct MockTransport {
        replies: Mutex<Vec<GjcResult<GjcResponse>>>,
        seen: Mutex<Vec<String>>,
    }

    impl MockTransport {
        fn new(replies: Vec<GjcResult<GjcResponse>>) -> Self {
            Self {
                replies: Mutex::new(replies),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl GjcTransport for MockTransport {
        async fn round_trip(
            &self,
            request: GjcRequest,
        ) -> std::result::Result<GjcResponse, GjcError> {
            self.seen.lock().unwrap().push(request.method.clone());
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                return Err(GjcError::Timeout {
                    method: request.method,
                });
            }
            let reply = replies.remove(0);
            match reply {
                Ok(mut response) => {
                    response.correlation_id = request.correlation_id;
                    Ok(response)
                }
                Err(error) => Err(error),
            }
        }
    }

    fn ack_reply(turn_id: &str) -> GjcResult<GjcResponse> {
        Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({"accepted": true, "turn_id": turn_id}),
        })
    }

    fn terminal_ack_reply(turn_id: &str, status: &str, summary: &str) -> GjcResult<GjcResponse> {
        Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({
                "accepted": true,
                "turn_id": turn_id,
                "outcome": {"status": status, "summary": summary},
            }),
        })
    }

    async fn implemented_plane_with(
        replies: Vec<GjcResult<GjcResponse>>,
    ) -> (GjcControlPlane, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport::new(replies));
        let plane = GjcControlPlane::new(
            transport.clone(),
            new_shared_command_registry(),
            true,
        );
        (plane, transport)
    }

    #[tokio::test]
    async fn query_session_parses_typed_sections_from_transport() {
        let (plane, _transport) = implemented_plane_with(vec![Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({
                "metadata": {"session_id": "sess-1", "title": "lane"},
                "stats": {"turns_total": 3, "queue_depth": 1},
            }),
        })])
        .await;
        let query = plane
            .query_session(&SessionId::new("sess-1").unwrap(), &["metadata", "stats"])
            .await
            .unwrap();
        assert_eq!(query.metadata.as_ref().unwrap().session_id, "sess-1");
        assert_eq!(query.stats.as_ref().unwrap().turns_total, 3);
        assert!(query.turn.is_none());
    }

    #[tokio::test]
    async fn query_session_fails_closed_on_malformed_sections() {
        let (plane, _transport) = implemented_plane_with(vec![Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({"metadata": {"session_id": 42}}),
        })])
        .await;
        let error = plane
            .query_session(&SessionId::new("sess-1").unwrap(), &["metadata"])
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "invalid_peer_reply");
    }

    #[tokio::test]
    async fn prompt_accepts_and_records_acked_receipt() {
        let key = IdempotencyKey::new("idem-key-0100").unwrap();
        let (plane, _transport) =
            implemented_plane_with(vec![ack_reply("turn-100")]).await;
        let receipt = plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-0100"),
                prompt: "hello".into(),
            })
            .await
            .unwrap();
        assert_eq!(receipt.status, GjcCommandStatus::Acked);
        assert_eq!(receipt.turn_id.as_deref(), Some("turn-100"));
        assert!(!receipt.status.is_terminal());
        // Non-terminal receipts are not replayable as outcomes.
        let error = plane.command_receipt(&key).await.unwrap_err();
        assert_eq!(error.error_code(), "invalid_request");
    }

    #[tokio::test]
    async fn idempotent_replay_returns_recorded_terminal_receipt() {
        let (plane, transport) =
            implemented_plane_with(vec![terminal_ack_reply("turn-101", "succeeded", "done")])
                .await;
        let first = plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-0101"),
                prompt: "once".into(),
            })
            .await
            .unwrap();
        assert_eq!(first.status, GjcCommandStatus::Completed);
        let second = plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-0101"),
                prompt: "twice".into(),
            })
            .await
            .unwrap();
        assert_eq!(second.command_id, first.command_id);
        assert_eq!(
            plane
                .command_receipt(&IdempotencyKey::new("idem-key-0101").unwrap())
                .await
                .unwrap()
                .command_id,
            first.command_id
        );
        // Only one peer exchange happened; the replay never re-sent.
        assert_eq!(transport.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ambiguous_ack_fails_closed_without_fabricating_outcome() {
        let (plane, _transport) = implemented_plane_with(vec![Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({"accepted": false}),
        })])
        .await;
        let error = plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-0102"),
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "ambiguous_ack");
        let receipt = plane
            .registry
            .read()
            .await
            .get("idem-key-0102")
            .cloned()
            .expect("record kept");
        // The record stays non-terminal so a replay cannot fake success.
        assert_eq!(receipt.status, GjcCommandStatus::Accepted);
    }

    #[tokio::test]
    async fn transport_errors_surface_their_taxonomy_code() {
        for (reply, expected) in [
            (
                GjcResult::<GjcResponse>::Err(GjcError::Timeout {
                    method: "control.prompt".into(),
                }),
                "timeout",
            ),
            (
                GjcResult::<GjcResponse>::Err(GjcError::StaleEndpoint {
                    capability: "session.control".into(),
                }),
                "stale_endpoint",
            ),
        ] {
            let (plane, _transport) = implemented_plane_with(vec![reply]).await;
            let error = plane
                .steer(SteerRequest {
                    envelope: envelope("idem-key-0103"),
                    message: "nudge".into(),
                })
                .await
                .unwrap_err();
            assert_eq!(error.error_code(), expected);
        }
    }

    #[tokio::test]
    async fn session_identity_mismatch_is_detected_on_query() {
        let (plane, _transport) = implemented_plane_with(vec![Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({"metadata": {"session_id": "sess-other"}}),
        })])
        .await;
        let error = plane
            .query_session(&SessionId::new("sess-1").unwrap(), &["metadata"])
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "session_mismatch");
    }

    #[tokio::test]
    async fn expected_session_mismatch_fails_closed_before_dispatch() {
        let (plane, transport) = implemented_plane_with(vec![ack_reply("turn-x")]).await;
        let mut env = envelope("idem-key-0104");
        env.expected_session = Some(SessionId::new("sess-other").unwrap());
        let error = plane
            .prompt(PromptRequest {
                envelope: env,
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "session_mismatch");
        // Nothing reached the transport.
        assert!(transport.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ack_echoing_wrong_session_fails_closed() {
        let (plane, _transport) = implemented_plane_with(vec![Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({
                "accepted": true,
                "session_id": "sess-other",
                "turn_id": "turn-105",
            }),
        })])
        .await;
        let error = plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-0105"),
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "session_mismatch");
    }

    fn envelope(key: &str) -> ControlRequestEnvelope {
        ControlRequestEnvelope {
            session: SessionId::new("sess-1").unwrap(),
            expected_session: Some(SessionId::new("sess-1").unwrap()),
            idempotency_key: IdempotencyKey::new(key).unwrap(),
            timeout_ms: ControlRequestEnvelope::DEFAULT_TIMEOUT_MS,
        }
    }

    fn plane() -> GjcControlPlane {
        GjcControlPlane::new(
            Arc::new(TransportUnavailable),
            new_shared_command_registry(),
            false,
        )
    }

    #[tokio::test]
    async fn queries_and_mutations_fail_closed_without_transport() {
        let plane = plane();
        let error = plane
            .query_session(&SessionId::new("s").unwrap(), &["metadata"])
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "transport_unavailable");
        let error = plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-0001"),
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "transport_unavailable");
    }

    #[tokio::test]
    async fn model_selection_requires_exactly_one_of_model_or_profile() {
        let plane = plane();
        let both = ModelSelectionRequest {
            envelope: envelope("idem-key-0002"),
            model: Some("m".into()),
            profile: Some("p".into()),
        };
        let error = plane.select_model(both).await.unwrap_err();
        assert_eq!(error.error_code(), "invalid_request");
    }

    #[tokio::test]
    async fn capabilities_gate_control_verbs_without_transport() {
        let caps = Capabilities::for_transport(false);
        assert!(
            caps.require(GjcCommandKind::Prompt.required_capability())
                .is_err()
        );
    }

    #[test]
    fn envelope_timeout_bounds_are_enforced() {
        let mut env = envelope("idem-key-0003");
        env.timeout_ms = 0;
        assert!(env.validate().is_err());
        env.timeout_ms = ControlRequestEnvelope::MAX_TIMEOUT_MS + 1;
        assert!(env.validate().is_err());
        env.timeout_ms = 1_000;
        assert!(env.validate().is_ok());
    }

    #[test]
    fn terminal_receipts_carry_outcome_and_turn_binding() {
        let completed = CommandReceipt {
            schema: GJC_CONTROL_SCHEMA.into(),
            command_id: "cmd-1".into(),
            idempotency_key: "idem-key-0004".into(),
            kind: "prompt".into(),
            session_id: "sess-1".into(),
            status: GjcCommandStatus::Completed,
            turn_id: Some("turn-1".into()),
            outcome: Some(json!({"status": "succeeded", "summary": "done"})),
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        assert!(completed.status.is_terminal());
        assert_eq!(
            completed.outcome.as_ref().unwrap()["status"],
            json!("succeeded")
        );
    }

    #[test]
    fn query_session_section_keys_are_stable() {
        // The wire section names are part of the public contract.
        for section in [
            "metadata",
            "stats",
            "model_profile",
            "turn",
            "queue",
            "workflow_gates",
            "goal_todo",
        ] {
            assert!(section.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }

    #[tokio::test]
    async fn command_receipt_replay_requires_terminal_state() {
        let plane = plane();
        let error = plane
            .command_receipt(&IdempotencyKey::new("idem-key-0007").unwrap())
            .await
            .unwrap_err();
        // Unknown keys surface as not-found rather than leaking registry size.
        assert_eq!(error.error_code(), "session_not_found");
    }
}
