//! GJC SDK fake-endpoint contract tests (issue #326), reconciled with the
//! full landed predecessor chain (#322/#323/#324 + hardening).
//!
//! Proves the deterministic fake endpoint behaves as the production
//! transport and control plane demand: token-authenticated loopback
//! websocket, hello-after-auth, `session.get` typed sections, correlated
//! `control.*` verbs with accepted verdicts, uncorrelated notification
//! streaming, disconnect/reconnect survival, and terminal retirement with no
//! ghost frames.

mod common;

use std::collections::VecDeque;
use std::time::Duration;

use common::gjc_fake_server::{FIXTURE_TOKEN, FakeGjcServer, FakePhase};
use common::gjc_wire::{EndpointMetadataFile, InboundFrame, RequestFrame, ResponseFrame};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream as WsTcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type WsStream = WebSocketStream<MaybeTlsStream<WsTcpStream>>;

async fn connect_authenticated(server: &FakeGjcServer) -> WsStream {
    let request = server.authenticated_url().into_client_request().unwrap();
    let (stream, _) = connect_async(request).await.expect("connect authenticated");
    stream
}

/// Persistent decoded-frame queue over one websocket connection.
#[derive(Default)]
struct Frames(VecDeque<InboundFrame>);

impl Frames {
    async fn next(&mut self, stream: &mut WsStream) -> InboundFrame {
        loop {
            if let Some(frame) = self.0.pop_front() {
                return frame;
            }
            let message = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;
            match message {
                Ok(Some(Ok(Message::Text(text)))) => {
                    if let Some(frame) = InboundFrame::decode(text.as_str()) {
                        self.0.push_back(frame);
                    }
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(error))) => panic!("stream error: {error}"),
                Ok(None) => panic!("stream closed while waiting for frame"),
                Err(_) => panic!("frame within timeout"),
            }
        }
    }
}

async fn round_trip_query(
    frames: &mut Frames,
    stream: &mut WsStream,
    correlation: &str,
    input: Value,
) -> ResponseFrame {
    round_trip(
        frames,
        stream,
        correlation,
        RequestFrame::query(correlation, "session.get", input),
    )
    .await
}

async fn round_trip_control(
    frames: &mut Frames,
    stream: &mut WsStream,
    correlation: &str,
    operation: &str,
    input: Value,
) -> ResponseFrame {
    round_trip(
        frames,
        stream,
        correlation,
        RequestFrame::control(correlation, operation, input),
    )
    .await
}

async fn round_trip(
    frames: &mut Frames,
    stream: &mut WsStream,
    correlation: &str,
    frame: RequestFrame,
) -> ResponseFrame {
    stream
        .send(Message::Text(serde_json::to_string(&frame).unwrap().into()))
        .await
        .unwrap();
    match frames.next(stream).await {
        InboundFrame::Response(response) if response.id.as_deref() == Some(correlation) => response,
        InboundFrame::Response(other) => {
            panic!("uncorrelated response {other:?} during round trip {correlation}")
        }
        other => panic!("expected response for {correlation}, got {other:?}"),
    }
}

#[tokio::test]
async fn metadata_file_carries_landed_schema_and_owner_only_permissions() {
    let temp = tempfile::TempDir::new().unwrap();
    let server = FakeGjcServer::start().await;
    let path = server.write_metadata(temp.path());
    assert!(path.starts_with(temp.path().join(".gjc/state/sdk")));

    let file: EndpointMetadataFile =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(file.version, 1);
    assert_eq!(file.url, server.metadata_url());
    assert!(file.url.starts_with("ws://127.0.0.1:"));
    assert!(
        !file.url.contains("token"),
        "metadata url must stay credential-free"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "metadata must be owner-only");
    }
    server.stop().await;
}

