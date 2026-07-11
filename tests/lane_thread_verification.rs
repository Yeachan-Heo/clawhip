//! Process-level proof for the lane state/effect protocol.  The fixture deliberately
//! records only operation names and counters: credentials and Discord bodies are never
//! persisted or printed by this test.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use serial_test::serial;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_clawhip")
}

fn write_file(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, body).expect("write fixture");
}

fn executable(path: &Path, body: &str) {
    write_file(path, body);
    let mut mode = fs::metadata(path).expect("metadata").permissions();
    mode.set_mode(0o755);
    fs::set_permissions(path, mode).expect("chmod");
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind port")
        .local_addr()
        .expect("address")
        .port()
}

fn output(command: &mut Command) -> Output {
    command.output().expect("run clawhip")
}

fn text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn successful(output: &Output) {
    assert!(
        output.status.success(),
        "status={:?}; output={}",
        output.status.code(),
        text(output)
    );
}

fn failed(output: &Output) {
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        text(output)
    );
}

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct MockDiscord {
    port: u16,
    calls: Arc<Mutex<Vec<String>>>,
    alive: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}
impl MockDiscord {
    fn start(mode: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind Discord mock");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("mock address").port();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let thread_calls = Arc::clone(&calls);
        let thread_alive = Arc::clone(&alive);
        let join = thread::spawn(move || {
            while thread_alive.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut bytes = [0; 4096];
                        let read = stream.read(&mut bytes).unwrap_or(0);
                        let line = String::from_utf8_lossy(&bytes[..read])
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_owned();
                        thread_calls.lock().expect("calls").push(line.clone());
                        if mode == "timeout" {
                            thread::sleep(Duration::from_secs(6));
                            continue;
                        }
                        let response = if line.starts_with("POST") && mode == "malformed" {
                            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx"
                        } else if line.starts_with("POST") {
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 62\r\nConnection: close\r\n\r\n{\"id\":\"987654321098765432\",\"timestamp\":\"2026-01-01T00:00:00Z\"}"
                        } else if mode == "receipt" && line.contains("/messages/987654321098765432")
                        {
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"id\":\"987654321098765432\"}"
                        } else if line.contains("messages?limit=1") {
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]"
                        } else {
                            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                        };
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.shutdown(Shutdown::Both);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            calls,
            alive,
            join: Some(join),
        }
    }
    fn api_base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    fn count(&self, method: &str) -> usize {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|line| line.starts_with(method))
            .count()
    }
    fn count_path(&self, needle: &str) -> usize {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .filter(|line| line.contains(needle))
            .count()
    }
}
impl Drop for MockDiscord {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct ResponseLossProxy {
    port: u16,
    paths: Arc<Mutex<Vec<String>>>,
    alive: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl ResponseLossProxy {
    fn start(backend_port: u16, drop_path: &'static str, drop_occurrence: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind response-loss proxy");
        listener.set_nonblocking(true).expect("nonblocking proxy");
        let port = listener.local_addr().expect("proxy address").port();
        let paths = Arc::new(Mutex::new(Vec::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let thread_paths = Arc::clone(&paths);
        let thread_alive = Arc::clone(&alive);
        let join = thread::spawn(move || {
            let mut occurrences = HashMap::<String, usize>::new();
            while thread_alive.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut client, _)) => {
                        let request = read_http_message(&mut client).expect("proxy request");
                        let line = request.lines().next().unwrap_or("");
                        let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                        thread_paths.lock().expect("proxy paths").push(path.clone());
                        let occurrence = occurrences.entry(path.clone()).or_default();
                        *occurrence += 1;
                        let mut backend =
                            TcpStream::connect(("127.0.0.1", backend_port)).expect("proxy backend");
                        backend
                            .write_all(request.as_bytes())
                            .expect("forward request");
                        let response = read_http_message(&mut backend).expect("backend response");
                        if path == drop_path && *occurrence == drop_occurrence {
                            continue;
                        }
                        let _ = client.write_all(response.as_bytes());
                        let _ = client.shutdown(Shutdown::Both);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            paths,
            alive,
            join: Some(join),
        }
    }

    fn path_count(&self, path: &str) -> usize {
        self.paths
            .lock()
            .expect("proxy paths")
            .iter()
            .filter(|value| value.as_str() == path)
            .count()
    }
}

impl Drop for ResponseLossProxy {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn read_http_message(stream: &mut TcpStream) -> std::io::Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(8)))?;
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length:")
                .or_else(|| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn fake_tmux(path: &Path, state: &Path) {
    let quoted = state.display().to_string().replace('\'', "'\\''");
    executable(
        path,
        &format!(
            r#"#!/usr/bin/env bash
set -eu
S='{quoted}'
mkdir -p "$S"
printf '%s\n' "$1" >> "$S/calls"
case "$1" in
  new-session) test -f "$S/fail_new_session" && exit 1; session=""; while [ $# -gt 0 ]; do case "$1" in -s) session="$2"; shift 2 ;; *) shift ;; esac; done; printf '%s' "$session" > "$S/session"; printf '1' > "$S/live" ;;
  has-session) test -f "$S/live" ;;
  set-option) key="$4"; value="$5"; test "$key" = "@clawhip_lane_generation" && test -f "$S/fail_generation_set" && exit 1; printf '%s' "$value" > "$S/${{key#@}}"; test "$key" = "@clawhip_lane_generation" && test -f "$S/mismatch_generation" && printf 'different-generation' > "$S/${{key#@}}" ;;
  show-options) key="$5"; safe="${{key#@}}"; test -f "$S/unreadable_$safe" && exit 1; file="$S/$safe"; if test -f "$file"; then printf '%s\n' "$(cat "$file")"; else echo 'invalid option' >&2; exit 1; fi ;;
  send-keys) test "${{4:-}}" = "-l" || printf '1' >> "$S/sends" ;;
  display-message) printf 'lane\t%%1\t1\tcodex\t/tmp\n' ;;
  kill-session) rm -f "$S/live" ;;
  list-sessions) test -f "$S/live" && test -f "$S/session" && cat "$S/session" ;;
  *) : ;;
