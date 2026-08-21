//! Integration tests for fail-closed Discord sender-identity verification
//! (issue #319).
//!
//! These tests run the real `clawhip` binary against a loopback mock of the
//! Discord REST API, exercising `clawhip config verify-sender-identity` end to
//! end. All bot IDs are synthetic test snowflakes — no host-private IDs.
//!
//! The regression this suite pins: a wrong-but-valid Discord bot token must
//! NOT produce a healthy sender-identity verdict merely because transport
//! works. The 2026-08-21 incident delivered real messages from the wrong bot
//! while `token_source=config` and `discord_send_success` stayed green.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::mpsc;
use std::thread;

use tempfile::TempDir;

/// Synthetic snowflake IDs (not real Discord applications).
const EXPECTED_BOT_ID: &str = "900000000000000101";
const WRONG_BOT_ID: &str = "900000000000000202";

fn clawhip_bin() -> &'static str {
    env!("CARGO_BIN_EXE_clawhip")
}

/// One-shot mock Discord API that records the incoming request line and
/// responds with `status` + `body`, then closes.
fn spawn_discord_api_mock(status: &str, body: &str) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Discord API");
    let addr = listener.local_addr().expect("mock addr");
    let (tx, rx) = mpsc::channel();
    let status = status.to_string();
    let body = body.to_string();

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut request = [0_u8; 2048];
        let read = stream.read(&mut request).expect("read request");
        tx.send(String::from_utf8_lossy(&request[..read]).into_owned())
            .expect("send request");
        write!(
            stream,
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len(),
        )
        .expect("write response");
    });

    (format!("http://{addr}"), rx)
}

fn write_config(temp: &TempDir, config: &str) -> std::path::PathBuf {
    let config_path = temp.path().join("clawhip.toml");
    fs::write(&config_path, config).expect("write config");
    config_path
}

fn run_verify(config_path: &std::path::Path, api_base: &str, temp: &TempDir, json: bool) -> Output {
    let mut command = Command::new(clawhip_bin());
    command
        .current_dir(temp.path())
        .env("HOME", temp.path())
        .env("CLAWHIP_DISCORD_API_BASE", api_base)
        .env_remove("CLAWHIP_BOT_TOKEN")
        .env_remove("DISCORD_BOT_TOKEN")
        .arg("--config")
        .arg(config_path)
        .arg("config")
        .arg("verify-sender-identity");
    if json {
        command.arg("--json");
    }
    command
        .output()
        .expect("run clawhip config verify-sender-identity")
}

type Output = std::process::Output;

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The identity probe must hit exactly `/users/@me` with the bot credential.
fn assert_identity_request(request: &str) {
    assert!(
        request.starts_with("GET /users/@me "),
        "expected GET /users/@me, got: {request}"
    );
    assert!(
        request.contains("authorization: Bot test-token-319")
            || request.contains("Authorization: Bot test-token-319"),
        "expected Bot authorization header, got: {request}"
    );
}