#[tokio::test]
async fn handshake_requires_token_and_hello_precedes_everything() {
    let server = FakeGjcServer::start().await;

    let bare = server.metadata_url().into_client_request().unwrap();
    let rejected = connect_async(bare).await;
    assert!(
        matches!(&rejected, Err(tokio_tungstenite::tungstenite::Error::Http(response))
            if response.status().as_u16() == 401),
        "unauthenticated handshake must be rejected"
    );

    let wrong = format!("{}?token=nope", server.metadata_url())
        .into_client_request()
        .unwrap();
    assert!(matches!(
        connect_async(wrong).await,
        Err(tokio_tungstenite::tungstenite::Error::Http(response))
            if response.status().as_u16() == 401
    ));

    let mut stream = connect_authenticated(&server).await;
    let mut frames = Frames::default();
    match frames.next(&mut stream).await {
        InboundFrame::Hello(hello) => assert_eq!(hello.connection_id, "fixture-connection"),
        other => panic!("hello must precede everything else, got {other:?}"),
    }
    server.stop().await;
}

#[tokio::test]
async fn session_get_returns_authoritative_typed_sections() {
    let server = FakeGjcServer::start().await;
    server.set_phase(FakePhase::Question).await;
    let mut stream = connect_authenticated(&server).await;
    let mut frames = Frames::default();
    frames.next(&mut stream).await; // hello

    let response = round_trip_query(
        &mut frames,
        &mut stream,
        "corr-1",
        json!({"session_id": "01a02ccd-c754-7656-95c7-f40b5a140bc3", "sections": ["metadata", "turn", "workflow_gates"]}),
    )
    .await;
    assert_eq!(response.ok, Some(true));
    assert_eq!(response.frame_type, "query_response");
    assert_eq!(
        response.result["metadata"]["session_id"],
        "01a02ccd-c754-7656-95c7-f40b5a140bc3"
    );
    assert_eq!(response.result["turn"]["status"], "running");
    assert_eq!(response.result["workflow_gates"][0]["gate_id"], "gate-326");
    assert_eq!(response.result["workflow_gates"][0]["state"], "ready");

    // Session identity is enforced: a foreign session id fails closed.
    let foreign = round_trip_query(
        &mut frames,
        &mut stream,
        "corr-2",
        json!({"session_id": "other-session"}),
    )
    .await;
    assert_eq!(foreign.ok, Some(false));
    assert_eq!(
        foreign.error.and_then(|error| error.code),
        Some("session_mismatch".into())
    );
    server.stop().await;
}

#[tokio::test]
async fn control_verbs_accept_with_correlated_receipts() {
    let server = FakeGjcServer::start().await;
    server.set_phase(FakePhase::Running).await;
    let mut stream = connect_authenticated(&server).await;
    let mut frames = Frames::default();
    frames.next(&mut stream).await; // hello

    for (operation, id) in [("prompt", "c-prompt"), ("steer", "c-steer")] {
        let response = round_trip_control(
            &mut frames,
            &mut stream,
            id,
            operation,
            json!({
                "session_id": "01a02ccd-c754-7656-95c7-f40b5a140bc3",
                "idempotency_key": format!("idem-{id}"),
                "prompt": "fixed-fixture-prompt",
            }),
        )
        .await;
        assert_eq!(response.frame_type, "control_response");
        assert_eq!(
            response.ok,
            Some(true),
            "{operation} accepted while running"
        );
        assert_eq!(response.result["accepted"], true);
    }
    assert_eq!(server.resolved_controls().await.len(), 2);
    server.stop().await;
}

#[tokio::test]
async fn gate_answer_verb_round_trips_against_ready_gate() {
    let server = FakeGjcServer::start().await;
    server.set_phase(FakePhase::Question).await;
    let mut stream = connect_authenticated(&server).await;
    let mut frames = Frames::default();
    frames.next(&mut stream).await; // hello

    let answer = round_trip_control(
        &mut frames,
        &mut stream,
        "answer-1",
        "workflow_gate_answer",
        json!({
            "session_id": "01a02ccd-c754-7656-95c7-f40b5a140bc3",
            "idempotency_key": "idem-answer-1",
            "gate_id": "gate-326",
            "option": "yes",
        }),
    )
    .await;
    assert_eq!(answer.ok, Some(true));
    assert_eq!(answer.result["accepted"], true);
    server.stop().await;
}