esac
"#
        ),
    );
}

fn config(path: &Path, port: u16) {
    write_file(
        path,
        &format!(
            "[daemon]\nbind_host = \"127.0.0.1\"\nport = {port}\nbase_url = \"http://127.0.0.1:{port}\"\n\n[providers.discord]\ntoken = \"fixture-token\"\n\n[defaults]\nformat = \"compact\"\n"
        ),
    );
}

fn config_with_base_url(path: &Path, port: u16, base_port: u16) {
    write_file(
        path,
        &format!(
            "[daemon]\nbind_host = \"127.0.0.1\"\nport = {port}\nbase_url = \"http://127.0.0.1:{base_port}\"\n\n[providers.discord]\ntoken = \"fixture-token\"\n\n[defaults]\nformat = \"compact\"\n"
        ),
    );
}

fn start(config_path: &Path, home: &Path, tmux: &Path, discord: &MockDiscord, port: u16) -> Daemon {
    let child = Command::new(bin())
        .arg("--config")
        .arg(config_path)
        .arg("start")
        .arg("--port")
        .arg(port.to_string())
        .env("HOME", home)
        .env("CLAWHIP_TMUX_BIN", tmux)
        .env("CLAWHIP_DISCORD_API_BASE", discord.api_base())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start daemon");
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let _ = stream
                .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
            if response.starts_with("HTTP/1.1 200") {
                return Daemon(child);
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon did not become healthy");
}

fn lane_command(
    config: &Path,
    home: &Path,
    tmux: &Path,
    discord: &MockDiscord,
    args: &[&str],
) -> Output {
    output(
        Command::new(bin())
            .arg("--config")
            .arg(config)
            .args(args)
            .env("HOME", home)
            .env("CLAWHIP_TMUX_BIN", tmux)
            .env("CLAWHIP_DISCORD_API_BASE", discord.api_base()),
    )
}

fn registry_path(config: &Path) -> std::path::PathBuf {
    config
        .parent()
        .expect("config parent")
        .join("tmux-watch-registry.json")
}

fn registry(config: &Path) -> Value {
    serde_json::from_slice(&fs::read(registry_path(config)).expect("registry bytes"))
        .expect("registry json")
}

fn assert_marker_gate(command_effect: bool, marker: &str, value: Option<&str>, unreadable: bool) {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("home");
    let config_path = temp.path().join("config.toml");
    let state = temp.path().join("tmux");
    let tmux = temp.path().join("tmux.sh");
    fs::create_dir_all(&home).expect("home");
    fake_tmux(&tmux, &state);
    let discord = MockDiscord::start("success");
    let port = free_port();
    config(&config_path, port);
    let _daemon = start(&config_path, &home, &tmux, &discord, port);
    let session = if command_effect {
        "marker-command"
    } else {
        "marker-create"
    };
    let generation_id = format!("generation-{session}");
    let kickoff_operation_id = format!("kickoff-{session}");
    let launch_operation_id = format!("launch-{session}");
    let executor_id = format!("executor-{session}");
    let worker_effect_kind = if command_effect {
        "command-submission"
    } else {
        "session-creation"
    };
    write_file(&state.join("live"), "1");
    write_file(&state.join("session"), session);
    write_file(&state.join("clawhip_lane_generation"), &generation_id);
    write_file(
        &state.join("clawhip_lane_launch_operation"),
        &launch_operation_id,
    );
    let marker_path = state.join(marker);
    if let Some(value) = value {
        write_file(&marker_path, value);
    } else {
        let _ = fs::remove_file(&marker_path);
    }
    if unreadable {
        write_file(&state.join(format!("unreadable_{marker}")), "1");
    }
    let registered = http_json_value(
        port,
        "POST",
        "/api/lane/register",
        json!({
            "registration": {"session":session,"channel":null,"mention":null,"keywords":[],"keyword_window_secs":30,"stale_minutes":10,"format":null,"active_wrapper_monitor":false},
            "generation_id":generation_id,"kickoff_operation_id":kickoff_operation_id,"launch_operation_id":launch_operation_id,"executor_id":executor_id,"worker_effect_kind":worker_effect_kind,"thread_id":"123456789012345678","expect_absent_or_retired":true
        }),
    );
    let claimed = http_json_value(
        port,
        "POST",
        "/api/lane/claim",
        json!({
            "session":session,"generation_id":generation_id,"executor_id":executor_id,"expected_revision":registered["snapshot"]["revision"]
        }),
    );
    let identity_verified = http_json_value(
        port,
        "POST",
        "/api/lane/evidence",
        json!({
            "session":session,"generation_id":generation_id,"launch_operation_id":launch_operation_id,"expected_revision":claimed["revision"],"launch_state":"identity-verified","failure_category":null,"executor_id":executor_id,"worker_effect_kind":worker_effect_kind
        }),
    );
    assert_eq!(
        identity_verified["durable_launch_state"],
        "identity-verified"
    );
    let mut args = vec![
        "tmux",
        "new",
        "--session",
        session,
        "--thread",
        "123456789012345678",
        "--json",
    ];
    if command_effect {
        args.extend(["--", "worker"]);
    }
    let baseline = fs::read_to_string(state.join("calls")).unwrap_or_default();
    let inspected = lane_command(&config_path, &home, &tmux, &discord, &args);
    failed(&inspected);
    let json: Value = serde_json::from_slice(&inspected.stdout).expect("inspector json");
    let expected = if unreadable {
        if command_effect {
            "submitted-marker-read-failed"
        } else {
            "submitted-marker-unreadable"
        }
    } else if value.is_some_and(|v| v.is_empty()) || value.is_none() {
        "submitted-marker-missing"
    } else {
        "submitted-marker-mismatch"
    };
    assert_eq!(json["exit_category"], expected);
    if command_effect {
        assert_eq!(
            registry(&config_path)[session]["lane"]["launch_state"],
            "command-submit-ambiguous",
            "daemon serializes the R2 terminal transition before returning"
        );
    }
    let delta = calls_after(&state, &baseline);
    assert_eq!(command_count(&delta, "has-session"), 1);
    assert_eq!(
        command_count(&delta, "show-options"),
        3,
        "inspector reads G/L/submitted exactly once"
    );
    assert!(
        !delta
            .lines()
            .any(|call| matches!(call, "send-keys" | "set-option")),
        "restart inspection must not write markers or submit work"
    );
}

fn http_json(port: u16, method: &str, path: &str, body: Value) -> String {
    let payload = serde_json::to_string(&body).expect("request json");
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect daemon");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream
        .write_all(request.as_bytes())
        .expect("write daemon request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read daemon response");
    response
}

fn http_json_value(port: u16, method: &str, path: &str, body: Value) -> Value {
    let response = http_json(port, method, path, body);
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "daemon endpoint rejected fixture request"
    );
    let payload = response
        .split_once("\r\n\r\n")
        .expect("HTTP response body")
        .1;
    serde_json::from_str(payload).expect("daemon JSON response")
}

