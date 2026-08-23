//! Integration fixtures for the worktree-local GJC SDK transport (#322/#328).
//!
//! Covers discovery (unavailable, stale, stale-flagged, filename mismatch,
//! unsupported record version, malformed, newest-wins), the filesystem trust
//! boundary (symlink/permission/path validation), and the repaired SDK v3
//! wire contract against a real loopback websocket fixture server: strict
//! hello gating, pre-hello ordering, typed correlated responses, typed
//! application errors, bounded reconnect with re-authentication, and
//! redacted diagnostics. The transport API is exercised black-box through
//! `CARGO_BIN_EXE_clawhip gjc inspect --probe` plus an in-process server.

use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::{TcpListener as AsyncTcpListener, TcpStream as AsyncTcpStream};
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_clawhip")
}

fn unused_port() -> u16 {
    TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn metadata_json(url: &str, token: &str, pid: Option<u32>) -> String {
    let pid_field = pid.map(|pid| format!(",\"pid\":{pid}")).unwrap_or_default();
    format!(
        "{{\"version\":1,\"sessionId\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"url\":\"{url}\",\"token\":\"{token}\"{pid_field}}}"
    )
}

fn write_metadata(state_root: &Path, contents: &str) -> PathBuf {
    write_metadata_as(
        state_root,
        "01a02ccd-c754-7656-95c7-f40b5a140bc3.json",
        contents,
    )
}

fn write_metadata_as(state_root: &Path, file_name: &str, contents: &str) -> PathBuf {
    let sdk_dir = state_root.join("sdk");
    std::fs::create_dir_all(&sdk_dir).unwrap();
    let path = sdk_dir.join(file_name);
    std::fs::write(&path, contents).unwrap();
    restrict_owner_only(&path);
    path
}

#[cfg(unix)]
fn restrict_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[cfg(not(unix))]
fn restrict_owner_only(_: &Path) {}

fn inspect(worktree: &Path) -> Output {
    Command::new(bin())
        .args(["gjc", "inspect"])
        .arg("--worktree")
        .arg(worktree)
        .arg("--json")
        .output()
        .expect("run clawhip gjc inspect")
}

fn inspect_stdout(worktree: &Path) -> Value {
    let output = inspect(worktree);
    assert!(
        output.status.success(),
        "inspect failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse inspect json")
}

fn probe_stdout(worktree: &Path) -> Value {
    let output = Command::new(bin())
        .args(["gjc", "inspect", "--probe"])
        .arg("--worktree")
        .arg(worktree)
        .arg("--json")
        .output()
        .expect("run probe");
    assert!(
        output.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse probe json")
}

fn state_root(worktree: &Path) -> PathBuf {
    worktree.join(".gjc").join("state")
}

// ---------------------------------------------------------------------------
// Discovery cases
// ---------------------------------------------------------------------------

#[test]
fn success_discovers_live_endpoint_without_leaking_secrets() {
    let temp = TempDir::new().unwrap();
    let worktree = temp.path().join("worktree");
    std::fs::create_dir_all(state_root(&worktree)).unwrap();
    write_metadata(
        &state_root(&worktree),
        &metadata_json(
            "ws://127.0.0.1:1/",
            "tok_secret_value",
            Some(std::process::id()),
        ),
    );
    let snapshot = inspect_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("live"));
    assert_eq!(
        snapshot["session_id"],
        json!("01a02ccd-c754-7656-95c7-f40b5a140bc3")
    );
    assert_eq!(snapshot["pid"], json!(std::process::id()));
    let encoded = snapshot.to_string();
    assert!(
        !encoded.contains("tok_secret_value"),
        "token leaked: {encoded}"
    );
    assert!(
        !encoded.contains("127.0.0.1"),
        "endpoint url leaked: {encoded}"
    );
}

#[test]
fn unavailable_when_no_metadata_exists() {
    let temp = TempDir::new().unwrap();
    let worktree = temp.path().join("empty");
    std::fs::create_dir_all(state_root(&worktree).join("sdk")).unwrap();
    let snapshot = inspect_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("unavailable"));
    assert_eq!(
        snapshot["diagnostic"]["reason"],
        json!("endpoint_unavailable")
    );
}