#[tokio::test]
async fn progress_notifications_stream_in_script_order_while_running() {
    let server = FakeGjcServer::start().await;
    let mut stream = connect_authenticated(&server).await;
    let mut frames = Frames::default();
    frames.next(&mut stream).await; // hello

    server.set_phase(FakePhase::Running).await;
    for expected in ["planning", "editing files", "verifying"] {
        match frames.next(&mut stream).await {
            InboundFrame::Notification(notification) => {
                assert_eq!(notification.event, "session.progress");
                assert_eq!(notification.body["summary"], expected);
            }
            other => panic!("expected progress '{expected}', got {other:?}"),
        }
    }
    server.stop().await;
}

#[tokio::test]
async fn retirement_rejects_controls_and_stays_silent() {
    let server = FakeGjcServer::start().await;
    let mut stream = connect_authenticated(&server).await;
    let mut frames = Frames::default();
    frames.next(&mut stream).await; // hello

    server.set_phase(FakePhase::Retired).await;
    match frames.next(&mut stream).await {
        InboundFrame::Notification(notification) => {
            assert_eq!(notification.event, "session.retired");
        }
        other => panic!("expected retired notification, got {other:?}"),
    }

    let late = round_trip_control(
        &mut frames,
        &mut stream,
        "late-1",
        "prompt",
        json!({"session_id": "01a02ccd-c754-7656-95c7-f40b5a140bc3", "idempotency_key": "late", "prompt": "late"}),
    )
    .await;
    assert_eq!(late.ok, Some(false));
    assert_eq!(
        late.error.as_ref().and_then(|error| error.code.clone()),
        Some("session_retired".into())
    );

    // No ghost traffic after retirement.
    let quiet = tokio::time::timeout(Duration::from_millis(300), stream.next()).await;
    assert!(
        quiet.is_err() || matches!(quiet, Ok(None)),
        "no frames after retirement"
    );
    server.stop().await;
}

#[tokio::test]
async fn completed_phase_broadcasts_completion_and_reports_phase() {
    let server = FakeGjcServer::start().await;
    let mut stream = connect_authenticated(&server).await;
    let mut frames = Frames::default();
    frames.next(&mut stream).await; // hello

    server.set_phase(FakePhase::Completed).await;
    match frames.next(&mut stream).await {
        InboundFrame::Notification(notification) => {
            assert_eq!(notification.event, "session.completed");
        }
        other => panic!("expected completed notification, got {other:?}"),
    }
    assert_eq!(server.phase().await, FakePhase::Completed);
    server.stop().await;
}

#[tokio::test]
async fn forced_disconnect_is_survived_by_reconnection() {
    let server = FakeGjcServer::start().await;
    server.drop_next_connection().await;

    let mut first = connect_authenticated(&server).await;
    server.wait_for_connections(1).await;
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match first.next().await {
                None | Some(Err(_)) => break,
                Some(Ok(Message::Close(_))) => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "first connection must terminate on forced drop"
    );

    let mut second = connect_authenticated(&server).await;
    server.wait_for_connections(2).await;
    let mut frames = Frames::default();
    frames.next(&mut second).await; // hello
    let response = round_trip_query(&mut frames, &mut second, "reconnect-1", json!({})).await;
    assert_eq!(response.ok, Some(true));
    assert_eq!(server.connections_total().await, 2);
    server.stop().await;
}

#[tokio::test]
async fn unauthorized_handshake_can_be_scripted_after_live_connections() {
    let server = FakeGjcServer::start().await;
    server.reject_next_handshake().await;

    let rejected = connect_async(server.authenticated_url().into_client_request().unwrap()).await;
    assert!(
        matches!(&rejected, Err(tokio_tungstenite::tungstenite::Error::Http(response))
        if response.status().as_u16() == 401)
    );

    let mut stream = connect_authenticated(&server).await;
    let mut frames = Frames::default();
    assert!(matches!(
        frames.next(&mut stream).await,
        InboundFrame::Hello(_)
    ));
    assert_eq!(FIXTURE_TOKEN, "fixture-token-326"); // fixture constant pinned
    server.stop().await;
}
