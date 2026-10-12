//! Full-loop GJC SDK daemon E2E (issue #326) on the combined landed
//! contract (#322 transport/discovery, #323 control plane, #324 event
//! bridge, #325 durable lane reconciliation).
//!
//! Drives a real clawhip binary end to end against the deterministic fake
//! SDK endpoint using only shipped surfaces:
//! publish metadata -> start daemon with `[gjc]`/`[gjc_lanes]` -> register
//! GJC lane -> submit prompt through `/api/gjc/prompt` (control plane
//! round-trip over discovery transport) -> observe progress/question/
//! completion through the #324 bridge ingress (`/api/gjc/bridge`) feeding
//! the normal ledger->router->sink pipeline -> answer the gate via
//! `/api/gjc/workflow-gate-answer` -> prove a full-section authoritative
//! read through `/api/gjc/session/{session}` -> retire the lane ->
//! restart the daemon with no ghost ownership and no replayed deliveries.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::gjc_fake_server::{FakeGjcServer, FakePhase, FakeScript};

use serde_json::{Value, json};
use tempfile::TempDir;

struct DaemonGuard(Child);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn unused_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn request(port: u16, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
    let body_bytes = body
        .map(|value| value.to_string().into_bytes())
        .unwrap_or_default();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nx-clawhip-local-control: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body_bytes.len()
    )
    .unwrap();
    stream.write_all(&body_bytes).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let raw = String::from_utf8(response).unwrap();
    let status: u16 = raw
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    let payload = raw
        .split("\r\n\r\n")
        .nth(1)
        .and_then(|body| serde_json::from_str::<Value>(body).ok())
        .unwrap_or(Value::Null);
    (status, payload)
}

fn wait_for_health(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(15);
    // Bounded, non-panicking readiness: first wait for the TCP listener so a
    // still-starting daemon cannot spam connection-refused panics, then poll
    // the real /health surface until it answers 200.
    while Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_millis(250),
        )
        .is_ok()
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    while Instant::now() < deadline {
        match request(port, "GET", "/health", None) {
            (200, _) => return,
            _ => thread::sleep(Duration::from_millis(50)),
        }
    }
    panic!("daemon did not become healthy on port {port}");
}

fn write_config(temp: &TempDir, port: u16) -> (PathBuf, PathBuf) {
    let config_path = temp.path().join("clawhip.toml");
    let delivery = temp.path().join("delivery.jsonl");
    let ledger_dir = temp.path().join("ledger");
    let lane_state = temp.path().join("gjc-lanes-state");
    std::fs::write(
        &config_path,
        format!(
            r#"[daemon]
bind_host = "127.0.0.1"
port = {port}
base_url = "http://127.0.0.1:{port}"

[gjc]
enabled = true

[gjc_lanes]
enabled = true
poll_interval_secs = 1
initial_backoff_ms = 50
max_backoff_ms = 200
state_path = "{}"

[[routes]]
event = "workflow.question"
sink = "localfile"
local_path = "{}"

[[routes]]
event = "session.finished"
sink = "localfile"
local_path = "{}"

[ledger]
enabled = true
path = "{}"
raw_retention_days = 7
summary_retention_days = 30
compaction_interval_secs = 1
max_records = 1000
max_record_bytes = 4096
max_keywords = 8
max_keyword_bytes = 32
max_query_results = 50
max_records_per_compaction = 100
"#,
            lane_state.display(),
            delivery.display(),
            delivery.display(),
            ledger_dir.display()
        ),
    )
    .unwrap();
    (config_path, delivery)
}

fn write_two_lane_config(
    temp: &TempDir,
    port: u16,
    worktree_a: &Path,
    delivery_a: &Path,
    worktree_b: &Path,
    delivery_b: &Path,
) -> PathBuf {
    let config_path = temp.path().join("clawhip-two-lane.toml");
    let ledger_dir = temp.path().join("two-lane-ledger");
    let lane_state = temp.path().join("two-lane-gjc-state");
    std::fs::write(
        &config_path,
        format!(
            r#"[daemon]
bind_host = "127.0.0.1"
port = {port}
base_url = "http://127.0.0.1:{port}"

[gjc]
enabled = true

[gjc_lanes]
enabled = true
discovery_worktrees = ["{}", "{}"]
poll_interval_secs = 1
initial_backoff_ms = 50
max_backoff_ms = 200
state_path = "{}"

[[routes]]
event = "*"
filter = {{ worktree_path = "{}" }}
sink = "localfile"
local_path = "{}"

[[routes]]
event = "*"
filter = {{ worktree_path = "{}" }}
sink = "localfile"
local_path = "{}"

[ledger]
enabled = true
path = "{}"
raw_retention_days = 7
summary_retention_days = 30
compaction_interval_secs = 1
max_records = 1000
max_record_bytes = 4096
max_keywords = 8
max_keyword_bytes = 32
max_query_results = 50
max_records_per_compaction = 100
"#,
            worktree_a.display(),
            worktree_b.display(),
            lane_state.display(),
            worktree_a.display(),
            delivery_a.display(),
            worktree_b.display(),
            delivery_b.display(),
            ledger_dir.display()
        ),
    )
    .unwrap();
    config_path
}