#[test]
fn stale_when_owning_process_is_dead() {
    let temp = TempDir::new().unwrap();
    let worktree = temp.path().join("worktree");
    std::fs::create_dir_all(state_root(&worktree)).unwrap();
    write_metadata(
        &state_root(&worktree),
        &metadata_json("ws://127.0.0.1:1/", "tok_abc123", Some(0xFFFF_FFF0u32)),
    );
    let snapshot = inspect_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("stale"));
    assert_eq!(
        snapshot["session_id"],
        json!("01a02ccd-c754-7656-95c7-f40b5a140bc3")
    );
    assert_eq!(snapshot["diagnostic"]["reason"], json!("endpoint_stale"));
}

#[test]
fn stale_flag_fences_even_a_live_pid() {
    let temp = TempDir::new().unwrap();
    let worktree = temp.path().join("worktree");
    std::fs::create_dir_all(state_root(&worktree)).unwrap();
    let flagged = format!(
        "{{\"version\":1,\"sessionId\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"url\":\"ws://127.0.0.1:1/\",\"token\":\"tok_abc123\",\"pid\":{},\"stale\":true}}",
        std::process::id()
    );
    write_metadata(&state_root(&worktree), &flagged);
    let snapshot = inspect_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("stale"), "{snapshot}");
    assert_eq!(snapshot["diagnostic"]["reason"], json!("endpoint_stale"));
}

#[test]
fn filename_identity_mismatch_fails_discovery_closed() {
    let temp = TempDir::new().unwrap();
    let worktree = temp.path().join("worktree");
    std::fs::create_dir_all(state_root(&worktree)).unwrap();
    // Payload claims a session its filename does not authorize.
    write_metadata_as(
        &state_root(&worktree),
        "01a00000-0000-0000-0000-000000000042.json",
        &metadata_json("ws://127.0.0.1:1/", "tok_abc123", Some(std::process::id())),
    );
    let snapshot = inspect_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("malformed"), "{snapshot}");
    assert_eq!(
        snapshot["diagnostic"]["reason"],
        json!("endpoint_malformed")
    );
}

#[test]
fn unsupported_or_missing_record_version_is_malformed() {
    for (name, version_field) in [
        ("unsupported version", "\"version\":2,"),
        ("zero version", "\"version\":0,"),
        ("missing version", ""),
    ] {
        let temp = TempDir::new().unwrap();
        let worktree = temp.path().join("worktree");
        std::fs::create_dir_all(state_root(&worktree)).unwrap();
        let body = format!(
            "{{{version_field}\"sessionId\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"url\":\"ws://127.0.0.1:1/\",\"token\":\"tok_abc123\",\"pid\":{}}}",
            std::process::id()
        );
        write_metadata(&state_root(&worktree), &body);
        let snapshot = inspect_stdout(&worktree);
        assert_eq!(
            snapshot["status"],
            json!("malformed"),
            "case {name}: {snapshot}"
        );
    }
}

