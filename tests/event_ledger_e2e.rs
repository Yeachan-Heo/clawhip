use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
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
    TcpListener::bind(("127.0.0.1", 0))
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

fn wait_for_ledger_status(port: u16, expected_duplicates: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let (status, body) = request(port, "GET", "/api/ledger/status", None);
        if status == 200 {
            let value: Value = serde_json::from_slice(&body).unwrap();
            if value["records"] == 1 && value["duplicates"] == expected_duplicates {
                return value;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("ledger status did not converge");
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
compaction_interval_secs = 1
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

fn spawn_daemon(config: &Path, temp: &TempDir, port: u16) -> DaemonGuard {
    let stdout = File::create(temp.path().join("daemon.stdout")).unwrap();
    let stderr = File::create(temp.path().join("daemon.stderr")).unwrap();
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

#[test]
fn daemon_appends_before_delivery_and_exposes_bounded_indexed_query() {
    let temp = TempDir::new().unwrap();
    let port = unused_port();
    let config = write_config(&temp, port);
    let _daemon = spawn_daemon(&config, &temp, port);
    wait_for_health(port);

    let event = serde_json::to_vec(&json!({
        "type": "agent.finished",
        "payload": {
            "agent_name": "ledger-e2e",
            "status": "finished",
            "event_id": "issue-304-e2e",
            "project": "Yeachan-Heo/clawhip",
            "worktree": "/tmp/clawhip-issue-304",
            "session_id": "session-304",
            "keywords": ["ledger", "verified"],
            "source_url": "https://github.com/Yeachan-Heo/clawhip/issues/304"
        }
    }))
    .unwrap();

    for _ in 0..2 {
        let (status, body) = request(port, "POST", "/event", Some(&event));
        assert_eq!(status, 202, "{}", String::from_utf8_lossy(&body));
    }
    let status = wait_for_ledger_status(port, 1);
    assert_eq!(status["appended"], 1);

    let output = Command::new(env!("CARGO_BIN_EXE_clawhip"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "ledger",
            "query",
            "--repo",
            "Yeachan-Heo/clawhip",
            "--worktree",
            "/tmp/clawhip-issue-304",
            "--session-id",
            "session-304",
            "--event-type",
            "session.finished",
            "--keywords",
            "ledger,verified",
            "--limit",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let query: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(query["records"].as_array().unwrap().len(), 1);
    assert_eq!(query["records"][0]["event_type"], "session.finished");
    assert_eq!(query["records"][0]["repo"], "Yeachan-Heo/clawhip");
    assert!(query["records"][0].get("summary").is_none());

    let delivery = std::fs::read_to_string(temp.path().join("delivery.jsonl")).unwrap();
    assert_eq!(delivery.lines().count(), 1);
}