fn calls_after(state: &Path, baseline: &str) -> String {
    let calls = fs::read_to_string(state.join("calls")).unwrap_or_default();
    calls
        .strip_prefix(baseline)
        .expect("tmux call log is append-only")
        .to_owned()
}

fn command_count(calls: &str, command: &str) -> usize {
    calls.lines().filter(|call| *call == command).count()
}

fn count(path: &Path, name: &str) -> usize {
    fs::read_to_string(path.join(name))
        .unwrap_or_default()
        .lines()
        .count()
}

#[test]
#[serial]
fn kickoff_command_launch_persists_receipts_markers_and_reloadable_status() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("home");
    let config_path = temp.path().join("config.toml");
    let state = temp.path().join("tmux");
    let tmux = temp.path().join("tmux.sh");
    fs::create_dir_all(&home).expect("home");
    fake_tmux(&tmux, &state);
    let discord = MockDiscord::start("success");
    let port = free_port();
    config(&config_path, port);
    let _daemon = start(&config_path, &home, &tmux, &discord, port);
    let result = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "lane-success",
            "--thread",
            "123456789012345678",
            "--kickoff",
            "fixture kickoff",
            "--json",
            "--",
            "worker",
        ],
    );
    successful(&result);
    let json: Value = serde_json::from_slice(&result.stdout).expect("launch json");
    assert_eq!(json["durable_launch_state"], "launched");
    assert!(json.get("exit_category").is_some_and(Value::is_null));
    assert_eq!(discord.count("POST"), 1);
    assert_eq!(count(&state, "sends"), 1);
    assert!(state.join("clawhip_lane_generation").is_file());
    assert!(state.join("clawhip_lane_launch_operation").is_file());
    assert_eq!(
        fs::read_to_string(state.join("clawhip_lane_command_submitted"))
            .expect("submitted")
            .trim(),
        json["launch_operation_id"].as_str().expect("L")
    );
    let durable = registry(&config_path);
    assert!(durable.to_string().contains("kickoff_message_id"));
    let status = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &["lane", "status", "--session", "lane-success", "--json"],
    );
    successful(&status);
    let public = String::from_utf8_lossy(&status.stdout);
    assert!(!public.contains("123456789012345678"));
}