#[test]
fn malformed_metadata_is_rejected() {
    let temp = TempDir::new().unwrap();
    let worktree = temp.path().join("worktree");
    std::fs::create_dir_all(state_root(&worktree)).unwrap();

    let cases: Vec<(&str, String)> = vec![
        ("non-json", "not json at all".to_string()),
        ("missing fields", "{\"version\":1}".to_string()),
        (
            "remote host",
            metadata_json("ws://10.0.0.5:1234/", "tok_abc123", None),
        ),
        (
            "wss scheme",
            metadata_json("wss://example.com/", "tok_abc123", None),
        ),
        (
            "url credentials",
            metadata_json("ws://user:pass@127.0.0.1:1/", "tok_abc123", None),
        ),
        (
            "url query",
            metadata_json("ws://127.0.0.1:1/?x=1", "tok_abc123", None),
        ),
        (
            "url fragment",
            metadata_json("ws://127.0.0.1:1/#frag", "tok_abc123", None),
        ),
        (
            "bad session id",
            "{\"version\":1,\"session_id\":\"../escape\",\"url\":\"ws://127.0.0.1:1/\",\"token\":\"t\"}"
                .to_string(),
        ),
        (
            "bad token charset",
            "{\"version\":1,\"sessionId\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"url\":\"ws://127.0.0.1:1/\",\"token\":\"has spaces\"}"
                .to_string(),
        ),
        ("host not ip", metadata_json("ws://localhost:1/", "tok_abc123", None)),
        (
            "oversized file",
            format!("{{\"padding\":\"{}\"}}", "x".repeat(8192)),
        ),
    ];
    for (name, contents) in cases {
        let _ = std::fs::remove_dir_all(state_root(&worktree).join("sdk"));
        write_metadata(&state_root(&worktree), &contents);
        let snapshot = inspect_stdout(&worktree);
        assert_eq!(
            snapshot["status"],
            json!("malformed"),
            "case {name}: {snapshot}"
        );
        assert_eq!(
            snapshot["diagnostic"]["reason"],
            json!("endpoint_malformed"),
            "case {name}: {snapshot}"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlinked_metadata_and_permissive_files_are_rejected() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let worktree = temp.path().join("worktree");
    std::fs::create_dir_all(state_root(&worktree).join("sdk")).unwrap();

    // Symlinked metadata file: skipped entirely.
    let outside = temp.path().join("outside.json");
    std::fs::write(
        &outside,
        metadata_json("ws://127.0.0.1:1/", "tok_abc123", None),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        &outside,
        state_root(&worktree).join("sdk").join("linked.json"),
    )
    .unwrap();
    let snapshot = inspect_stdout(&worktree);
    assert_eq!(
        snapshot["status"],
        json!("malformed"),
        "symlinked metadata entry must not yield a live endpoint"
    );

    // Permissive (group/world readable) metadata: skipped.
    std::fs::remove_file(state_root(&worktree).join("sdk").join("linked.json")).unwrap();
    let permissive = write_metadata(
        &state_root(&worktree),
        &metadata_json("ws://127.0.0.1:1/", "tok_abc123", None),
    );
    let mut permissions = std::fs::metadata(&permissive).unwrap().permissions();
    permissions.set_mode(0o644);
    std::fs::set_permissions(&permissive, permissions).unwrap();
    let snapshot = inspect_stdout(&worktree);
    assert_eq!(
        snapshot["status"],
        json!("malformed"),
        "permissive metadata must never yield a live endpoint"
    );

    // Symlinked state-root directory: rejected as malformed.
    let outside_dir = temp.path().join("outside-state");
    std::fs::create_dir_all(&outside_dir).unwrap();
    let other = temp.path().join("other");
    std::fs::create_dir_all(other.join(".gjc")).unwrap();
    std::os::unix::fs::symlink(&outside_dir, other.join(".gjc").join("state")).unwrap();
    let snapshot = inspect_stdout(&other);
    assert_eq!(snapshot["status"], json!("malformed"));
    assert_eq!(
        snapshot["diagnostic"]["reason"],
        json!("endpoint_malformed")
    );

    // World-writable sdk dir: rejected as malformed. (Group-writable dirs are
    // tolerated because umask 002 is the common Linux default; the strict
    // owner-only policy applies to token-bearing metadata files.)
    let strict = temp.path().join("strict");
    let sdk_dir = state_root(&strict).join("sdk");
    std::fs::create_dir_all(&sdk_dir).unwrap();
    let mut dir_permissions = std::fs::metadata(&sdk_dir).unwrap().permissions();
    dir_permissions.set_mode(0o777);
    std::fs::set_permissions(&sdk_dir, dir_permissions).unwrap();
    write_metadata(
        &state_root(&strict),
        &metadata_json("ws://127.0.0.1:1/", "t", None),
    );
    let snapshot = inspect_stdout(&strict);
    assert_eq!(snapshot["status"], json!("malformed"));
    assert_eq!(
        snapshot["diagnostic"]["reason"],
        json!("endpoint_malformed")
    );

    // Group-writable sdk dir with valid metadata stays live.
    let lenient = temp.path().join("lenient");
    let lenient_sdk = state_root(&lenient).join("sdk");
    std::fs::create_dir_all(&lenient_sdk).unwrap();
    let mut lenient_permissions = std::fs::metadata(&lenient_sdk).unwrap().permissions();
    lenient_permissions.set_mode(0o770);
    std::fs::set_permissions(&lenient_sdk, lenient_permissions).unwrap();
    write_metadata(
        &state_root(&lenient),
        &metadata_json("ws://127.0.0.1:2/", "t", Some(std::process::id())),
    );
    let snapshot = inspect_stdout(&lenient);
    assert_eq!(
        snapshot["status"],
        json!("live"),
        "group-writable sdk dir must not break discovery"
    );
}

#[test]
fn discovery_reads_only_the_lane_state_root() {
    let temp = TempDir::new().unwrap();
    let target = temp.path().join("target");
    let unrelated = temp.path().join("unrelated");
    std::fs::create_dir_all(state_root(&target)).unwrap();
    std::fs::create_dir_all(state_root(&unrelated)).unwrap();
    write_metadata(
        &state_root(&unrelated),
        &metadata_json("ws://127.0.0.1:1/", "tok_other", None),
    );
    let snapshot = inspect_stdout(&target);
    assert_eq!(
        snapshot["status"],
        json!("unavailable"),
        "unrelated metadata leaked"
    );
}

#[test]
fn newest_live_metadata_wins_over_older_stale_metadata() {
    let temp = TempDir::new().unwrap();
    let worktree = temp.path().join("worktree");
    let sdk_dir = state_root(&worktree).join("sdk");
    std::fs::create_dir_all(&sdk_dir).unwrap();

    // Stale (dead pid) file first, then a fresh live file that is newer.
    let stale_path = sdk_dir.join("01a00000-0000-0000-0000-000000000001.json");
    let stale_body = metadata_json("ws://127.0.0.1:1/", "tok_old", Some(0xFFFF_FFF1u32)).replace(
        "01a02ccd-c754-7656-95c7-f40b5a140bc3",
        "01a00000-0000-0000-0000-000000000001",
    );
    std::fs::write(&stale_path, stale_body).unwrap();
    restrict_owner_only(&stale_path);
    std::thread::sleep(Duration::from_millis(20));
    write_metadata(
        &state_root(&worktree),
        &metadata_json("ws://127.0.0.1:2/", "tok_new", Some(std::process::id())),
    );
    let snapshot = inspect_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("live"));
    assert_eq!(
        snapshot["session_id"],
        json!("01a02ccd-c754-7656-95c7-f40b5a140bc3")
    );
}

// ---------------------------------------------------------------------------
// SDK v3 transport cases against a real loopback websocket fixture server
// ---------------------------------------------------------------------------

/// Wire behaviors the v3 fixture server can exhibit per connection.
#[derive(Clone, Copy, PartialEq)]
enum ServerBehavior {
    /// Correct token: hello first, then correlated typed responses.
    Authenticated,
    /// Reject every handshake (401).
    RejectAuth,
    /// Accept handshake, hello, then never answer requests.
    Silent,
    /// Accept handshake, hello, then reply with a foreign correlation id.
    Mismatch,
    /// First connection drops mid-exchange; later connections answer.
    ReconnectNewIdentity,
    /// First server frame is not a `hello` (handshake contract violation).
    WrongHelloType,
    /// Detects any client frame transmitted before the hello.
    PreHelloOrdering,
    /// Answers with `ok:false` and a structured error block.
    ApplicationError,
    /// Accept handshake + hello, then close before answering every request.
    CloseBeforeAnswer,
}

struct TestServer {
    port: u16,
    token: String,
    /// True when a client frame reached the server before its hello.
    prehello_violation: Arc<AtomicBool>,
    /// Number of accepted websocket connections.
    connections: Arc<AtomicUsize>,
}

fn start_server(behavior: ServerBehavior) -> TestServer {
    use std::sync::mpsc;
    let token = "tok_fixture_0123456789".to_string();
    let (port_tx, port_rx) = mpsc::channel();
    let prehello_violation = Arc::new(AtomicBool::new(false));
    let connections = Arc::new(AtomicUsize::new(0));
    let server_token = token.clone();
    let violation = Arc::clone(&prehello_violation);
    let conn_counter = Arc::clone(&connections);
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = AsyncTcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .unwrap();
            let port = listener.local_addr().unwrap().port();
            port_tx.send(port).unwrap();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let token = server_token.clone();
                let violation = Arc::clone(&violation);
                let conn_counter = Arc::clone(&conn_counter);
                tokio::spawn(async move {
                    run_connection(stream, behavior, token, violation, conn_counter).await;
                });
            }
        });
    });
    let port = port_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("fixture server started");
    TestServer {
        port,
        token,
        prehello_violation,
        connections,
    }
}