#[test]
fn match_reports_verified_and_exits_zero() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = write_config(
        &temp,
        &format!(
            r#"
[providers.discord]
bot_token = "test-token-319"
expected_bot_id = "{EXPECTED_BOT_ID}"
"#
        ),
    );
    let (api_base, request_rx) = spawn_discord_api_mock(
        "200 OK",
        &format!(r#"{{"id": "{EXPECTED_BOT_ID}", "username": "clawhip"}}"#),
    );

    let output = run_verify(&config_path, &api_base, &temp, false);

    assert!(
        output.status.success(),
        "verified identity must exit 0\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert_identity_request(&request_rx.recv().expect("mock saw request"));
    let text = stdout(&output);
    assert!(text.contains("VERIFIED"), "stdout: {text}");
    assert!(text.contains(EXPECTED_BOT_ID), "stdout: {text}");
    assert!(!text.contains("test-token-319"), "token leaked: {text}");
}

#[test]
fn mismatch_fails_closed_with_precise_public_safe_diagnosis() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = write_config(
        &temp,
        &format!(
            r#"
[providers.discord]
bot_token = "test-token-319"
expected_bot_id = "{EXPECTED_BOT_ID}"
"#
        ),
    );
    // Wrong-but-valid token: transport would succeed; identity must not.
    let (api_base, request_rx) = spawn_discord_api_mock(
        "200 OK",
        &format!(r#"{{"id": "{WRONG_BOT_ID}", "username": "other-bot"}}"#),
    );

    let output = run_verify(&config_path, &api_base, &temp, true);

    assert!(
        !output.status.success(),
        "wrong-but-valid token must fail closed\nstdout:\n{}",
        stdout(&output)
    );
    assert_identity_request(&request_rx.recv().expect("mock saw request"));
    let text = stdout(&output);
    assert!(text.contains("\"verified\": false"), "stdout: {text}");
    assert!(text.contains("sender_identity_mismatch"), "stdout: {text}");
    assert!(
        text.contains(EXPECTED_BOT_ID),
        "expected id missing: {text}"
    );
    assert!(text.contains(WRONG_BOT_ID), "observed id missing: {text}");
    assert!(!text.contains("test-token-319"), "token leaked: {text}");
}

#[test]
fn absent_expectation_is_not_reported_healthy() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = write_config(
        &temp,
        r#"
[providers.discord]
bot_token = "test-token-319"
"#,
    );
    let (api_base, _request_rx) = spawn_discord_api_mock(
        "200 OK",
        &format!(r#"{{"id": "{WRONG_BOT_ID}", "username": "other-bot"}}"#),
    );

    let output = run_verify(&config_path, &api_base, &temp, true);

    // Absent expectation must fail the preflight: identity is unverified, and
    // the operator is told how to enable verification. No API call expected.
    assert!(
        !output.status.success(),
        "absent expectation must not be reported healthy\nstdout:\n{}",
        stdout(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("sender_identity_not_configured"),
        "stdout: {text}"
    );
    assert!(
        text.contains("null"),
        "expected_bot_id should be null: {text}"
    );
}

#[test]
fn invalid_credential_fails_closed() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = write_config(
        &temp,
        &format!(
            r#"
[providers.discord]
bot_token = "test-token-319"
expected_bot_id = "{EXPECTED_BOT_ID}"
"#
        ),
    );
    let (api_base, request_rx) =
        spawn_discord_api_mock("401 Unauthorized", r#"{"message": "401: Unauthorized"}"#);

    let output = run_verify(&config_path, &api_base, &temp, true);

    assert!(
        !output.status.success(),
        "invalid credential must fail closed\nstdout:\n{}",
        stdout(&output)
    );
    assert_identity_request(&request_rx.recv().expect("mock saw request"));
    let text = stdout(&output);
    assert!(
        text.contains("sender_identity_invalid_credential"),
        "stdout: {text}"
    );
    // Public-safe: the Discord error body must not be echoed.
    assert!(!text.contains("401: Unauthorized"), "body leaked: {text}");
}

#[test]
fn transport_failure_fails_closed() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = write_config(
        &temp,
        &format!(
            r#"
[providers.discord]
bot_token = "test-token-319"
expected_bot_id = "{EXPECTED_BOT_ID}"
"#
        ),
    );
    // Point at a dead port: connection refused.
    let dead_listener = TcpListener::bind("127.0.0.1:0").expect("bind dead port");
    let dead_addr = dead_listener.local_addr().expect("dead addr");
    drop(dead_listener);

    let output = run_verify(&config_path, &format!("http://{dead_addr}"), &temp, true);

    assert!(
        !output.status.success(),
        "transport failure must fail closed\nstdout:\n{}",
        stdout(&output)
    );
    let text = stdout(&output);
    assert!(
        text.contains("sender_identity_transport_failure"),
        "stdout: {text}"
    );
    assert!(!text.contains("test-token-319"), "token leaked: {text}");
}

#[test]
fn malformed_success_fails_closed() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = write_config(
        &temp,
        &format!(
            r#"
[providers.discord]
bot_token = "test-token-319"
expected_bot_id = "{EXPECTED_BOT_ID}"
"#
        ),
    );
    // 200 with no usable stable bot ID: identity is unverified, fail closed.
    let (api_base, request_rx) = spawn_discord_api_mock("200 OK", r#"{"username": "anon"}"#);

    let output = run_verify(&config_path, &api_base, &temp, true);

    assert!(
        !output.status.success(),
        "malformed success must fail closed\nstdout:\n{}",
        stdout(&output)
    );
    assert_identity_request(&request_rx.recv().expect("mock saw request"));
    let text = stdout(&output);
    assert!(
        text.contains("sender_identity_malformed_response"),
        "stdout: {text}"
    );
}

#[test]
fn no_token_fails_closed_without_api_call() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = write_config(
        &temp,
        &format!(
            r#"
[providers.discord]
expected_bot_id = "{EXPECTED_BOT_ID}"

[[routes]]
event = "git.commit"
webhook = "https://discord.com/api/webhooks/1/synthetic"
"#
        ),
    );
    let (api_base, request_rx) =
        spawn_discord_api_mock("200 OK", &format!(r#"{{"id": "{EXPECTED_BOT_ID}"}}"#));

    let output = run_verify(&config_path, &api_base, &temp, true);

    assert!(
        !output.status.success(),
        "no token must fail closed\nstdout:\n{}",
        stdout(&output)
    );
    let text = stdout(&output);
    assert!(text.contains("sender_identity_no_token"), "stdout: {text}");
    // No token means no request should have been attempted.
    assert!(
        request_rx.try_recv().is_err(),
        "no API call expected without a token"
    );
}

#[test]
fn legacy_discord_bot_id_config_is_migrated_and_verified() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = write_config(
        &temp,
        &format!(
            r#"
[discord]
token = "test-token-319"
bot_id = "{EXPECTED_BOT_ID}"
"#
        ),
    );
    let (api_base, request_rx) = spawn_discord_api_mock(
        "200 OK",
        &format!(r#"{{"id": "{EXPECTED_BOT_ID}", "username": "clawhip"}}"#),
    );

    let output = run_verify(&config_path, &api_base, &temp, false);

    assert!(
        output.status.success(),
        "legacy [discord].bot_id must migrate and verify\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert_identity_request(&request_rx.recv().expect("mock saw request"));
}

#[test]
fn conflicting_legacy_and_provider_bot_id_is_rejected() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = write_config(
        &temp,
        &format!(
            r#"
[discord]
bot_id = "{EXPECTED_BOT_ID}"
[providers.discord]
bot_token = "test-token-319"
bot_id = "{WRONG_BOT_ID}"
"#
        ),
    );
    let (api_base, _request_rx) = spawn_discord_api_mock("200 OK", r#"{"id": "1"}"#);

    let output = run_verify(&config_path, &api_base, &temp, true);

    assert!(
        !output.status.success(),
        "conflicting bot_id values must be rejected\nstdout:\n{}",
        stdout(&output)
    );
}

#[test]
fn non_snowflake_expected_bot_id_is_rejected_by_config_validation() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = write_config(
        &temp,
        r#"
[providers.discord]
bot_token = "test-token-319"
expected_bot_id = "clawhip-bot"
"#,
    );
    let (api_base, _request_rx) = spawn_discord_api_mock("200 OK", r#"{"id": "1"}"#);

    let output = run_verify(&config_path, &api_base, &temp, true);

    assert!(
        !output.status.success(),
        "non-snowflake expected_bot_id must fail config validation\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("expected_bot_id must be a numeric Discord snowflake ID"),
        "diagnostic missing: {text}"
    );
}

#[test]
fn help_documents_the_fail_closed_contract() {
    let output = Command::new(clawhip_bin())
        .arg("config")
        .arg("verify-sender-identity")
        .arg("--help")
        .output()
        .expect("run help");

    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("expected_bot_id"), "help: {text}");
    assert!(text.contains("--json"), "help: {text}");
}