#[test]
#[serial]
fn session_creation_and_restart_inspection_do_not_send_commands_or_kickoff_twice() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("home");
    let config_path = temp.path().join("config.toml");
    let state = temp.path().join("tmux");
    let tmux = temp.path().join("tmux.sh");
    fs::create_dir_all(&home).expect("home");
    fake_tmux(&tmux, &state);
    let discord = MockDiscord::start("success");
    let port = free_port();
    config(&config_path, port);
    let _daemon = start(&config_path, &home, &tmux, &discord, port);
    let first = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "lane-create",
            "--thread",
            "123456789012345678",
            "--json",
        ],
    );
    successful(&first);
    let snapshot: Value = serde_json::from_slice(&first.stdout).expect("json");
    assert_eq!(snapshot["worker_effect_kind"], "session-creation");
    assert_eq!(count(&state, "sends"), 0);
    assert_eq!(discord.count("POST"), 0);
    let baseline = fs::read_to_string(state.join("calls")).unwrap_or_default();
    let restart = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "lane-create",
            "--thread",
            "123456789012345678",
            "--json",
        ],
    );
    successful(&restart);
    assert_eq!(discord.count("POST"), 0);
    let delta = calls_after(&state, &baseline);
    assert!(
        !delta
            .lines()
            .any(|call| matches!(call, "new-session" | "set-option" | "send-keys")),
        "restart inspector must not create or mutate a lane"
    );
}

#[test]
#[serial]
fn marker_gates_and_ambiguous_delivery_are_terminal_without_resend() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("home");
    let config_path = temp.path().join("config.toml");
    let state = temp.path().join("tmux");
    let tmux = temp.path().join("tmux.sh");
    fs::create_dir_all(&home).expect("home");
    fake_tmux(&tmux, &state);
    let discord = MockDiscord::start("malformed");
    let port = free_port();
    config(&config_path, port);
    let _daemon = start(&config_path, &home, &tmux, &discord, port);
    let launch = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "lane-ambiguous",
            "--thread",
            "123456789012345678",
            "--kickoff",
            "fixture",
            "--json",
            "--",
            "worker",
        ],
    );
    failed(&launch);
    assert_eq!(discord.count("POST"), 1);
    assert_eq!(count(&state, "sends"), 0);
    let combined = text(&launch);
    assert!(combined.contains("malformed-success") || combined.contains("ambiguous"));
    assert!(!combined.contains("fixture"));
    let retry = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "lane-ambiguous",
            "--thread",
            "123456789012345678",
            "--json",
            "--",
            "worker",
        ],
    );
    failed(&retry);
    assert_eq!(
        discord.count("POST"),
        1,
        "ambiguous kickoff is never retried"
    );
}

#[test]
#[serial]
fn verification_status_and_human_null_mapping_are_public_safe() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("home");
    let config_path = temp.path().join("config.toml");
    let state = temp.path().join("tmux");
    let tmux = temp.path().join("tmux.sh");
    fs::create_dir_all(&home).expect("home");
    fake_tmux(&tmux, &state);
    let discord = MockDiscord::start("success");
    let port = free_port();
    config(&config_path, port);
    let _daemon = start(&config_path, &home, &tmux, &discord, port);
    let launch = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "lane-verify",
            "--thread",
            "123456789012345678",
            "--json",
        ],
    );
    successful(&launch);
    let verify = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "lane",
            "verify-thread",
            "--session",
            "lane-verify",
            "--json",
        ],
    );
    failed(&verify);
    let json: Value = serde_json::from_slice(&verify.stdout).expect("verification json");
    assert_eq!(json["outcome"], "unverified");
    assert_eq!(json["visibility"], "unverified");
    assert!(!String::from_utf8_lossy(&verify.stdout).contains("123456789012345678"));
    let human = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &["lane", "status", "--session", "lane-verify"],
    );
    successful(&human);
    let public = String::from_utf8_lossy(&human.stdout);
    assert!(public.contains("unknown") || public.contains("unverified"));
    assert!(!public.contains("null"));
    assert!(!public.contains("123456789012345678"));
}