fn spawn_daemon(config_path: &Path, stderr_path: &Path, worktree: Option<&Path>) -> DaemonGuard {
    let stderr = std::fs::File::create(stderr_path).expect("create daemon stderr log");
    let mut command = Command::new(env!("CARGO_BIN_EXE_clawhip"));
    command
        .arg("--config")
        .arg(config_path)
        .arg("start")
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));
    if let Some(worktree) = worktree {
        // Anchors the control plane's discovery root to the isolated
        // worktree so only this fixture's endpoint is discoverable.
        command.current_dir(worktree);
    }
    DaemonGuard(command.spawn().expect("spawn clawhip daemon"))
}

fn delivery_lines(delivery: &Path) -> Vec<Value> {
    std::fs::read_to_string(delivery)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("delivery line parses"))
        .collect()
}

fn wait_for_delivery_kind(delivery: &Path, kind: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        for line in delivery_lines(delivery) {
            if line["event_kind"] == kind {
                return line;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "{kind} never delivered; file: {:?}",
        std::fs::read_to_string(delivery).unwrap_or_default()
    );
}

/// Push one authoritative observation through the shipped #324 ingress seam.
fn push_bridge_snapshot(port: u16, snapshot: Value) -> Value {
    let (status, response) = request(port, "POST", "/api/gjc/bridge", Some(&snapshot));
    assert_eq!(status, 200, "bridge push failed: {response}");
    assert_eq!(
        response["ok"],
        Value::Bool(true),
        "bridge push rejected: {response}"
    );
    response
}

#[test]
fn full_loop_register_prompt_question_answer_complete_retire_restart() {
    // --- fake endpoint + isolated discovery worktree -----------------------
    // The repo's .gjc/state/sdk hosts live session metadata from real
    // runtime owners; discovery is newest-wins, so the daemon anchors to a
    // dedicated temp worktree where only this fixture's endpoint exists.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let server = FakeGjcServer::start().await;
        let temp = TempDir::new().unwrap();
        let worktree = temp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let _metadata = server.write_metadata(&worktree);
        let daemon_port = unused_port();
        let (config_path, delivery) = write_config(&temp, daemon_port);
        let daemon_log = temp.path().join("daemon.log");
        let mut daemon = spawn_daemon(&config_path, &daemon_log, Some(&worktree));
        wait_for_health(daemon_port);

        let session = "01a02ccd-c754-7656-95c7-f40b5a140bc3";

        // --- register the GJC lane (#325 store) ----------------------------
        let (status, registered) = request(
            daemon_port,
            "POST",
            "/api/gjc/lanes",
            Some(&json!({
                "sdk_session_id": session,
                "worktree": worktree.to_str().unwrap(),
            })),
        );
        assert_eq!(status, 200, "gjc lane registration failed: {registered}");
        let lane_id = registered["lane_id"].as_str().expect("lane id").to_string();

        // Harness-side interop probe: the fake endpoint must answer a
        // production-shaped session.get over raw ws before the daemon tries.
        {
            use futures_util::{SinkExt as _, StreamExt as _};
            use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
            let req = server.authenticated_url().into_client_request().unwrap();
            let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
            let _ = ws
                .send(tokio_tungstenite::tungstenite::Message::text(
                    serde_json::to_string(&common::gjc_wire::RequestFrame::query(
                        "probe-1",
                        "session.get",
                        json!({}),
                    ))
                    .unwrap(),
                ))
                .await;
            let mut saw_response = false;
            for _ in 0..4 {
                match tokio::time::timeout(Duration::from_secs(2), ws.next()).await {
                    Ok(Some(Ok(msg))) => {
                        if let tokio_tungstenite::tungstenite::Message::Text(text) = msg
                            && let Some(frame) =
                                common::gjc_wire::InboundFrame::decode(text.as_str())
                            && matches!(frame, common::gjc_wire::InboundFrame::Response(_))
                        {
                            saw_response = true;
                            break;
                        }
                    }
                    other => panic!("probe failed: {other:?}"),
                }
            }
            assert!(saw_response, "fake endpoint failed harness interop probe");
        }

        // --- submit prompt through the #323 control plane -------------------
        let (status, receipt) = request(
            daemon_port,
            "POST",
            "/api/gjc/prompt",
            Some(&json!({
                "session": session,
                "idempotency_key": "idem-prompt-326",
                "prompt": "fixed-fixture-prompt",
            })),
        );
        assert_eq!(
            status,
            200,
            "prompt submission failed: {receipt} (endpoint connections={})",
            server.connections_total().await
        );
        assert_eq!(
            receipt["status"], "acked",
            "prompt receipt must record acceptance: {receipt}"
        );

        // --- observe progress through the #324 bridge -----------------------
        let pushed = push_bridge_snapshot(
            daemon_port,
            json!({
                "session_id": session,
                "revision": 2,
                "turn": {"id": "fake-turn-1", "state": "active", "prompt_accepted": true},
                "prompt": {"command_id": "idem-prompt-326", "status": "accepted"},
                "summary": "turn running",
            }),
        );
        let emitted_kinds: Vec<&str> = pushed["emitted"]
            .as_array()
            .map(|events| {
                events
                    .iter()
                    .filter_map(|event| event["type"].as_str())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            emitted_kinds.contains(&"session.started")
                && emitted_kinds.contains(&"session.prompt-submitted"),
            "progress observation must emit lifecycle events: {pushed}"
        );

        // --- question episode -------------------------------------------------
        let pushed = push_bridge_snapshot(
            daemon_port,
            json!({
                "session_id": session,
                "revision": 3,
                "turn": {"id": "fake-turn-1", "state": "active"},
                "gate": {
                    "id": "gate-326",
                    "kind": "ask",
                    "revision": 1,
                    "status": "open",
                    "summary": "Deploy to staging?",
                },
            }),
        );
        assert!(
            pushed["emitted"]
                .as_array()
                .expect("emitted list")
                .iter()
                .any(|event| event["type"] == "workflow.question"),
            "ask gate opening must emit workflow.question: {pushed}"
        );
        let delivered = wait_for_delivery_kind(&delivery, "workflow.question");
        let rendered = serde_json::to_string(&delivered).unwrap();
        assert!(
            rendered.contains("Deploy to staging?") || rendered.contains("gate-326"),
            "delivered question must carry whitelisted identifiers: {rendered}"
        );
        assert!(
            !rendered.contains(FIXTURE_TOKEN),
            "token material leaked into delivery: {rendered}"
        );

        // --- answer the gate through the control plane -----------------------
        let (status, answer_receipt) = request(
            daemon_port,
            "POST",
            "/api/gjc/workflow-gate-answer",
            Some(&json!({
                "session": session,
                "idempotency_key": "idem-answer-326",
                "gate_id": "gate-326",
                "option": "yes",
            })),
        );
        assert_eq!(status, 200, "gate answer failed: {answer_receipt}");
        assert_eq!(
            answer_receipt["status"], "acked",
            "gate answer must be accepted by the endpoint: {answer_receipt}"
        );

        // --- completion ---------------------------------------------------------
        server.set_phase(FakePhase::Completed).await;
        // Authoritative full-section read through the discovery transport:
        // proves the #322/#323 query path against the live endpoint.
        let (status, query) = request(
            daemon_port,
            "GET",
            &format!("/api/gjc/session/{session}?sections=metadata,turn"),
            None,
        );
        assert_eq!(status, 200, "authoritative session read failed: {query}");

        let pushed = push_bridge_snapshot(
            daemon_port,
            json!({
                "session_id": session,
                "revision": 4,
                "turn": {"id": "fake-turn-1", "state": "complete"},
            }),
        );
        assert!(
            pushed["emitted"]
                .as_array()
                .expect("emitted list")
                .iter()
                .any(|event| event["type"] == "session.finished"),
            "turn completion must emit session.finished: {pushed}"
        );
        wait_for_delivery_kind(&delivery, "session.finished");

        // --- retire the lane ------------------------------------------------------
        let (status, retired) = request(
            daemon_port,
            "POST",
            &format!("/api/gjc/lanes/{lane_id}/retire"),
            Some(&json!({"reason": "e2e terminal retirement"})),
        );
        assert_eq!(status, 200, "lane retirement failed: {retired}");

        let deliveries_before_restart = delivery_lines(&delivery).len();

        // --- restart with no ghost --------------------------------------------------
        daemon.0.kill().expect("kill daemon");
        daemon.0.wait().expect("reap daemon");
        let mut restarted = spawn_daemon(&config_path, &daemon_log, Some(&worktree));
        wait_for_health(daemon_port);

        // Durable lane store reloads without resurrecting an active watch.
        let (status, lanes) = request(daemon_port, "GET", "/api/gjc/lanes?removed=true", None);
        assert_eq!(status, 200);
        let records = lanes["lanes"].as_array().cloned().unwrap_or_default();
        assert!(
            !records.is_empty(),
            "durable store must reload lanes: {lanes}"
        );
        let ours = records
            .iter()
            .filter(|record| record["sdk_session_id"] == session)
            .count();
        assert_eq!(
            ours, 1,
            "exactly one retained record, no ghost duplicate: {lanes}"
        );
        assert!(
            !records[0]["terminal_disposition"].is_null(),
            "retired disposition survives restart: {}",
            serde_json::to_string(&records).unwrap()
        );

        thread::sleep(Duration::from_millis(500));
        assert_eq!(
            delivery_lines(&delivery).len(),
            deliveries_before_restart,
            "ghost event replay detected after restart"
        );

        // Endpoint verdict evidence recorded by the fake endpoint.
        let resolved = server.resolved_controls().await;
        assert!(
            resolved
                .iter()
                .all(|(_, verdict)| *verdict == "control.accepted"),
            "every control verb reached the endpoint and was accepted: {resolved:?}"
        );

        restarted.0.kill().ok();
        restarted.0.wait().ok();
        server.stop().await;
    });
}

