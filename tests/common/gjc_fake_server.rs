//! Deterministic fake GJC SDK endpoint for clawhip E2E fixtures (issue
//! #326), reconciled with the full landed predecessor chain.
//!
//! Speaks the production wire contract:
//! - loopback websocket at the metadata-advertised path;
//! - `?token=` query or matching `Authorization: Bearer` authenticates
//!   (401 when neither credential matches);
//! - `{"type":"hello","connectionId":...}` immediately after authentication;
//! - `session.get` answered from the scripted phase with typed sections whose
//!   serde names match `gjc::model` exactly;
//! - `control.prompt|steer|workflow_gate_answer|ask_answer` answered by
//!   correlated `control_response` frames with `result.accepted`
//!   (`session_retired` error block once retired);
//! - scripted phase transitions broadcast uncorrelated notifications.
//!
//! Determinism: phases advance only via [`FakeGjcServer::set_phase`]; every
//! emitted string comes from [`FakeScript`] — client prompt text is never
//! reflected back. Safety: loopback bind, owner-only metadata writer.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

use super::gjc_wire::{
    EndpointMetadataFile, FakeGateSection, FakeSections, NotificationFrame, RequestFrame,
    ResponseFrame, ServerErrorBlock,
};

/// Router shared state: mutable fake state, scripted identity, notification bus.
type RouterState = (
    Arc<Mutex<FakeState>>,
    Arc<FakeScript>,
    broadcast::Sender<Notification>,
);

/// Scripted phase of the fake GJC session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FakePhase {
    /// Fresh session: turn idle, gate closed.
    #[default]
    Idle,
    /// A prompt was accepted; the turn is running.
    Running,
    /// The turn hit a workflow gate and raised a ready gate.
    Question,
    /// The gate was answered; the turn resumed to completion.
    Completed,
    /// The session reached terminal retirement.
    Retired,
}

impl FakePhase {
    fn turn_status(self) -> &'static str {
        match self {
            FakePhase::Idle => "queued",
            FakePhase::Running | FakePhase::Question => "running",
            FakePhase::Completed => "succeeded",
            FakePhase::Retired => "aborted",
        }
    }
}

/// Deterministic fixture identity for one fake session.
#[derive(Debug, Clone)]
pub struct FakeScript {
    pub session_id: String,
    pub turn_id: String,
    pub gate_id: String,
    pub question_title: String,
    pub question_options: Vec<String>,
}