#[test]
#[serial]
fn submitted_marker_gates_cover_missing_empty_mismatch_and_unreadable_for_both_effect_kinds() {
    for command_effect in [true, false] {
        assert_marker_gate(
            command_effect,
            "clawhip_lane_command_submitted",
            None,
            false,
        );
        assert_marker_gate(
            command_effect,
            "clawhip_lane_command_submitted",
            Some(""),
            false,
        );
        assert_marker_gate(
            command_effect,
            "clawhip_lane_command_submitted",
            Some("other-operation"),
            false,
        );
        assert_marker_gate(
            command_effect,
            "clawhip_lane_command_submitted",
            Some("ignored"),
            true,
        );
    }
}

#[test]
#[serial]
fn timeout_and_malformed_kickoffs_have_one_post_and_no_command_effect() {
    for mode in ["malformed", "timeout"] {
        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("home");
        let config_path = temp.path().join("config.toml");
        let state = temp.path().join("tmux");
        let tmux = temp.path().join("tmux.sh");
        fs::create_dir_all(&home).expect("home");
        fake_tmux(&tmux, &state);
        let discord = MockDiscord::start(mode);
        let port = free_port();
        config(&config_path, port);
        let _daemon = start(&config_path, &home, &tmux, &discord, port);
        let result = lane_command(
            &config_path,
            &home,
            &tmux,
            &discord,
            &[
                "tmux",
                "new",
                "--session",
                "delivery-once",
                "--thread",
                "123456789012345678",
                "--kickoff",
                "redacted",
                "--json",
                "--",
                "worker",
            ],
        );
        failed(&result);
        let rendered = text(&result);
        assert!(rendered.contains(if mode == "timeout" {
            "timeout"
        } else {
            "malformed-success"
        }));
        assert!(!rendered.contains("redacted"));
        assert_eq!(discord.count("POST"), 1);
        assert_eq!(count(&state, "sends"), 0);
    }
}

#[test]
#[serial]
fn exact_receipt_verification_fetches_once_while_empty_fallback_remains_unverified() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("home");
    let config_path = temp.path().join("config.toml");
    let state = temp.path().join("tmux");
    let tmux = temp.path().join("tmux.sh");
    fs::create_dir_all(&home).expect("home");
    fake_tmux(&tmux, &state);
    let discord = MockDiscord::start("receipt");
    let port = free_port();
    config(&config_path, port);
    let _daemon = start(&config_path, &home, &tmux, &discord, port);
    successful(&lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "receipt-lane",
            "--thread",
            "123456789012345678",
            "--kickoff",
            "redacted",
            "--json",
            "--",
            "worker",
        ],
    ));
    let exact = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "lane",
            "verify-thread",
            "--session",
            "receipt-lane",
            "--json",
        ],
    );
    successful(&exact);
    let exact_json: Value = serde_json::from_slice(&exact.stdout).expect("exact verification json");
    assert_eq!(exact_json["outcome"], "kickoff-visible");
    assert_eq!(discord.count_path("/messages/987654321098765432"), 1);
    successful(&lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "empty-lane",
            "--thread",
            "123456789012345678",
            "--json",
        ],
    ));
    let fallback = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &["lane", "verify-thread", "--session", "empty-lane", "--json"],
    );
    failed(&fallback);
    let fallback_json: Value =
        serde_json::from_slice(&fallback.stdout).expect("fallback verification json");
    assert_eq!(fallback_json["outcome"], "unverified");
    assert_eq!(fallback_json["reason"], "no-message-observed");
    assert_eq!(discord.count_path("messages?limit=1"), 1);
}

#[test]
#[serial]
fn remote_lane_client_is_denied_before_network_and_private_targets_are_not_in_errors() {
    let temp = TempDir::new().expect("temp");
    let config_path = temp.path().join("remote.toml");
    write_file(
        &config_path,
        "[daemon]\nbind_host = \"127.0.0.1\"\nport = 9\nbase_url = \"http://192.0.2.1:9\"\n\n[providers.discord]\ntoken = \"fixture-token\"\n",
    );
    let fake = temp.path().join("tmux.sh");
    fake_tmux(&fake, &temp.path().join("state"));
    let discord = MockDiscord::start("success");
    let result = lane_command(
        &config_path,
        temp.path(),
        &fake,
        &discord,
        &["lane", "status", "--session", "private-target"],
    );
    failed(&result);
    let rendered = text(&result);
    assert!(rendered.contains("loopback") || rendered.contains("local"));
    assert!(!rendered.contains("123456789012345678"));
}