fn extract_query_token(request: &Request) -> Option<String> {
    let query = request.uri().query()?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "token").then(|| value.to_string())
    })
}

#[allow(clippy::result_large_err)] // fixture-only 401 rejection path
async fn run_connection(
    stream: AsyncTcpStream,
    behavior: ServerBehavior,
    token: String,
    prehello_violation: Arc<AtomicBool>,
    connections: Arc<AtomicUsize>,
) {
    let accepted = accept_hdr_async(stream, |request: &Request, response: Response| {
        let authenticated = extract_query_token(request) == Some(token);
        if matches!(behavior, ServerBehavior::RejectAuth) || !authenticated {
            return Err(Response::builder()
                .status(401)
                .body::<Option<String>>(None)
                .expect("static unauthorized response"));
        }
        Ok(response)
    })
    .await;
    let Ok(ws_stream) = accepted else {
        return;
    };
    let connection_index = connections.fetch_add(1, Ordering::SeqCst);
    let (mut writer, mut reader) = ws_stream.split();
    let connection_id = format!("fixture-connection-{connection_index}");

    if behavior == ServerBehavior::WrongHelloType {
        // Violate the handshake contract: first frame is not a hello.
        let welcome = r#"{"type":"welcome","connectionId":"not-a-hello"}"#;
        let _ = writer
            .send(Message::Text(Utf8Bytes::from_static(welcome)))
            .await;
        while let Some(Ok(_)) = reader.next().await {}
        return;
    }

    if behavior == ServerBehavior::PreHelloOrdering {
        // A correct client sends nothing until its hello lands, so this
        // bounded read must time out empty.
        let early = tokio::time::timeout(Duration::from_millis(300), reader.next()).await;
        if let Ok(Some(Ok(_))) = early {
            prehello_violation.store(true, Ordering::SeqCst);
        }
    }

    let hello = format!(r#"{{"type":"hello","connectionId":"{connection_id}"}}"#);
    writer
        .send(Message::Text(Utf8Bytes::from(hello)))
        .await
        .unwrap();

    match behavior {
        ServerBehavior::Authenticated
        | ServerBehavior::Mismatch
        | ServerBehavior::ReconnectNewIdentity
        | ServerBehavior::ApplicationError
        | ServerBehavior::PreHelloOrdering => {
            while let Some(Ok(message)) = reader.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let Ok(value) = serde_json::from_slice::<Value>(text.as_bytes()) else {
                    continue;
                };
                if behavior == ServerBehavior::ReconnectNewIdentity && connection_index == 0 {
                    // Drop without answering so the client must reconnect and
                    // re-authenticate against a fresh hello identity.
                    return;
                }
                let response_type = match value.get("type").and_then(Value::as_str) {
                    Some("control_request") => "control_response",
                    Some("broker_request") => "broker_response",
                    _ => "query_response",
                };
                let response_id = if behavior == ServerBehavior::Mismatch {
                    json!("foreign-correlation-id")
                } else {
                    value.get("id").cloned().unwrap_or(Value::Null)
                };
                let response = if behavior == ServerBehavior::ApplicationError {
                    json!({
                        "type": response_type,
                        "id": response_id,
                        "ok": false,
                        "error": {
                            "code": "operation_not_session_owned",
                            "message": "fixture application error",
                        },
                    })
                } else {
                    json!({
                        "type": response_type,
                        "id": response_id,
                        "ok": true,
                        "page": {
                            "items": [{"sessionId": "01a02ccd-c754-7656-95c7-f40b5a140bc3"}],
                            "complete": true,
                        },
                    })
                };
                writer
                    .send(Message::Text(Utf8Bytes::from(
                        serde_json::to_string(&response).unwrap(),
                    )))
                    .await
                    .unwrap();
            }
        }
        ServerBehavior::Silent => while let Some(Ok(_)) = reader.next().await {},
        // Drop without answering so the client observes a connection lost
        // mid-exchange on every attempt.
        ServerBehavior::CloseBeforeAnswer => {
            reader.next().await;
        }
        ServerBehavior::WrongHelloType | ServerBehavior::RejectAuth => unreachable!(),
    }
}

