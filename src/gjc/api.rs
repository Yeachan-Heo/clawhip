//! Public-safe DTOs shared by the daemon HTTP surface and the CLI.

use super::control::GjcControlPlane;
use super::model::{
    AbortAndPromptRequest, AskAnswerRequest, AskChoice, Capabilities, CommandReceipt,
    ControlRequestEnvelope, GJC_CONTROL_SCHEMA, GjcError, GjcResult, IdempotencyKey,
    ModelSelectionRequest, PromptRequest, SessionId, SessionQuery, SteerRequest,
    WorkflowGateAnswer, WorkflowGateAnswerRequest,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// Sections accepted on the session query surface. Unknown sections are
/// rejected so callers cannot probe arbitrary peer fields.
pub const SESSION_SECTIONS: &[&str] = &[
    "metadata",
    "stats",
    "model_profile",
    "turn",
    "queue",
    "workflow_gates",
    "goal_todo",
];

pub fn parse_sections(raw: Option<&str>) -> GjcResult<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(SESSION_SECTIONS.iter().map(|s| s.to_string()).collect());
    };
    let mut sections = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if !SESSION_SECTIONS.contains(&part) {
            return Err(GjcError::InvalidRequest {
                field: "sections",
                reason: format!("unknown section `{part}`"),
            });
        }
        if !sections.iter().any(|s: &String| s == part) {
            sections.push(part.to_string());
        }
    }
    if sections.is_empty() {
        return Err(GjcError::InvalidRequest {
            field: "sections",
            reason: "at least one section is required".into(),
        });
    }
    Ok(sections)
}

fn session_id(raw: &str) -> GjcResult<SessionId> {
    SessionId::new(raw)
}

fn idempotency_key(raw: &str) -> GjcResult<IdempotencyKey> {
    IdempotencyKey::new(raw)
}

fn envelope(raw: &MutationDto) -> GjcResult<ControlRequestEnvelope> {
    Ok(ControlRequestEnvelope {
        session: session_id(&raw.session)?,
        expected_session: raw
            .expected_session
            .as_deref()
            .map(session_id)
            .transpose()?,
        idempotency_key: idempotency_key(&raw.idempotency_key)?,
        timeout_ms: raw
            .timeout_ms
            .unwrap_or(ControlRequestEnvelope::DEFAULT_TIMEOUT_MS),
    })
}

/// Shared mutation DTO accepted by every control verb.
#[derive(Debug, Clone, Deserialize)]
pub struct MutationDto {
    pub session: String,
    pub idempotency_key: String,
    /// When set, the command fails closed unless the authoritative session
    /// id matches exactly.
    pub expected_session: Option<String>,
    /// Bounded peer exchange timeout in milliseconds.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PromptDto {
    #[serde(flatten)]
    pub base: MutationDto,
    pub prompt: String,
}