#[test]
#[serial]
fn legacy_registry_projection_is_loaded_without_lane_private_fields() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("home");
    let config_path = temp.path().join("config.toml");
    let state = temp.path().join("tmux");
    let tmux = temp.path().join("tmux.sh");
    fs::create_dir_all(&home).expect("home");
    fake_tmux(&tmux, &state);
    write_file(&state.join("session"), "legacy-session");
    write_file(&state.join("live"), "1");
    config(&config_path, free_port());
    write_file(&registry_path(&config_path), &json!({"legacy-session": {"session":"legacy-session","channel":"alerts","mention":"<@123>","keywords":["panic"],"keyword_window_secs":30,"stale_minutes":10,"format":"compact","active_wrapper_monitor":false}}).to_string());
    let discord = MockDiscord::start("success");
    let port = free_port();
    config(&config_path, port);
    let _daemon = start(&config_path, &home, &tmux, &discord, port);
    let api_projection = http_json(port, "GET", "/api/tmux", json!({}));
    assert!(api_projection.starts_with("HTTP/1.1 200"));
    assert!(api_projection.contains("legacy-session"));
    assert!(!api_projection.contains("thread_id"));
    let listed = lane_command(&config_path, &home, &tmux, &discord, &["tmux", "list"]);
    successful(&listed);
    let public = String::from_utf8_lossy(&listed.stdout);
    assert!(public.contains("legacy-session"));
    assert!(!public.contains("thread_id"));
    assert!(!public.contains("123456789012345678"));
}

#[test]
#[serial]
fn absent_lane_can_retire_then_replace_while_old_generation_is_fenced() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("home");
    let config_path = temp.path().join("config.toml");
    let state = temp.path().join("tmux");
    let tmux = temp.path().join("tmux.sh");
    fs::create_dir_all(&home).expect("home");
    fake_tmux(&tmux, &state);
    let discord = MockDiscord::start("success");
    let port = free_port();
    config(&config_path, port);
    let _daemon = start(&config_path, &home, &tmux, &discord, port);
    let first = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "replace-lane",
            "--thread",
            "123456789012345678",
            "--json",
        ],
    );
    successful(&first);
    let old: Value = serde_json::from_slice(&first.stdout).expect("first lane json");
    fs::remove_file(state.join("live")).expect("simulate missing tmux");
    let before = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &["lane", "status", "--session", "replace-lane", "--json"],
    );
    successful(&before);
    assert_eq!(
        serde_json::from_slice::<Value>(&before.stdout).expect("missing status")[0]["runtime"],
        "tmux-missing"
    );
    let retired = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "lane",
            "update",
            "--session",
            "replace-lane",
            "--message",
            "redacted",
            "--kind",
            "handoff",
            "--workflow",
            "retired",
            "--json",
        ],
    );
    successful(&retired);
    let replacement = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "replace-lane",
            "--thread",
            "123456789012345678",
            "--json",
        ],
    );
    successful(&replacement);
    let new: Value = serde_json::from_slice(&replacement.stdout).expect("replacement json");
    assert_ne!(old["generation_id"], new["generation_id"]);
    let current_revision = registry(&config_path)["replace-lane"]["lane"]["revision"].clone();
    assert!(
        current_revision.is_u64(),
        "durable replacement revision is present"
    );
    let stale = http_json(
        port,
        "POST",
        "/api/lane/workflow",
        json!({"session":"replace-lane","generation_id":old["generation_id"],"expected_revision":current_revision,"workflow":"active","quiesced":false}),
    );
    assert!(
        stale.starts_with("HTTP/1.1 409"),
        "stale generation must be fenced"
    );
}

#[test]
#[serial]
fn active_missing_and_handoff_missing_keep_distinct_status_semantics() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("home");
    let config_path = temp.path().join("config.toml");
    let state = temp.path().join("tmux");
    let tmux = temp.path().join("tmux.sh");
    fs::create_dir_all(&home).expect("home");
    fake_tmux(&tmux, &state);
    let discord = MockDiscord::start("success");
    let port = free_port();
    config(&config_path, port);
    let _daemon = start(&config_path, &home, &tmux, &discord, port);
    successful(&lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "workflow-lane",
            "--thread",
            "123456789012345678",
            "--json",
        ],
    ));
    fs::remove_file(state.join("live")).expect("simulate missing tmux");
    let active = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &["lane", "status", "--session", "workflow-lane", "--json"],
    );
    successful(&active);
    let active_json: Value = serde_json::from_slice(&active.stdout).expect("active status");
    assert_eq!(active_json[0]["runtime"], "tmux-missing");
    assert_eq!(active_json[0]["derived_status"], "tmux-missing");
    successful(&lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "lane",
            "update",
            "--session",
            "workflow-lane",
            "--message",
            "redacted",
            "--kind",
            "handoff",
            "--workflow",
            "awaiting-human",
            "--json",
        ],
    ));
    let handoff = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &["lane", "status", "--session", "workflow-lane", "--json"],
    );
    successful(&handoff);
    let handoff_json: Value = serde_json::from_slice(&handoff.stdout).expect("handoff status");
    assert_eq!(handoff_json[0]["derived_status"], "workflow-handoff");
    assert!(!String::from_utf8_lossy(&handoff.stdout).contains("123456789012345678"));
}