fn write_live_metadata_for(server: &TestServer, temp: &TempDir) -> PathBuf {
    let worktree = temp.path().join("worktree");
    std::fs::create_dir_all(state_root(&worktree)).unwrap();
    write_metadata(
        &state_root(&worktree),
        &metadata_json(
            &format!("ws://127.0.0.1:{}/", server.port),
            &server.token,
            Some(std::process::id()),
        ),
    );
    worktree
}

#[test]
fn transport_success_round_trip_with_correlation() {
    let server = start_server(ServerBehavior::Authenticated);
    let temp = TempDir::new().unwrap();
    let worktree = write_live_metadata_for(&server, &temp);
    let snapshot = probe_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("live"));
    assert_eq!(
        snapshot["probe"]["hello_connection_id"],
        json!("fixture-connection-0")
    );
    assert_eq!(snapshot["probe"]["request_ok"], json!(true));
    assert_eq!(snapshot["probe"]["request_correlated"], json!(true));
    assert!(snapshot["probe"].get("request_error_code").is_none());
    let encoded = snapshot.to_string();
    assert!(!encoded.contains(&server.token), "token leaked: {encoded}");
    assert!(
        !encoded.contains("127.0.0.1"),
        "endpoint url leaked: {encoded}"
    );
}

#[test]
fn transport_wrong_hello_type_fails_closed() {
    let server = start_server(ServerBehavior::WrongHelloType);
    let temp = TempDir::new().unwrap();
    let worktree = write_live_metadata_for(&server, &temp);
    let snapshot = probe_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("live"));
    assert_eq!(
        snapshot["probe"]["hello_reason"],
        json!("invalid_hello"),
        "{snapshot}"
    );
    assert!(snapshot["probe"].get("request_ok").is_none());
}

