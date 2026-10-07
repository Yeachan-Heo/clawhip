//! Typed wire fixtures for the landed GJC SDK contract (issue #326),
//! reconciled with the full predecessor chain: #322 transport/discovery,
//! #323 control plane (`session.get` + `control.*` verbs), #324 event
//! bridge snapshot schema, and the #330/#331 envelope hardening.
//!
//! `clawhip` is a binary crate, so integration tests cannot import the
//! production modules; these fixtures mirror only the *wire shapes*:
//! - metadata file `<worktree>/.gjc/state/sdk/<session-id>.json` with
//!   `{version, sessionId, url, token, pid?}` written owner-only;
//! - authenticated loopback websocket with `?token=` query credential;
//! - server hello `{"type":"hello","connectionId":...}` after auth;
//! - client requests `{"type":"request","id":<correlation>,"method":...,
//!   "params":{...}}` answered by correlated responses
//!   `{"type":"query_response"|"control_response","id":<same>,"ok":bool,
//!   "result":{...},"error":{code,message}?}`;
//! - server-initiated notifications are uncorrelated frames (no `id`),
//!   tolerated by the production client up to its bounded stray-frame budget.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Metadata file schema (`<state-root>/sdk/<session-id>.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EndpointMetadataFile {
    pub version: u8,
    #[serde(rename = "sessionId", alias = "session_id")]
    pub session_id: String,
    pub url: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

/// Typed v3 request frame. Exactly one of `query` (on `query_request`) or
/// `operation` (on `control_request`/`broker_request`) selects the call;
/// `input` is a JSON object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RequestFrame {
    /// Correlation ID echoed by the server on the matching response.
    pub id: String,
    #[serde(rename = "type")]
    pub frame_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(default)]
    pub input: Value,
}

impl RequestFrame {
    pub fn query(id: &str, query: &str, input: Value) -> Self {
        Self {
            id: id.to_string(),
            frame_type: "query_request".into(),
            query: Some(query.to_string()),
            operation: None,
            input,
        }
    }

    pub fn control(id: &str, operation: &str, input: Value) -> Self {
        Self {
            id: id.to_string(),
            frame_type: "control_request".into(),
            query: None,
            operation: Some(operation.to_string()),
            input,
        }
    }
}

/// Correlated server response envelope. `frame_type` distinguishes query and
/// control replies exactly as the hardened #322 parser accepts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResponseFrame {
    #[serde(rename = "type")]
    pub frame_type: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub ok: Option<bool>,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub error: Option<ServerErrorBlock>,
}

impl ResponseFrame {
    pub fn query(id: &str, result: Value) -> Self {
        Self {
            frame_type: "query_response".into(),
            id: Some(id.to_string()),
            ok: Some(true),
            result,
            error: None,
        }
    }