#[test]
#[serial]
fn invalid_thread_target_is_rejected_before_daemon_registration_or_effects() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("home");
    let config_path = temp.path().join("config.toml");
    let state = temp.path().join("tmux");
    let tmux = temp.path().join("tmux.sh");
    fs::create_dir_all(&home).expect("home");
    fake_tmux(&tmux, &state);
    let discord = MockDiscord::start("success");
    let port = free_port();
    config(&config_path, port);
    let _daemon = start(&config_path, &home, &tmux, &discord, port);
    let baseline = fs::read_to_string(state.join("calls")).unwrap_or_default();
    let output = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "invalid-target",
            "--thread",
            "not-a-snowflake",
            "--json",
        ],
    );
    failed(&output);
    assert_eq!(discord.count("POST"), 0);
    let delta = calls_after(&state, &baseline);
    assert!(
        !delta
            .lines()
            .any(|call| matches!(call, "new-session" | "set-option" | "send-keys"))
    );
    assert!(
        !registry_path(&config_path).is_file(),
        "invalid target must not register a lane"
    );
}

#[test]
#[serial]
fn session_creation_new_session_failure_is_ambiguous_without_command_submission() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("home");
    let config_path = temp.path().join("config.toml");
    let state = temp.path().join("tmux");
    let tmux = temp.path().join("tmux.sh");
    fs::create_dir_all(&home).expect("home");
    fake_tmux(&tmux, &state);
    write_file(&state.join("fail_new_session"), "1");
    let discord = MockDiscord::start("success");
    let port = free_port();
    config(&config_path, port);
    let _daemon = start(&config_path, &home, &tmux, &discord, port);
    let output = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "create-fault",
            "--thread",
            "123456789012345678",
            "--json",
        ],
    );
    failed(&output);
    let json: Value = serde_json::from_slice(&output.stdout).expect("fault json");
    assert_eq!(
        json["durable_launch_state"],
        "blocked-session-creation-ambiguous"
    );
    assert_eq!(json["exit_category"], "owner-aborted-before-r2");
    assert_eq!(count(&state, "sends"), 0);
}

#[test]
#[serial]
fn post_spawn_generation_identity_faults_are_durable_and_command_cleanup_differs_from_session_retention()
 {
    for (flag, category) in [
        ("fail_generation_set", "identity-marker-mismatch"),
        (
            "unreadable_clawhip_lane_generation",
            "identity-marker-read-failed",
        ),
        ("mismatch_generation", "identity-marker-mismatch"),
    ] {
        for command in [true, false] {
            let temp = TempDir::new().expect("temp");
            let home = temp.path().join("home");
            let config_path = temp.path().join("config.toml");
            let state = temp.path().join("tmux");
            let tmux = temp.path().join("tmux.sh");
            fs::create_dir_all(&home).expect("home");
            fake_tmux(&tmux, &state);
            write_file(&state.join(flag), "1");
            let discord = MockDiscord::start("success");
            let port = free_port();
            config(&config_path, port);
            let _daemon = start(&config_path, &home, &tmux, &discord, port);
            let mut args = vec![
                "tmux",
                "new",
                "--session",
                "identity-fault",
                "--thread",
                "123456789012345678",
                "--json",
            ];
            if command {
                args.extend(["--", "worker"]);
            }
            let output = lane_command(&config_path, &home, &tmux, &discord, &args);
            failed(&output);
            let json: Value = serde_json::from_slice(&output.stdout).expect("identity fault json");
            assert_eq!(json["exit_category"], category);
            assert_eq!(
                json["durable_launch_state"],
                if command {
                    "launch-failed-no-worker-effect"
                } else {
                    "blocked-session-creation-ambiguous"
                }
            );
            assert_eq!(count(&state, "sends"), 0);
            assert_eq!(
                state.join("live").is_file(),
                !command,
                "command cleanup removes R1 session while session effect is retained"
            );
        }
    }
}