#[test]
fn transport_never_sends_before_the_hello_gate() {
    let server = start_server(ServerBehavior::PreHelloOrdering);
    let temp = TempDir::new().unwrap();
    let worktree = write_live_metadata_for(&server, &temp);
    let snapshot = probe_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("live"));
    assert_eq!(snapshot["probe"]["request_ok"], json!(true), "{snapshot}");
    assert!(
        !server.prehello_violation.load(Ordering::SeqCst),
        "the binary transmitted before receiving hello"
    );
}

#[test]
fn transport_surfaces_typed_application_errors() {
    let server = start_server(ServerBehavior::ApplicationError);
    let temp = TempDir::new().unwrap();
    let worktree = write_live_metadata_for(&server, &temp);
    let snapshot = probe_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("live"));
    assert_eq!(snapshot["probe"]["request_ok"], json!(false), "{snapshot}");
    assert_eq!(
        snapshot["probe"]["request_correlated"],
        json!(true),
        "{snapshot}"
    );
    assert_eq!(
        snapshot["probe"]["request_error_code"],
        json!("operation_not_session_owned")
    );
}

#[test]
fn transport_reconnect_reauthenticates_with_new_identity() {
    let server = start_server(ServerBehavior::ReconnectNewIdentity);
    let temp = TempDir::new().unwrap();
    let worktree = write_live_metadata_for(&server, &temp);
    let snapshot = probe_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("live"));
    // Connection 0 drops mid-exchange; the bounded retry re-authenticates and
    // completes against the fresh hello identity.
    assert_eq!(
        snapshot["probe"]["hello_connection_id"],
        json!("fixture-connection-1"),
        "{snapshot}"
    );
    assert_eq!(snapshot["probe"]["request_ok"], json!(true));
    assert_eq!(server.connections.load(Ordering::SeqCst), 2);
}