impl Default for FakeScript {
    fn default() -> Self {
        Self {
            session_id: "01a02ccd-c754-7656-95c7-f40b5a140bc3".into(),
            turn_id: "fake-turn-1".into(),
            gate_id: "gate-326".into(),
            question_title: "Deploy to staging?".into(),
            question_options: vec!["yes".into(), "no".into()],
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Notification {
    Progress { turn_id: String, summary: String },
    Question,
    Completed,
    Retired,
}

impl Notification {
    fn encode(&self, script: &FakeScript) -> String {
        let frame = match self {
            Notification::Progress { turn_id, summary } => NotificationFrame {
                frame_type: "notification".into(),
                event: "session.progress".into(),
                body: json!({"turnId": turn_id, "summary": summary}),
            },
            Notification::Question => NotificationFrame {
                frame_type: "notification".into(),
                event: "session.gate".into(),
                body: json!({"gateId": script.gate_id, "title": script.question_title}),
            },
            Notification::Completed => NotificationFrame {
                frame_type: "notification".into(),
                event: "session.completed".into(),
                body: json!({"sessionId": script.session_id}),
            },
            Notification::Retired => NotificationFrame {
                frame_type: "notification".into(),
                event: "session.retired".into(),
                body: json!({"sessionId": script.session_id}),
            },
        };
        serde_json::to_string(&frame).expect("notification serializes")
    }
}

#[derive(Default)]
struct FakeState {
    phase: Option<FakePhase>,

    acked_controls: Vec<(String, bool)>,
    control_requests: Vec<(String, String, String)>,
    resolved_controls: Vec<(String, &'static str)>,
    connections_total: u64,
    reject_next_auth: bool,
    drop_next: bool,
}

impl FakeState {
    fn phase(&self) -> FakePhase {
        self.phase.unwrap_or(FakePhase::Idle)
    }

    fn phase_mut(&mut self) -> &mut Option<FakePhase> {
        &mut self.phase
    }

    fn should_reject_auth(&mut self) -> bool {
        std::mem::take(&mut self.reject_next_auth)
    }

    fn should_drop(&mut self) -> bool {
        std::mem::take(&mut self.drop_next)
    }
}

/// Handle to a running fake GJC SDK endpoint.
#[allow(dead_code)]
pub struct FakeGjcServer {
    addr: SocketAddr,
    state: Arc<Mutex<FakeState>>,
    script: Arc<FakeScript>,
    bus: broadcast::Sender<Notification>,
    listener_task: JoinHandle<()>,
}

/// Fixture token shared by the metadata writer and the endpoint check.
pub const FIXTURE_TOKEN: &str = "fixture-token-326";

impl FakeGjcServer {
    /// Start with the default deterministic script and token.
    pub async fn start() -> Self {
        Self::start_with(FakeScript::default()).await
    }

    pub async fn start_with(script: FakeScript) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind fake gjc server");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(Mutex::new(FakeState::default()));
        let script = Arc::new(script);
        let (bus, _) = broadcast::channel(64);

        let router_state: RouterState = (state.clone(), script.clone(), bus.clone());
        let app = Router::new().route(
            "/sdk",
            get(
                |ws: WebSocketUpgrade,
                 axum::extract::Query(params): axum::extract::Query<
                    std::collections::BTreeMap<String, String>,
                >,
                 headers: axum::http::HeaderMap,
                 axum::extract::State(state): axum::extract::State<RouterState>| async move {
                    // Documented contract: the credential travels as the
                    // `?token=` query parameter plus an Authorization Bearer
                    // header; either matching credential authenticates.
                    let query_ok = params.get("token").map(String::as_str) == Some(FIXTURE_TOKEN);
                    let bearer_ok = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .is_some_and(|value| value == format!("Bearer {FIXTURE_TOKEN}"));
                    if !(query_ok || bearer_ok) || state.0.lock().await.should_reject_auth() {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    ws.on_upgrade(move |socket| session_socket(socket, state.0, state.1, state.2))
                        .into_response()
                },
            )
            .with_state(router_state),
        );

        let listener_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Self {
            addr,
            state,
            script,
            bus,
            listener_task,
        }
    }

    /// The metadata `url` value for this endpoint (loopback, no credentials).
    pub fn metadata_url(&self) -> String {
        format!("ws://{}/sdk", self.addr)
    }

    /// The authenticated URL the production transport would dial.
    pub fn authenticated_url(&self) -> String {
        format!("{}?token={FIXTURE_TOKEN}", self.metadata_url())
    }

    /// Write a valid owner-only metadata file under `<worktree>/.gjc/state/sdk/`.
    pub fn write_metadata(&self, worktree: &std::path::Path) -> std::path::PathBuf {
        write_metadata_file_for_session(
            worktree,
            &self.metadata_url(),
            FIXTURE_TOKEN,
            Some(std::process::id()),
            &self.script.session_id,
        )
    }

    /// Test hook: advance the scripted phase and broadcast matching notifications.
    pub async fn set_phase(&self, phase: FakePhase) {
        *self.state.lock().await.phase_mut() = Some(phase);
        match phase {
            FakePhase::Running => {
                for summary in ["planning", "editing files", "verifying"] {
                    self.emit(Notification::Progress {
                        turn_id: self.script.turn_id.clone(),
                        summary: summary.into(),
                    })
                    .await;
                }
            }
            FakePhase::Question => self.emit(Notification::Question).await,
            FakePhase::Completed => self.emit(Notification::Completed).await,
            FakePhase::Retired => self.emit(Notification::Retired).await,
            FakePhase::Idle => {}
        }
    }

    async fn emit(&self, notification: Notification) {
        let _ = self.bus.send(notification);
    }

    pub async fn phase(&self) -> FakePhase {
        self.state.lock().await.phase()
    }

    pub async fn connections_total(&self) -> u64 {
        self.state.lock().await.connections_total
    }

    pub async fn acked_controls(&self) -> Vec<(String, bool)> {
        self.state.lock().await.acked_controls.clone()
    }

    pub async fn control_requests(&self) -> Vec<(String, String, String)> {
        self.state.lock().await.control_requests.clone()
    }

    pub async fn resolved_controls(&self) -> Vec<(String, &'static str)> {
        self.state.lock().await.resolved_controls.clone()
    }

    /// Test hook: reject the next handshake with 401 (unauthorized).
    pub async fn reject_next_handshake(&self) {
        self.state.lock().await.reject_next_auth = true;
    }

    /// Test hook: drop the next accepted connection immediately.
    pub async fn drop_next_connection(&self) {
        self.state.lock().await.drop_next = true;
    }

    pub async fn wait_for_connections(&self, expected: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if self.connections_total().await >= expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "fake gjc endpoint did not reach {expected} connections (has {})",
            self.connections_total().await
        );
    }

    pub async fn stop(self) {
        self.listener_task.abort();
    }
}

async fn session_socket(
    socket: WebSocket,
    state: Arc<Mutex<FakeState>>,
    script: Arc<FakeScript>,
    bus: broadcast::Sender<Notification>,
) {
    {
        let mut guard = state.lock().await;
        guard.connections_total += 1;
        if guard.should_drop() {
            return;
        }
    }
    let (mut sink, mut stream) = socket.split();
    let mut notifications = bus.subscribe();
    // Landed contract: hello precedes every exchange.
    let hello = json!({"type": "hello", "connectionId": "fixture-connection"});
    if sink
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            message = stream.next() => {
                let Some(message) = message else { break };
                let Ok(Message::Text(text)) = message else { continue };
                let response = handle_request(&text, &state, &script).await;
                if let Some(response) = response
                    && sink.send(Message::Text(response.into())).await.is_err()
                {
                    break;
                }
            }
            notification = notifications.recv() => {
                match notification {
                    Ok(frame) => {
                        if sink.send(Message::Text(frame.encode(&script).into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

/// Authoritative sections served for the current phase.
fn sections_for(phase: FakePhase, script: &FakeScript) -> FakeSections {
    FakeSections {
        metadata_session_id: script.session_id.clone(),
        turn_id: script.turn_id.clone(),
        revision: match phase {
            FakePhase::Idle => 1,
            FakePhase::Running => 2,
            FakePhase::Question => 3,
            FakePhase::Completed => 4,
            FakePhase::Retired => 5,
        },
        turn_status: phase.turn_status(),
        gate: match phase {
            FakePhase::Question => Some(FakeGateSection {
                gate_id: script.gate_id.clone(),
                state: "ready",
                title: script.question_title.clone(),
                options: script.question_options.clone(),
            }),
            _ => None,
        },
    }
}

/// Handle one request envelope and produce the deterministic correlated
/// response, honoring the v3 selector families: `query_request` frames are
/// answered by `query_response`, `control_request` by `control_response`.
async fn handle_request(
    text: &str,
    state: &Arc<Mutex<FakeState>>,
    script: &Arc<FakeScript>,
) -> Option<String> {
    let frame: RequestFrame = match serde_json::from_str(text) {
        Ok(frame) => frame,
        Err(_) => return None,
    };
    let phase = state.lock().await.phase();

    match frame.frame_type.as_str() {
        "query_request" => {
            let query = frame.query.as_deref().unwrap_or_default();
            if query != "session.get" {
                return Some(error_reply("query_response", &frame.id, "unknown_query"));
            }
            let wanted_session = frame.input.get("session_id").and_then(Value::as_str);
            if let Some(wanted) = wanted_session
                && wanted != script.session_id
            {
                return Some(error_reply("query_response", &frame.id, "session_mismatch"));
            }
            serde_json::to_string(&ResponseFrame::query(
                &frame.id,
                sections_for(phase, script).to_result(),
            ))
            .ok()
        }
        "control_request" => {
            let operation = frame.operation.as_deref().unwrap_or_default();
            let known = matches!(
                operation,
                "prompt" | "steer" | "abort_and_prompt" | "workflow_gate_answer" | "ask_answer"
            );
            if !known {
                return Some(error_reply(
                    "control_response",
                    &frame.id,
                    "unknown_operation",
                ));
            }
            let accepted = phase != FakePhase::Retired;
            let outcome_kind = if accepted {
                "control.accepted"
            } else {
                "control.rejected"
            };
            {
                let mut guard = state.lock().await;
                guard.acked_controls.push((frame.id.clone(), accepted));
                guard.control_requests.push((
                    operation.to_string(),
                    frame
                        .input
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    frame
                        .input
                        .get("idempotency_key")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ));
                guard
                    .resolved_controls
                    .push((frame.id.clone(), outcome_kind));
            }
            // Accept-acks carry no `outcome` object: shipped control-plane
            // semantics treat any present outcome as a TERMINAL claim and
            // fail closed unless it parses to a terminal prompt status.
            serde_json::to_string(&ResponseFrame::control(
                &frame.id,
                accepted,
                json!({
                    "accepted": accepted,
                    "session_id": script.session_id,
                    "operation": operation,
                    "command_id": frame.input.get("idempotency_key").cloned().unwrap_or(Value::Null),
                }),
            ))
            .ok()
        }
        _ => Some(error_reply(
            match frame.frame_type.as_str() {
                t if t.ends_with("_request") => t.replace("_request", "_response"),
                other => format!("{other}_response"),
            }
            .as_str(),
            &frame.id,
            "unknown_frame",
        )),
    }
}

fn error_reply(family: &str, id: &str, code: &str) -> String {
    serde_json::to_string(&ResponseFrame {
        frame_type: family.into(),
        id: Some(id.to_string()),
        ok: Some(false),
        result: Value::Null,
        error: Some(ServerErrorBlock {
            code: Some(code.into()),
            message: Some(code.into()),
        }),
    })
    .expect("error reply serializes")
}

/// Write an owner-only metadata file under `<worktree>/.gjc/state/sdk/<session>.json`.
pub fn write_metadata_file(
    worktree: &std::path::Path,
    url: &str,
    token: &str,
    pid: Option<u32>,
) -> std::path::PathBuf {
    write_metadata_file_for_session(worktree, url, token, pid, &FakeScript::default().session_id)
}

/// Write owner-only metadata for an explicit session identity.
pub fn write_metadata_file_for_session(
    worktree: &std::path::Path,
    url: &str,
    token: &str,
    pid: Option<u32>,
    session_id: &str,
) -> std::path::PathBuf {
    let sdk_dir = worktree.join(".gjc/state/sdk");
    std::fs::create_dir_all(&sdk_dir).expect("create .gjc/state/sdk");
    let file = EndpointMetadataFile {
        version: 1,
        session_id: session_id.to_string(),
        url: url.to_string(),
        token: token.to_string(),
        pid,
    };
    let path = sdk_dir.join(format!("{session_id}.json"));
    std::fs::write(
        &path,
        serde_json::to_string(&file).expect("metadata serializes"),
    )
    .expect("write metadata");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path)
            .expect("stat metadata")
            .permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(&path, permissions).expect("restrict metadata");
    }
    path
}
