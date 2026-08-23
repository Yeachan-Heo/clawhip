//! End-to-end boundary verification for the GJC SDK event bridge (#324):
//! replay, duplicate, out-of-order/stale suppression, and daemon-restart
//! dedupe through the real HTTP ingress, dispatcher, event ledger, and
//! local-file delivery pipeline.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

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
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn request(port: u16, method: &str, path: &str, body: Option<&[u8]>) -> (u16, Vec<u8>) {
    let body = body.unwrap_or_default();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let headers = std::str::from_utf8(&response[..split]).unwrap();
    let status = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, response[split + 4..].to_vec())
}

fn post_snapshot(port: u16, snapshot: &Value) -> Value {
    let (status, body) = request(
        port,
        "POST",
        "/api/gjc/bridge",
        Some(serde_json::to_vec(snapshot).unwrap().as_slice()),
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

fn wait_for_health(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            let (status, _) = request(port, "GET", "/health", None);
            if status == 200 {
                return;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("daemon did not become healthy on port {port}");
}

fn wait_for_ledger_and_delivery(
    port: u16,
    expected_records: u64,
    expected_duplicates: u64,
    delivery_path: &std::path::Path,
    expected_deliveries: usize,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last: Option<(Value, usize)> = None;
    while Instant::now() < deadline {
        let (status, body) = request(port, "GET", "/api/ledger/status", None);
        if status == 200 {
            let value: Value = serde_json::from_slice(&body).unwrap();
            let deliveries = std::fs::read_to_string(delivery_path)
                .map(|content| content.lines().count())
                .unwrap_or(0);
            last = Some((value.clone(), deliveries));
            if value["records"] == expected_records
                && value["duplicates"] == expected_duplicates
                && deliveries == expected_deliveries
            {
                return;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "ledger/delivery state did not converge (expected records={expected_records}, duplicates={expected_duplicates}, deliveries={expected_deliveries}); observed {last:?}"
    );
}

fn write_config(temp: &TempDir, port: u16) -> std::path::PathBuf {
    let config = temp.path().join("clawhip.toml");
    let ledger = temp.path().join("ledger");
    let delivery = temp.path().join("delivery.jsonl");
    std::fs::write(
        &config,
        format!(
            r#"[daemon]
bind_host = "127.0.0.1"
port = {port}
base_url = "http://127.0.0.1:{port}"

[ledger]
enabled = true
path = "{}"
raw_retention_days = 7
summary_retention_days = 30
compaction_interval_secs = 3600
max_records = 1000
max_record_bytes = 4096
max_keywords = 8
max_keyword_bytes = 32
max_query_results = 50
max_records_per_compaction = 100

[[routes]]
event = "*"
sink = "localfile"
local_path = "{}"
"#,
            ledger.display(),
            delivery.display()
        ),
    )
    .unwrap();
    config
}

fn spawn_daemon(config: &std::path::Path, temp: &TempDir, port: u16, tag: &str) -> DaemonGuard {
    let stdout = File::create(temp.path().join(format!("daemon-{tag}.stdout"))).unwrap();
    let stderr = File::create(temp.path().join(format!("daemon-{tag}.stderr"))).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_clawhip"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "start",
            "--port",
            &port.to_string(),
        ])
        .current_dir(temp.path())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap();
    DaemonGuard(child)
}

use std::fs::File;

fn snapshot(revision: u64) -> Value {
    json!({
        "session_id": "sess-324",
        "revision": revision,
        "repo_name": "Yeachan-Heo/clawhip",
        "worktree_path": "/wt/issue-324",
        "branch": "feat/issue-324",
        "observed_at": "2026-08-23T00:00:00Z",
        "turn": {"id": "t1", "state": "active", "attempt": 0},
        "prompt": {"command_id": "c1", "status": "accepted"}
    })
}

#[test]
fn gjc_bridge_ingest_dedupes_replay_stale_and_restart_boundaries() {
    let temp = TempDir::new().unwrap();
    let delivery_path = temp.path().join("delivery.jsonl");

    // --- First daemon run: live progression -------------------------------
    let port_a = unused_port();
    let config = write_config(&temp, port_a);
    let _daemon_a = spawn_daemon(&config, &temp, port_a, "a");
    wait_for_health(port_a);

    // Prompt acceptance + activation emit lifecycle evidence once.
    let response = post_snapshot(port_a, &snapshot(1));
    let emitted: Vec<String> = response["emitted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["type"].as_str().unwrap().to_string())
        .collect();
    assert!(
        emitted.contains(&"session.started".to_string()),
        "{response}"
    );
    assert!(
        emitted.contains(&"session.prompt-submitted".to_string()),
        "{response}"
    );

    // Exact replay at the same revision is a duplicate with no emissions.
    let response = post_snapshot(port_a, &snapshot(1));
    assert_eq!(response["duplicate"], json!(true));
    assert_eq!(response["emitted"], json!([]));

    // Out-of-order/stale snapshot below the watermark is suppressed.
    let mut stale = snapshot(1);
    stale["revision"] = json!(0);
    let response = post_snapshot(port_a, &stale);
    assert_eq!(response["stale"], json!(true));
    assert_eq!(response["emitted"], json!([]));

    // Malformed snapshots fail closed without emitting anything.
    let (status, _) = request(
        port_a,
        "POST",
        "/api/gjc/bridge",
        Some(br#"{"revision": 9}"#.as_slice()),
    );
    assert_eq!(status, 400);

    // Question gate carries stable identifiers through to the ledger surface.
    let mut question = snapshot(2);
    question["turn"] = json!({"id": "t1", "state": "waiting_input", "attempt": 0});
    question["gate"] = json!({
        "id": "q-1",
        "kind": "ask",
        "revision": 4,
        "status": "open",
        "summary": "Ship the release?"
    });
    let response = post_snapshot(port_a, &question);
    let emitted: Vec<String> = response["emitted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["type"].as_str().unwrap().to_string())
        .collect();
    assert!(
        emitted.contains(&"workflow.question".to_string()),
        "{response}"
    );

    // Completion closes the turn.
    let mut completion = snapshot(3);
    completion["turn"] = json!({"id": "t1", "state": "complete", "attempt": 0});
    completion["prompt"] = json!(null);
    completion["gate"] = json!({
        "id": "q-1",
        "kind": "ask",
        "revision": 4,
        "status": "resolved"
    });
    let response = post_snapshot(port_a, &completion);
    let emitted: Vec<String> = response["emitted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["type"].as_str().unwrap().to_string())
        .collect();
    assert!(
        emitted.contains(&"session.finished".to_string()),
        "{response}"
    );

    // Four unique events reached ledger + delivery exactly once each:
    // started + prompt-submitted share snapshot 1, then question, then finish.
    // The bridge-level replay never reached the ledger, so no ledger
    // duplicates exist on the first daemon.
    let expected_events = 4usize;
    wait_for_ledger_and_delivery(
        port_a,
        expected_events as u64,
        0,
        &delivery_path,
        expected_events,
    );

    let query: Value = serde_json::from_slice(
        request(
            port_a,
            "GET",
            "/api/ledger/query?session_id=sess-324&limit=50",
            None,
        )
        .1
        .as_slice(),
    )
    .unwrap();
    let types: Vec<String> = query["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["event_type"].as_str().unwrap().to_string())
        .collect();
    assert!(
        types.contains(&"workflow.question".to_string()),
        "{types:?}"
    );
    assert!(types.contains(&"session.finished".to_string()), "{types:?}");

    drop(_daemon_a);

    // --- Restart: fresh daemon replays the identical authoritative feed ---
    let port_b = unused_port();
    let config_b = write_config(&temp, port_b);
    let _daemon_b = spawn_daemon(&config_b, &temp, port_b, "b");
    wait_for_health(port_b);

    let mut stale_replay = snapshot(1);
    stale_replay["revision"] = json!(0);
    let steps: Vec<(Value, &str)> = vec![
        (snapshot(1), "emit"),
        (snapshot(1), "duplicate"),
        (stale_replay, "stale"),
        (question.clone(), "emit"),
        (completion.clone(), "emit"),
    ];
    for (replay, expected) in steps {
        let response = post_snapshot(port_b, &replay);
        assert_eq!(response["ok"], json!(true), "{response}");
        match expected {
            "emit" => {
                // A fresh bridge legitimately re-emits every transition; the
                // deterministic event ids must make the ledger swallow them.
                assert!(
                    !response["emitted"].as_array().unwrap().is_empty(),
                    "{response}"
                );
            }
            other => {
                assert_eq!(
                    response[if other == "stale" {
                        "stale"
                    } else {
                        "duplicate"
                    }],
                    json!(true),
                    "{response}"
                );
                assert_eq!(response["emitted"], json!([]), "{response}");
            }
        }
    }

    // Ledger records and deliveries stay untouched: deterministic event ids
    // made all five restarted emissions ledger-level duplicates.
    wait_for_ledger_and_delivery(
        port_b,
        expected_events as u64,
        expected_events as u64,
        &delivery_path,
        expected_events,
    );

    // Bridge observability counters stay visible on the ingress surface.
    let response = post_snapshot(port_b, &completion);
    assert_eq!(response["duplicate"], json!(true));
    let totals = response["totals"].clone();
    assert!(totals["snapshots"].as_u64().unwrap() >= 6, "{totals}");
    assert!(totals["duplicates"].as_u64().unwrap() >= 2, "{totals}");
    assert!(totals["stale"].as_u64().unwrap() >= 1, "{totals}");
}