#[test]
fn transport_unauthorized_is_typed() {
    let server = start_server(ServerBehavior::RejectAuth);
    let temp = TempDir::new().unwrap();
    let worktree = temp.path().join("worktree");
    std::fs::create_dir_all(state_root(&worktree)).unwrap();
    write_metadata(
        &state_root(&worktree),
        &metadata_json(
            &format!("ws://127.0.0.1:{}/", server.port),
            "wrong-token",
            Some(std::process::id()),
        ),
    );
    let snapshot = probe_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("live"));
    // A 401 handshake rejection surfaces at the hello stage.
    assert_eq!(
        snapshot["probe"]["hello_reason"],
        json!("endpoint_unauthorized")
    );
}

#[test]
fn transport_timeout_is_typed() {
    let server = start_server(ServerBehavior::Silent);
    let temp = TempDir::new().unwrap();
    let worktree = write_live_metadata_for(&server, &temp);
    let snapshot = probe_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("live"));
    assert_eq!(snapshot["probe"]["request_reason"], json!("timeout"));
}

#[test]
fn transport_correlation_mismatch_is_typed() {
    let server = start_server(ServerBehavior::Mismatch);
    let temp = TempDir::new().unwrap();
    let worktree = write_live_metadata_for(&server, &temp);
    let snapshot = probe_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("live"));
    assert_eq!(
        snapshot["probe"]["request_reason"],
        json!("correlation_mismatch")
    );
}

#[test]
fn transport_unavailable_endpoint_is_typed() {
    let port = unused_port();
    let temp = TempDir::new().unwrap();
    let worktree = temp.path().join("worktree");
    std::fs::create_dir_all(state_root(&worktree)).unwrap();
    write_metadata(
        &state_root(&worktree),
        &metadata_json(
            &format!("ws://127.0.0.1:{port}/"),
            "tok_none",
            Some(std::process::id()),
        ),
    );
    let snapshot = probe_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("live"));
    // Nothing listens on the port: connect fails before any HTTP status, so
    // the taxonomy surfaces transport-level unavailability at the hello stage.
    assert_eq!(
        snapshot["probe"]["hello_reason"],
        json!("endpoint_unavailable")
    );
}

#[test]
fn transport_close_before_answer_exhausts_bounded_reconnect() {
    // Every connection drops mid-exchange; the probe's bounded reconnect
    // budget is exhausted and reported under the retry taxonomy.
    let server = start_server(ServerBehavior::CloseBeforeAnswer);
    let temp = TempDir::new().unwrap();
    let worktree = write_live_metadata_for(&server, &temp);
    let snapshot = probe_stdout(&worktree);
    assert_eq!(snapshot["status"], json!("live"));
    assert_eq!(
        snapshot["probe"]["request_reason"],
        json!("retry_exhausted"),
        "{snapshot}"
    );
}