impl PromptDto {
    pub fn into_request(self) -> GjcResult<PromptRequest> {
        let prompt_len = self.prompt.len();
        if prompt_len == 0 || prompt_len > 16_384 {
            return Err(GjcError::InvalidRequest {
                field: "prompt",
                reason: "must be 1..=16384 bytes".into(),
            });
        }
        Ok(PromptRequest {
            envelope: envelope(&self.base)?,
            prompt: self.prompt,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SteerDto {
    #[serde(flatten)]
    pub base: MutationDto,
    pub message: String,
}

impl SteerDto {
    pub fn into_request(self) -> GjcResult<SteerRequest> {
        let message_len = self.message.len();
        if message_len == 0 || message_len > 16_384 {
            return Err(GjcError::InvalidRequest {
                field: "message",
                reason: "must be 1..=16384 bytes".into(),
            });
        }
        Ok(SteerRequest {
            envelope: envelope(&self.base)?,
            message: self.message,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AbortAndPromptDto {
    #[serde(flatten)]
    pub base: MutationDto,
    #[serde(default)]
    pub turn_ids: Vec<String>,
    pub prompt: String,
}

impl AbortAndPromptDto {
    pub fn into_request(self) -> GjcResult<AbortAndPromptRequest> {
        let prompt_len = self.prompt.len();
        if prompt_len == 0 || prompt_len > 16_384 {
            return Err(GjcError::InvalidRequest {
                field: "prompt",
                reason: "must be 1..=16384 bytes".into(),
            });
        }
        if self.turn_ids.len() > 64 {
            return Err(GjcError::InvalidRequest {
                field: "turn_ids",
                reason: "at most 64 turn ids are accepted".into(),
            });
        }
        Ok(AbortAndPromptRequest {
            envelope: envelope(&self.base)?,
            turn_ids: self
                .turn_ids
                .iter()
                .map(|raw| super::model::TurnId::new(raw.as_str()))
                .collect::<GjcResult<Vec<_>>>()?,
            prompt: self.prompt,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowGateAnswerDto {
    #[serde(flatten)]
    pub base: MutationDto,
    pub gate_id: String,
    pub option: String,
}

impl WorkflowGateAnswerDto {
    pub fn into_request(self) -> GjcResult<WorkflowGateAnswerRequest> {
        if self.gate_id.is_empty()
            || self.gate_id.len() > 128
            || self.gate_id.chars().any(|ch| ch.is_control())
        {
            return Err(GjcError::InvalidRequest {
                field: "gate_id",
                reason: "must be 1..=128 bytes".into(),
            });
        }
        if self.option.is_empty()
            || self.option.len() > 128
            || self.option.chars().any(|ch| ch.is_control())
        {
            return Err(GjcError::InvalidRequest {
                field: "option",
                reason: "must be 1..=256 bytes".into(),
            });
        }
        Ok(WorkflowGateAnswerRequest {
            envelope: envelope(&self.base)?,
            gate_id: self.gate_id,
            answer: WorkflowGateAnswer {
                option: self.option,
            },
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AskAnswerDto {
    #[serde(flatten)]
    pub base: MutationDto,
    pub ask_id: String,
    #[serde(default)]
    pub choices: Vec<String>,
}

impl AskAnswerDto {
    pub fn into_request(self) -> GjcResult<AskAnswerRequest> {
        if self.ask_id.is_empty()
            || self.ask_id.len() > 128
            || self.ask_id.chars().any(|ch| ch.is_control())
        {
            return Err(GjcError::InvalidRequest {
                field: "ask_id",
                reason: "must be 1..=128 bytes".into(),
            });
        }
        if self.choices.is_empty() || self.choices.len() > 16 {
            return Err(GjcError::InvalidRequest {
                field: "choices",
                reason: "1..=16 choices are required".into(),
            });
        }
        Ok(AskAnswerRequest {
            envelope: envelope(&self.base)?,
            ask_id: self.ask_id,
            choices: self
                .choices
                .into_iter()
                .map(|option| {
                    if option.is_empty()
                        || option.len() > 256
                        || option.chars().any(|ch| ch.is_control())
                    {
                        Err(GjcError::InvalidRequest {
                            field: "choices",
                            reason: "each choice must be 1..=256 bytes".into(),
                        })
                    } else {
                        Ok(AskChoice { option })
                    }
                })
                .collect::<GjcResult<Vec<_>>>()?,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelSelectionDto {
    #[serde(flatten)]
    pub base: MutationDto,
    pub model: Option<String>,
    pub profile: Option<String>,
}

impl ModelSelectionDto {
    pub fn into_request(self) -> GjcResult<ModelSelectionRequest> {
        for (field, value) in [("model", &self.model), ("profile", &self.profile)] {
            if let Some(value) = value.as_deref()
                && (value.is_empty()
                    || value.len() > 128
                    || value.chars().any(|ch| ch.is_control()))
            {
                return Err(GjcError::InvalidRequest {
                    field: "model",
                    reason: format!("{field} must be 1..=128 bytes when present"),
                });
            }
        }
        Ok(ModelSelectionRequest {
            envelope: envelope(&self.base)?,
            model: self.model,
            profile: self.profile,
        })
    }
}

// ---------------------------------------------------------------------------
// Public-safe rendering
// ---------------------------------------------------------------------------

/// Capabilities response body.
pub fn capabilities_body(caps: &Capabilities) -> Value {
    serde_json::to_value(caps).unwrap_or(Value::Null)
}

/// Session query response body: typed sections plus schema marker, with
/// provider metadata filtered down to scalar values so nested provider
/// objects cannot leak through the public surface.
pub fn session_query_body(query: &SessionQuery) -> Value {
    let mut body = json!({"schema": GJC_CONTROL_SCHEMA});
    if let Some(metadata) = query.metadata.as_ref() {
        let mut metadata = metadata.clone();
        metadata.provider_metadata = metadata
            .provider_metadata
            .iter()
            .filter(|(key, value)| {
                matches!(
                    key.as_str(),
                    "provider" | "model" | "profile" | "status" | "region"
                ) && (value.is_string()
                    || value.is_number()
                    || value.is_boolean()
                    || value.is_null())
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        body["metadata"] = serde_json::to_value(metadata).unwrap_or(Value::Null);
    }
    if let Some(stats) = query.stats.as_ref() {
        body["stats"] = serde_json::to_value(stats).unwrap_or(Value::Null);
    }
    if let Some(model_profile) = query.model_profile.as_ref() {
        body["model_profile"] = json!({
            "model": if model_profile.model.len() <= 128
                && !model_profile.model.chars().any(|ch| ch.is_control())
            {
                model_profile.model.clone()
            } else {
                "redacted".to_string()
            },
            "profile": model_profile.profile.as_deref().filter(|profile| {
                profile.len() <= 128 && !profile.chars().any(|ch| ch.is_control())
            }),
        });
    }
    if let Some(turn) = query.turn.as_ref() {
        let mut turn_body = serde_json::to_value(turn).unwrap_or(Value::Null);
        if let Some(outcome) = turn_body.get("outcome").cloned() {
            turn_body["outcome"] = crate::gjc::control::public_outcome(&outcome);
        }
        body["turn"] = turn_body;
    } else if query.turn_present {
        body["turn"] = Value::Null;
    }
    if let Some(queue) = query.queue.as_ref() {
        body["queue"] = json!({
            "depth": queue.depth,
            "entries": queue
                .entries
                .iter()
                .take(64)
                .map(|entry| json!({
                    "position": entry.position,
                    "kind": if entry.kind.len() <= 128
                        && !entry.kind.chars().any(|ch| ch.is_control())
                    {
                        entry.kind.clone()
                    } else {
                        "redacted".to_string()
                    },
                    "summary": entry.summary.as_deref().filter(|summary| {
                        summary.len() <= 256 && !summary.chars().any(|ch| ch.is_control())
                    }),
                }))
                .collect::<Vec<_>>(),
        });
    }
    if let Some(gates) = query.workflow_gates.as_ref() {
        body["workflow_gates"] = Value::Array(
            gates
                .iter()
                .map(|gate| {
                    let title = gate.title.as_deref().filter(|value| {
                        value.len() <= 256 && !value.chars().any(|ch| ch.is_control())
                    });
                    let options = gate
                        .options
                        .iter()
                        .filter(|value| {
                            !value.is_empty()
                                && value.len() <= 256
                                && !value.chars().any(|ch| ch.is_control())
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    json!({
                        "gate_id": gate.gate_id,
                        "state": gate.state,
                        "title": title,
                        "options": options,
                    })
                })
                .collect(),
        );
    }
    if let Some(goal_todo) = query.goal_todo.as_ref() {
        body["goal_todo"] = serde_json::json!({
            "todo_count": goal_todo.todos.len(),
        });
    }
    body
}

/// Command receipt response body.
pub fn command_receipt_body(receipt: &CommandReceipt) -> Value {
    let mut body = serde_json::to_value(receipt).unwrap_or(Value::Null);
    if let Some(outcome) = body.get("outcome").cloned() {
        body["outcome"] = crate::gjc::control::public_outcome(&outcome);
    }
    body
}

/// Terminal outcome response body.
pub fn outcome_body(outcome: &Value) -> Value {
    json!({
        "schema": GJC_CONTROL_SCHEMA,
        "outcome": outcome,
    })
}

/// Map a control-plane error to an HTTP-ish (status, body) pair for both
/// daemon handlers and CLI rendering.
pub fn error_response(error: &GjcError) -> (u16, Value) {
    (error.http_status(), error.public_body())
}

/// Run a session query through the control plane and render its body.
pub async fn run_session_query(
    plane: &GjcControlPlane,
    session: &str,
    sections: Option<&str>,
) -> GjcResult<Value> {
    let session = session_id(session)?;
    let sections = parse_sections(sections)?;
    let refs = sections.iter().map(String::as_str).collect::<Vec<_>>();
    let query = plane.query_session(&session, &refs).await?;
    Ok(session_query_body(&query))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_parse_defaults_to_all() {
        let sections = parse_sections(None).unwrap();
        assert_eq!(sections.len(), SESSION_SECTIONS.len());
    }

    #[test]
    fn sections_reject_unknown_and_empty() {
        assert!(parse_sections(Some("metadata,secret")).is_err());
        assert!(parse_sections(Some(",,")).is_err());
        assert!(parse_sections(Some("metadata,metadata")).unwrap().len() == 1);
    }

    #[test]
    fn prompt_dto_validates_before_transport() {
        let error = PromptDto {
            base: base_dto(),
            prompt: String::new(),
        }
        .into_request()
        .unwrap_err();
        assert_eq!(error.error_code(), "invalid_request");

        let invalid_id = AskAnswerDto {
            base: base_dto(),
            ask_id: "ask-\n1".into(),
            choices: vec!["yes".into()],
        };
        assert!(invalid_id.into_request().is_err());
        let invalid_choice = AskAnswerDto {
            base: base_dto(),
            ask_id: "ask-1".into(),
            choices: vec!["yes\u{7f}".into()],
        };
        assert!(invalid_choice.into_request().is_err());
    }

    #[test]
    fn mutation_dto_rejects_short_idempotency_keys() {
        let mut base = base_dto();
        base.idempotency_key = "short".into();
        let error = PromptDto {
            base,
            prompt: "hi".into(),
        }
        .into_request()
        .unwrap_err();
        assert_eq!(error.error_code(), "invalid_request");
    }

    #[test]
    fn gate_answer_dto_validates_bounds() {
        let error = WorkflowGateAnswerDto {
            base: base_dto(),
            gate_id: String::new(),
            option: "ok".into(),
        }
        .into_request()
        .unwrap_err();
        assert_eq!(error.error_code(), "invalid_request");
        let mut bad_control = WorkflowGateAnswerDto {
            base: base_dto(),
            gate_id: "gate-\n1".into(),
            option: "ok".into(),
        };
        assert!(bad_control.clone().into_request().is_err());
        bad_control.gate_id = "gate-1".into();
        bad_control.option = "ok\u{7f}".into();
        assert!(bad_control.into_request().is_err());
    }

    #[test]
    fn ask_answer_dto_requires_choices() {
        let error = AskAnswerDto {
            base: base_dto(),
            ask_id: "ask-1".into(),
            choices: vec![],
        }
        .into_request()
        .unwrap_err();
        assert_eq!(error.error_code(), "invalid_request");
    }

    #[test]
    fn model_selection_dto_rejects_both_and_neither() {
        let dto = ModelSelectionDto {
            base: base_dto(),
            model: Some("m".into()),
            profile: Some("p".into()),
        };
        // DTO-level validation passes; exactly-one is enforced by the
        // control plane (and surfaced as invalid_request there).
        assert!(dto.clone().into_request().is_ok());
        let invalid = ModelSelectionDto {
            base: base_dto(),
            model: Some("model\nname".into()),
            profile: None,
        };
        assert!(invalid.into_request().is_err());
    }

    #[test]
    fn session_query_body_filters_non_scalar_provider_metadata() {
        let query = SessionQuery {
            metadata: Some(super::super::model::SessionMetadata {
                session_id: "sess-1".into(),
                title: Some("t".into()),
                project: None,
                created_at: None,
                last_active_at: None,
                lane: None,
                provider_metadata: [
                    ("provider".to_string(), Value::String("v".into())),
                    ("nested".to_string(), json!({"secret": true})),
                ]
                .into_iter()
                .collect(),
            }),
            ..SessionQuery::default()
        };
        let body = session_query_body(&query);
        assert_eq!(body["metadata"]["provider_metadata"]["provider"], "v");
        assert!(
            body["metadata"]["provider_metadata"]
                .get("nested")
                .is_none()
        );
    }

    #[test]
    fn session_query_body_preserves_present_null_turn() {
        let query = SessionQuery {
            turn_present: true,
            ..SessionQuery::default()
        };
        assert_eq!(session_query_body(&query)["turn"], Value::Null);
    }

    #[test]
    fn session_query_body_never_exposes_raw_goal_or_todo_values() {
        let query = SessionQuery {
            goal_todo: Some(crate::gjc::model::GoalTodoSnapshot {
                goal: Some(serde_json::json!("prompt=secret")),
                todos: vec![serde_json::json!("token=secret")],
            }),
            ..SessionQuery::default()
        };
        assert_eq!(
            session_query_body(&query)["goal_todo"],
            serde_json::json!({"todo_count": 1})
        );
    }

    #[test]
    fn abort_dto_bounds_turn_ids() {
        let mut dto = AbortAndPromptDto {
            base: base_dto(),
            turn_ids: (0..65).map(|i| format!("turn-{i}")).collect(),
            prompt: "p".into(),
        };
        assert!(dto.clone().into_request().is_err());
        dto.turn_ids.truncate(1);
        assert!(dto.into_request().is_ok());
    }

    fn base_dto() -> MutationDto {
        MutationDto {
            session: "sess-1".into(),
            idempotency_key: "idem-key-0001".into(),
            expected_session: Some("sess-1".into()),
            timeout_ms: None,
        }
    }
}