    pub fn control(id: &str, accepted: bool, detail: Value) -> Self {
        Self {
            frame_type: "control_response".into(),
            id: Some(id.to_string()),
            ok: Some(accepted),
            result: detail,
            error: (!accepted).then(|| ServerErrorBlock {
                code: Some("session_retired".into()),
                message: Some("controls against a retired session are rejected".into()),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerErrorBlock {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Server hello frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HelloFrame {
    #[serde(rename = "type")]
    pub frame_type: String,
    #[serde(rename = "connectionId", alias = "connection_id")]
    pub connection_id: String,
}

/// A decoded inbound frame from the fake SDK endpoint.
#[derive(Debug, Clone, PartialEq)]
pub enum InboundFrame {
    Hello(HelloFrame),
    Response(ResponseFrame),
    Notification(NotificationFrame),
    Unknown(String),
}

/// Server-initiated notification (uncorrelated: no `id` field).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NotificationFrame {
    #[serde(rename = "type")]
    pub frame_type: String,
    /// Event discriminator, e.g. `session.progress`.
    pub event: String,
    #[serde(flatten)]
    pub body: Value,
}

impl InboundFrame {
    pub fn decode(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let frame_type = value.get("type")?.as_str()?.to_string();
        Some(match frame_type.as_str() {
            "hello" => InboundFrame::Hello(HelloFrame {
                frame_type: "hello".into(),
                connection_id: value.get("connectionId")?.as_str()?.to_string(),
            }),
            "notification" => InboundFrame::Notification(decode_notification(&value)?),
            "query_response" | "control_response" | "broker_response" => {
                if value.get("id").is_none() && value.get("event").is_some() {
                    // Defensive: a notification mislabeled as a response is
                    // still treated as uncorrelated notification traffic.
                    return Some(InboundFrame::Notification(decode_notification(&value)?));
                }
                InboundFrame::Response(ResponseFrame {
                    frame_type: frame_type.clone(),
                    id: value.get("id").and_then(Value::as_str).map(str::to_string),
                    ok: value.get("ok").and_then(Value::as_bool),
                    result: value.get("result").cloned().unwrap_or(Value::Null),
                    error: value.get("error").map(|error| {
                        serde_json::from_value::<ServerErrorBlock>(error.clone()).unwrap_or(
                            ServerErrorBlock {
                                code: None,
                                message: None,
                            },
                        )
                    }),
                })
            }
            other => InboundFrame::Unknown(other.to_string()),
        })
    }
}

fn decode_notification(value: &Value) -> Option<NotificationFrame> {
    let mut body = value.clone();
    let object = body.as_object_mut()?;
    let event = object.get("event").and_then(Value::as_str)?.to_string();
    object.remove("type");
    object.remove("event");
    Some(NotificationFrame {
        frame_type: "notification".into(),
        event,
        body,
    })
}

/// Authoritative `session.get` reply sections served by the fake endpoint.
/// Field names match `gjc::model` serde names exactly.
#[derive(Debug, Clone)]
pub struct FakeSections {
    pub metadata_session_id: String,
    pub turn_id: String,
    pub revision: u64,
    /// queued | running | succeeded | failed | aborted
    pub turn_status: &'static str,
    pub gate: Option<FakeGateSection>,
}

#[derive(Debug, Clone)]
pub struct FakeGateSection {
    pub gate_id: String,
    /// ready | answered
    pub state: &'static str,
    pub title: String,
    pub options: Vec<String>,
}

impl FakeSections {
    pub fn to_result(&self) -> Value {
        let mut turn = serde_json::json!({
            "turn_id": self.turn_id,
            "status": self.turn_status,
            "prompt_accepted": true,
            "started_at": "2026-08-23T00:00:00Z",
        });
        if matches!(self.turn_status, "succeeded" | "failed") {
            turn["finished_at"] = serde_json::json!("2026-08-23T00:05:00Z");
            turn["outcome"] = serde_json::json!({
                "status": self.turn_status,
                "summary": "turn reached terminal state",
                "finished_at": "2026-08-23T00:05:00Z",
            });
        }
        let mut result = serde_json::json!({
            "revision": self.revision,
            "metadata": {"session_id": self.metadata_session_id},
            "turn": turn,
        });
        if let Some(gate) = &self.gate {
            result["workflow_gates"] = serde_json::json!([{
                "gate_id": gate.gate_id,
                "workflow_id": "workflow-326",
                "kind": "ask",
                "state": gate.state,
                "title": gate.title,
                "options": gate.options,
                "raised_at": "2026-08-23T00:03:00Z",
            }]);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_landed_envelope_shapes() {
        let hello = InboundFrame::decode(r#"{"type":"hello","connectionId":"c1"}"#).unwrap();
        assert!(matches!(hello,
            InboundFrame::Hello(HelloFrame { connection_id, .. }) if connection_id == "c1"));

        let correlated = InboundFrame::decode(
            r#"{"type":"query_response","id":"abc","ok":true,"result":{"x":1}}"#,
        )
        .unwrap();
        match correlated {
            InboundFrame::Response(response) => {
                assert_eq!(response.id.as_deref(), Some("abc"));
                assert_eq!(response.ok, Some(true));
                assert_eq!(response.result["x"], 1);
            }
            other => panic!("expected correlated response, got {other:?}"),
        }

        let control = InboundFrame::decode(
            r#"{"type":"control_response","id":"e2","ok":true,"result":{"accepted":true}}"#,
        )
        .unwrap();
        assert!(matches!(control, InboundFrame::Response(_)));

        let notification = InboundFrame::decode(
            r#"{"type":"notification","event":"session.progress","turnId":"t1"}"#,
        )
        .unwrap();
        match notification {
            InboundFrame::Notification(notification) => {
                assert_eq!(notification.event, "session.progress");
                assert_eq!(notification.body["turnId"], "t1");
            }
            other => panic!("expected notification, got {other:?}"),
        }

        assert_eq!(
            InboundFrame::decode(r#"{"type":"future","x":1}"#),
            Some(InboundFrame::Unknown("future".into()))
        );
        assert_eq!(InboundFrame::decode("not json"), None);
    }

    #[test]
    fn metadata_file_round_trips_camel_case_schema() {
        let file: EndpointMetadataFile = serde_json::from_str(
            r#"{"version":1,"sessionId":"01a02ccd-c754-7656-95c7-f40b5a140bc3","url":"ws://127.0.0.1:9/","token":"tok","pid":42}"#,
        )
        .unwrap();
        assert_eq!(file.version, 1);
        assert_eq!(file.pid, Some(42));
        let serialized = serde_json::to_string(&file).unwrap();
        assert!(serialized.contains("\"sessionId\""));
    }

    #[test]
    fn session_get_sections_match_model_serde_names() {
        let sections = FakeSections {
            metadata_session_id: "sess-326".into(),
            turn_id: "turn-1".into(),
            revision: 1,
            turn_status: "running",
            gate: Some(FakeGateSection {
                gate_id: "gate-326".into(),
                state: "ready",
                title: "Deploy to staging?".into(),
                options: vec!["yes".into(), "no".into()],
            }),
        };
        let result = sections.to_result();
        assert_eq!(result["metadata"]["session_id"], "sess-326");
        assert_eq!(result["turn"]["status"], "running");
        assert_eq!(result["workflow_gates"][0]["gate_id"], "gate-326");
        assert_eq!(result["workflow_gates"][0]["state"], "ready");
    }
}