#[test]
#[serial]
fn retained_target_mismatch_and_invalid_session_names_have_no_effects() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("home");
    let config_path = temp.path().join("config.toml");
    let state = temp.path().join("tmux");
    let tmux = temp.path().join("tmux.sh");
    fs::create_dir_all(&home).expect("home");
    fake_tmux(&tmux, &state);
    let discord = MockDiscord::start("success");
    let port = free_port();
    config(&config_path, port);
    let _daemon = start(&config_path, &home, &tmux, &discord, port);
    successful(&lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "retained-target",
            "--thread",
            "123456789012345678",
            "--json",
        ],
    ));
    let baseline = fs::read_to_string(state.join("calls")).unwrap_or_default();
    let mismatch = lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "retained-target",
            "--thread",
            "234567890123456789",
            "--json",
        ],
    );
    failed(&mismatch);
    assert!(!text(&mismatch).contains("234567890123456789"));
    assert!(
        calls_after(&state, &baseline)
            .lines()
            .all(|call| !matches!(call, "new-session" | "set-option" | "send-keys"))
    );
    let oversized = "x".repeat(129);
    for bad in ["bad/name", "bad?name", "bad\nname", oversized.as_str()] {
        let output = lane_command(
            &config_path,
            &home,
            &tmux,
            &discord,
            &[
                "tmux",
                "new",
                "--session",
                bad,
                "--thread",
                "123456789012345678",
                "--json",
            ],
        );
        failed(&output);
    }
}

#[test]
#[serial]
fn kickoff_update_preserves_original_receipt_and_records_latest_update() {
    let temp = TempDir::new().expect("temp");
    let home = temp.path().join("home");
    let config_path = temp.path().join("config.toml");
    let state = temp.path().join("tmux");
    let tmux = temp.path().join("tmux.sh");
    fs::create_dir_all(&home).expect("home");
    fake_tmux(&tmux, &state);
    let discord = MockDiscord::start("success");
    let port = free_port();
    config(&config_path, port);
    let _daemon = start(&config_path, &home, &tmux, &discord, port);
    successful(&lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "tmux",
            "new",
            "--session",
            "update-kickoff",
            "--thread",
            "123456789012345678",
            "--kickoff",
            "redacted",
            "--json",
            "--",
            "worker",
        ],
    ));
    let before = registry(&config_path)["update-kickoff"]["lane"]["kickoff_message_id"].clone();
    successful(&lane_command(
        &config_path,
        &home,
        &tmux,
        &discord,
        &[
            "lane",
            "update",
            "--session",
            "update-kickoff",
            "--message",
            "redacted",
            "--kind",
            "kickoff",
            "--json",
        ],
    ));
    let durable = registry(&config_path);
    let lane = &durable["update-kickoff"]["lane"];
    assert_eq!(lane["kickoff_message_id"], before);
    assert_eq!(lane["latest_update_kind"], "kickoff");
    assert_eq!(lane["latest_update_message_id"], "987654321098765432");
}

#[test]
#[serial]
fn response_loss_reconciles_each_lane_mutation_without_duplicate_effects() {
    for (drop_path, occurrence) in [
        ("/api/lane/register", 1),
        ("/api/lane/claim", 1),
        ("/api/lane/delivery", 1),
        ("/api/lane/evidence", 1),
        ("/api/lane/evidence", 2),
    ] {
        let temp = TempDir::new().expect("temp");
        let home = temp.path().join("home");
        let config_path = temp.path().join("config.toml");
        let state = temp.path().join("tmux");
        let tmux = temp.path().join("tmux.sh");
        fs::create_dir_all(&home).expect("home");
        fake_tmux(&tmux, &state);
        let discord = MockDiscord::start("success");
        let backend_port = free_port();
        let proxy = ResponseLossProxy::start(backend_port, drop_path, occurrence);
        config_with_base_url(&config_path, backend_port, proxy.port);
        let _daemon = start(&config_path, &home, &tmux, &discord, backend_port);
        let output = lane_command(
            &config_path,
            &home,
            &tmux,
            &discord,
            &[
                "tmux",
                "new",
                "--session",
                "loss-lane",
                "--thread",
                "123456789012345678",
                "--kickoff",
                "redacted",
                "--json",
                "--",
                "worker",
            ],
        );
        successful(&output);
        let result: Value = serde_json::from_slice(&output.stdout).expect("reconciled launch json");
        assert_eq!(result["durable_launch_state"], "launched");
        assert_eq!(discord.count("POST"), 1);
        assert_eq!(
            command_count(
                &fs::read_to_string(state.join("calls")).unwrap_or_default(),
                "new-session"
            ),
            1
        );
        assert_eq!(count(&state, "sends"), 1);
        assert_eq!(
            fs::read_to_string(state.join("clawhip_lane_command_submitted"))
                .expect("submitted marker")
                .trim(),
            result["launch_operation_id"]
                .as_str()
                .expect("launch operation")
        );
        let lane = &registry(&config_path)["loss-lane"]["lane"];
        assert_eq!(lane["launch_state"], "launched");
        assert!(lane["revision"].is_u64());
        assert_eq!(proxy.path_count("/api/lane/register"), 1);
        assert_eq!(proxy.path_count("/api/lane/claim"), 1);
        assert_eq!(proxy.path_count("/api/lane/delivery"), 1);
        assert_eq!(proxy.path_count("/api/lane/evidence"), 2);
    }
}