#[test]
fn two_sessions_isolate_endpoints_worktrees_revisions_receipts_and_alerts() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let session_a = "01a02ccd-c754-7656-95c7-f40b5a140bc3";
        let session_b = "01b02ccd-c754-7656-95c7-f40b5a140bc4";
        let script_a = FakeScript {
            session_id: session_a.into(),
            turn_id: "fake-turn-a".into(),
            gate_id: "gate-a".into(),
            question_title: "Approve lane A?".into(),
            question_options: vec!["yes".into(), "no".into()],
        };
        let script_b = FakeScript {
            session_id: session_b.into(),
            turn_id: "fake-turn-b".into(),
            gate_id: "gate-b".into(),
            question_title: "Approve lane B?".into(),
            question_options: vec!["yes".into(), "no".into()],
        };
        let server_a = FakeGjcServer::start_with(script_a).await;
        let server_b = FakeGjcServer::start_with(script_b).await;
        let temp = TempDir::new().unwrap();
        let worktree_a = temp.path().join("worktree-a");
        let worktree_b = temp.path().join("worktree-b");
        let daemon_root = temp.path().join("daemon-root");
        std::fs::create_dir_all(&worktree_a).unwrap();
        std::fs::create_dir_all(&worktree_b).unwrap();
        std::fs::create_dir_all(&daemon_root).unwrap();
        let metadata_a = server_a.write_metadata(&worktree_a);
        let metadata_b = server_b.write_metadata(&worktree_b);
        let metadata_a_value: Value = serde_json::from_str(
            &std::fs::read_to_string(&metadata_a).expect("read lane A metadata"),
        )
        .unwrap();
        let metadata_b_value: Value = serde_json::from_str(
            &std::fs::read_to_string(&metadata_b).expect("read lane B metadata"),
        )
        .unwrap();
        assert_eq!(metadata_a_value["sessionId"], session_a);
        assert_eq!(metadata_b_value["sessionId"], session_b);
        assert_eq!(metadata_a_value["url"], server_a.metadata_url());
        assert_eq!(metadata_b_value["url"], server_b.metadata_url());
        assert_ne!(metadata_a_value["url"], metadata_b_value["url"]);

        let delivery_a = temp.path().join("delivery-a.jsonl");
        let delivery_b = temp.path().join("delivery-b.jsonl");
        let daemon_port = unused_port();
        let config_path = write_two_lane_config(
            &temp,
            daemon_port,
            &worktree_a,
            &delivery_a,
            &worktree_b,
            &delivery_b,
        );
        let daemon_log = temp.path().join("two-lane-daemon.log");
        let mut daemon = spawn_daemon(&config_path, &daemon_log, Some(&daemon_root));
        wait_for_health(daemon_port);
        let deadline = Instant::now() + Duration::from_secs(10);
        let records = loop {
            let (_, lanes) = request(daemon_port, "GET", "/api/gjc/lanes", None);
            let records = lanes["lanes"].as_array().cloned().unwrap_or_default();
            if records.iter().any(|r| r["sdk_session_id"] == session_a)
                && records.iter().any(|r| r["sdk_session_id"] == session_b)
            {
                break records;
            }
            assert!(
                Instant::now() < deadline,
                "automatic enrollment failed: {lanes}"
            );
            thread::sleep(Duration::from_millis(50));
        };
        let lane_a = records
            .iter()
            .find(|r| r["sdk_session_id"] == session_a)
            .and_then(|r| r["lane_id"].as_str())
            .unwrap()
            .to_string();
        let lane_b = records
            .iter()
            .find(|r| r["sdk_session_id"] == session_b)
            .and_then(|r| r["lane_id"].as_str())
            .unwrap()
            .to_string();
        assert_ne!(lane_a, lane_b);

        let prompt_key_a = "idem-two-session-a";
        let prompt_key_b = "idem-two-session-b";
        let (status, receipt_a) = request(
            daemon_port,
            "POST",
            "/api/gjc/prompt",
            Some(&json!({
                "session": session_a,
                "idempotency_key": prompt_key_a,
                "prompt": "fixed lane A prompt",
            })),
        );
        assert_eq!(status, 200, "lane A prompt failed: {receipt_a}");
        assert_eq!(receipt_a["status"], "acked");
        assert_eq!(receipt_a["session_id"], session_a);
        assert_eq!(receipt_a["idempotency_key"], prompt_key_a);
        let (status, receipt_b) = request(
            daemon_port,
            "POST",
            "/api/gjc/prompt",
            Some(&json!({
                "session": session_b,
                "idempotency_key": prompt_key_b,
                "prompt": "fixed lane B prompt",
            })),
        );
        assert_eq!(status, 200, "lane B prompt failed: {receipt_b}");
        assert_eq!(receipt_b["status"], "acked");
        assert_eq!(receipt_b["session_id"], session_b);
        assert_eq!(receipt_b["idempotency_key"], prompt_key_b);
        assert_ne!(receipt_a["command_id"], receipt_b["command_id"]);

        server_a.set_phase(FakePhase::Running).await;
        server_b.set_phase(FakePhase::Running).await;
        thread::sleep(Duration::from_secs(5));
        server_a.set_phase(FakePhase::Question).await;
        server_b.set_phase(FakePhase::Question).await;
        thread::sleep(Duration::from_secs(5));

        let answer_key_a = "idem-two-answer-a";
        let answer_key_b = "idem-two-answer-b";
        let (status, answer_a) = request(
            daemon_port,
            "POST",
            "/api/gjc/workflow-gate-answer",
            Some(&json!({
                "session": session_a,
                "idempotency_key": answer_key_a,
                "gate_id": "gate-a",
                "option": "yes",
            })),
        );
        assert_eq!(status, 200, "lane A answer failed: {answer_a}");
        assert_eq!(answer_a["status"], "acked");
        assert_eq!(answer_a["session_id"], session_a);
        let (status, answer_b) = request(
            daemon_port,
            "POST",
            "/api/gjc/workflow-gate-answer",
            Some(&json!({
                "session": session_b,
                "idempotency_key": answer_key_b,
                "gate_id": "gate-b",
                "option": "yes",
            })),
        );
        assert_eq!(status, 200, "lane B answer failed: {answer_b}");
        assert_eq!(answer_b["status"], "acked");
        assert_eq!(answer_b["session_id"], session_b);

        // Both lanes now resume independently after their scoped answers.
        server_a.set_phase(FakePhase::Completed).await;
        server_b.set_phase(FakePhase::Completed).await;
        let (status, query_a) = request(
            daemon_port,
            "GET",
            &format!("/api/gjc/session/{session_a}?sections=metadata,turn"),
            None,
        );
        assert_eq!(status, 200, "lane A query failed: {query_a}");
        assert_eq!(query_a["metadata"]["session_id"], session_a);
        assert_eq!(query_a["turn"]["turn_id"], "fake-turn-a");
        let (status, query_b) = request(
            daemon_port,
            "GET",
            &format!("/api/gjc/session/{session_b}?sections=metadata,turn"),
            None,
        );
        assert_eq!(status, 200, "lane B query failed: {query_b}");
        assert_eq!(query_b["metadata"]["session_id"], session_b);
        assert_eq!(query_b["turn"]["turn_id"], "fake-turn-b");

        server_a.set_phase(FakePhase::Completed).await;
        server_b.set_phase(FakePhase::Completed).await;
        thread::sleep(Duration::from_secs(5));

        // Receipt lookup is session-bound: the other lane can never observe
        // this lane's command journal entry.
        let (status, lookup_a) = request(
            daemon_port,
            "GET",
            &format!("/api/gjc/command/{prompt_key_a}?session={session_a}"),
            None,
        );
        assert_eq!(status, 200);
        assert_eq!(lookup_a["session_id"], session_a);
        let (status, wrong_lookup) = request(
            daemon_port,
            "GET",
            &format!("/api/gjc/command/{prompt_key_a}?session={session_b}"),
            None,
        );
        assert_eq!(status, 400, "cross-lane receipt leaked: {wrong_lookup}");
        let (status, lookup_b) = request(
            daemon_port,
            "GET",
            &format!("/api/gjc/command/{prompt_key_b}?session={session_b}"),
            None,
        );
        assert_eq!(status, 200);
        assert_eq!(lookup_b["session_id"], session_b);

        let record_a = request(
            daemon_port,
            "GET",
            &format!("/api/gjc/lanes/{lane_a}"),
            None,
        );
        assert_eq!(record_a.0, 200);
        assert_eq!(record_a.1["sdk_session_id"], session_a);
        assert_eq!(record_a.1["worktree"], worktree_a.to_str().unwrap());
        assert_eq!(record_a.1["sdk_revision"], 4);
        let record_b = request(
            daemon_port,
            "GET",
            &format!("/api/gjc/lanes/{lane_b}"),
            None,
        );
        assert_eq!(record_b.0, 200);
        assert_eq!(record_b.1["sdk_session_id"], session_b);
        assert_eq!(record_b.1["worktree"], worktree_b.to_str().unwrap());
        assert_eq!(record_b.1["sdk_revision"], 4);

        wait_for_delivery_kind(&delivery_a, "workflow.question");
        wait_for_delivery_kind(&delivery_b, "workflow.question");

        let worktree_a_text = worktree_a.to_str().unwrap();
        let worktree_b_text = worktree_b.to_str().unwrap();
        for line in delivery_lines(&delivery_a) {
            assert_eq!(line["summary_payload"]["session_id"], session_a);
            if !line["summary_payload"]["repo_path"].is_null() {
                assert_eq!(line["summary_payload"]["repo_path"], worktree_a_text);
            }
            assert!(!line.to_string().contains(session_b));
            assert!(!line.to_string().contains(worktree_b_text));
        }
        for line in delivery_lines(&delivery_b) {
            assert_eq!(line["summary_payload"]["session_id"], session_b);
            if !line["summary_payload"]["repo_path"].is_null() {
                assert_eq!(line["summary_payload"]["repo_path"], worktree_b_text);
            }
            assert!(!line.to_string().contains(session_a));
            assert!(!line.to_string().contains(worktree_a_text));
        }

        let controls_a = server_a.control_requests().await;
        let controls_b = server_b.control_requests().await;
        assert!(
            controls_a
                .iter()
                .all(|(_, session, _)| session == session_a)
        );
        assert!(
            controls_b
                .iter()
                .all(|(_, session, _)| session == session_b)
        );
        assert!(
            controls_a
                .iter()
                .any(|(operation, _, key)| operation == "prompt" && key == prompt_key_a)
        );
        assert!(controls_a.iter().any(|(operation, _, key)| {
            operation == "workflow_gate_answer" && key == answer_key_a
        }));
        assert!(
            controls_b
                .iter()
                .any(|(operation, _, key)| operation == "prompt" && key == prompt_key_b)
        );
        assert!(controls_b.iter().any(|(operation, _, key)| {
            operation == "workflow_gate_answer" && key == answer_key_b
        }));
        assert!(
            controls_a
                .iter()
                .all(|(_, session, _)| session != session_b)
        );
        assert!(
            controls_b
                .iter()
                .all(|(_, session, _)| session != session_a)
        );

        daemon.0.kill().ok();
        daemon.0.wait().ok();
        server_a.stop().await;
        server_b.stop().await;
    });
}

const FIXTURE_TOKEN: &str = "fixture-token-326";
