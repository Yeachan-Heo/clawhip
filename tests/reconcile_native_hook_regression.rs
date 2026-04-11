use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::Value;
use serial_test::serial;
use tempfile::TempDir;
use tokio::time::sleep;

fn clawhip_bin() -> &'static str {
    env!("CARGO_BIN_EXE_clawhip")
}

fn shell_escape_path(path: &Path) -> String {
    let value = path.display().to_string();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn write_file(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent directories");
    }
    fs::write(path, contents).expect("write file");
}

fn write_executable(path: &Path, contents: &str) {
    write_file(path, contents);
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod");
}

fn run_command(command: &mut Command) -> Output {
    command.output().expect("run command")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "expected failure\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_git_repo(repo: &Path) {
    fs::create_dir_all(repo).expect("create repo");
    git(repo, &["init"]);
}

fn fake_global_codex_install(home: &Path) {
    write_file(
        &home.join(".codex/hooks.json"),
        r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"node ~/.clawhip/hooks/native-hook.mjs --provider codex"}]}]}}"#,
    );
    write_file(
        &home.join(".clawhip/hooks/native-hook.mjs"),
        "function maybeWritePromptSubmitState() { return '.clawhip/state/prompt-submit.json'; }\n",
    );
}

fn write_fake_tmux(path: &Path, state_dir: &Path, pane_cwd: &Path, marker_path: &Path) {
    let marker_dir = marker_path.parent().expect("marker dir");
    write_executable(
        path,
        &format!(
            "#!/usr/bin/env bash\nset -euo pipefail\nSTATE_DIR={state}\nMARKER={marker}\nMARKER_DIR={marker_dir}\nCMD=\"$1\"\nshift\ncase \"$CMD\" in\n  display-message)\n    while [ $# -gt 0 ]; do\n      case \"$1\" in\n        -p) shift ;;\n        -t) shift 2 ;;\n        *) shift ;;\n      esac\n    done\n    printf 'issue-184\\t%%1\\t999999\\tcodex\\t%s\\n' {cwd}\n    ;;\n  capture-pane)\n    cat \"$STATE_DIR/capture.txt\" 2>/dev/null || true\n    ;;\n  send-keys)\n    literal=0\n    while [ $# -gt 0 ]; do\n      case \"$1\" in\n        -t) shift 2 ;;\n        -l) literal=1; shift; text=\"$1\"; shift ;;\n        *) key=\"$1\"; shift ;;\n      esac\n    done\n    if [ \"$literal\" -eq 1 ]; then\n      printf '%s\\n' \"$text\" > \"$STATE_DIR/prompt.txt\"\n      printf '%s\\n' \"$text\" > \"$STATE_DIR/capture.txt\"\n    else\n      count=$(cat \"$STATE_DIR/enters.txt\" 2>/dev/null || echo 0)\n      count=$((count + 1))\n      printf '%s' \"$count\" > \"$STATE_DIR/enters.txt\"\n      if [ \"$count\" -ge 2 ]; then\n        mkdir -p \"$MARKER_DIR\"\n        printf '{{\"attempt\":%s}}\\n' \"$count\" > \"$MARKER\"\n        printf 'submitted\\n' > \"$STATE_DIR/capture.txt\"\n      else\n        cat \"$STATE_DIR/prompt.txt\" > \"$STATE_DIR/capture.txt\"\n      fi\n    fi\n    ;;\n  *)\n    echo \"unsupported fake tmux command: $CMD\" >&2\n    exit 1\n    ;;\nesac\n",
            state = shell_escape_path(state_dir),
            marker = shell_escape_path(marker_path),
            marker_dir = shell_escape_path(marker_dir),
            cwd = shell_escape_path(pane_cwd),
        ),
    );
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

fn write_config(path: &Path, port: u16, routes_toml: &str) {
    write_file(
        path,
        &format!(
            "[daemon]\nbind_host = \"127.0.0.1\"\nport = {port}\n\n[defaults]\nchannel = \"default\"\nformat = \"compact\"\n\n{routes_toml}\n"
        ),
    );
}

fn spawn_daemon(config_path: &Path, home: &Path, port: u16) -> Child {
    Command::new(clawhip_bin())
        .arg("--config")
        .arg(config_path)
        .arg("start")
        .arg("--port")
        .arg(port.to_string())
        .env("HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon")
}

async fn wait_for_daemon(client: &Client, port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(response) = client
            .get(format!("http://127.0.0.1:{port}/health"))
            .send()
            .await
        {
            if response.status().is_success() {
                return;
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("daemon did not become healthy on port {port}");
}

#[test]
#[serial]
fn hooks_install_global_scope_keeps_state_in_home_only() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    init_git_repo(&repo);

    let output = run_command(
        Command::new(clawhip_bin())
            .arg("hooks")
            .arg("install")
            .arg("--provider")
            .arg("codex")
            .arg("--scope")
            .arg("global")
            .current_dir(&repo)
            .env("HOME", &home),
    );
    assert_success(&output);

    assert!(home.join(".clawhip/hooks/native-hook.mjs").is_file());
    assert!(home.join(".codex/hooks.json").is_file());
    assert!(!repo.join(".clawhip/project.json").exists());
    assert!(!repo.join(".codex/hooks.json").exists());
    assert!(!home.join(".clawhip/project.json").exists());
}

#[test]
#[serial]
fn hooks_install_project_scope_is_a_global_only_compat_shim() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    init_git_repo(&repo);

    let output = run_command(
        Command::new(clawhip_bin())
            .arg("hooks")
            .arg("install")
            .arg("--provider")
            .arg("codex")
            .arg("--scope")
            .arg("project")
            .arg("--root")
            .arg(&repo)
            .current_dir(&repo)
            .env("HOME", &home),
    );
    assert_success(&output);

    assert!(home.join(".clawhip/hooks/native-hook.mjs").is_file());
    assert!(home.join(".codex/hooks.json").is_file());
    assert!(!repo.join(".clawhip/project.json").exists());
    assert!(!repo.join(".clawhip/hooks/native-hook.mjs").exists());
    assert!(!repo.join(".codex/hooks.json").exists());
}

#[test]
#[serial]
fn explain_orders_route_matches_by_worktree_then_repo_then_name() {
    let temp = TempDir::new().expect("tempdir");
    let config = temp.path().join("config.toml");
    write_config(
        &config,
        25294,
        r#"[[routes]]
event = "session.*"
channel = "repo-name-route"
filter = { repo_name = "clawhip" }

[[routes]]
event = "session.*"
channel = "repo-path-route"
filter = { repo_path = "/repo/clawhip" }

[[routes]]
event = "session.*"
channel = "worktree-route"
filter = { worktree_path = "/repo/clawhip.worktrees/issue-152" }
"#,
    );

    let output = run_command(
        Command::new(clawhip_bin())
            .arg("--config")
            .arg(&config)
            .arg("explain")
            .arg("--json")
            .arg("session.started")
            .arg("--session_name")
            .arg("clawhip-issue-152")
            .arg("--repo_name")
            .arg("clawhip")
            .arg("--repo_path")
            .arg("/repo/clawhip")
            .arg("--worktree_path")
            .arg("/repo/clawhip.worktrees/issue-152"),
    );
    assert_success(&output);

    let provenance: Value =
        serde_json::from_slice(&output.stdout).expect("parse explain provenance");
    let deliveries = provenance["deliveries"]
        .as_array()
        .expect("deliveries array");
    let channels: Vec<&str> = deliveries
        .iter()
        .map(|delivery| delivery["channel"].as_str().expect("channel"))
        .collect();

    assert_eq!(
        channels,
        vec!["worktree-route", "repo-path-route", "repo-name-route"]
    );
}

#[test]
#[serial]
fn deliver_uses_global_hook_install_for_repo_sessions() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    let tmux_path = temp.path().join("fake-tmux.sh");
    let state_dir = temp.path().join("tmux-state");
    let marker_path = repo.join(".clawhip/state/prompt-submit.json");
    init_git_repo(&repo);
    fake_global_codex_install(&home);
    fs::create_dir_all(&state_dir).expect("create state dir");
    write_file(&state_dir.join("capture.txt"), "codex$ \n");
    write_fake_tmux(&tmux_path, &state_dir, &repo, &marker_path);

    let output = run_command(
        Command::new(clawhip_bin())
            .arg("deliver")
            .arg("--session")
            .arg("issue-184")
            .arg("--prompt")
            .arg("Ship the fix")
            .arg("--max-enters")
            .arg("3")
            .env("HOME", &home)
            .env("CLAWHIP_TMUX_BIN", &tmux_path),
    );
    assert_success(&output);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Delivered prompt"));
    assert!(marker_path.is_file());
}

#[test]
#[serial]
fn deliver_rejects_non_repo_sessions_even_with_global_hook_install() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let non_repo = temp.path().join("scratch");
    let tmux_path = temp.path().join("fake-tmux.sh");
    let state_dir = temp.path().join("tmux-state");
    let marker_path = non_repo.join(".clawhip/state/prompt-submit.json");
    fs::create_dir_all(&non_repo).expect("create non-repo");
    fake_global_codex_install(&home);
    fs::create_dir_all(&state_dir).expect("create state dir");
    write_file(&state_dir.join("capture.txt"), "codex$ \n");
    write_fake_tmux(&tmux_path, &state_dir, &non_repo, &marker_path);

    let output = run_command(
        Command::new(clawhip_bin())
            .arg("deliver")
            .arg("--session")
            .arg("issue-184")
            .arg("--prompt")
            .arg("Ship the fix")
            .arg("--max-enters")
            .arg("1")
            .env("HOME", &home)
            .env("CLAWHIP_TMUX_BIN", &tmux_path),
    );
    assert_failure(&output);

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("repo/workdir"), "{combined}");
}

#[tokio::test]
#[serial]
async fn daemon_drops_non_git_native_hook_events_before_routing() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let config = temp.path().join("config.toml");
    let non_repo = temp.path().join("scratch");
    fs::create_dir_all(&home).expect("create home");
    fs::create_dir_all(&non_repo).expect("create non-repo");
    let port = free_port();
    write_config(
        &config,
        port,
        r#"[[routes]]
event = "*"
webhook = "https://example.invalid/hook"
"#,
    );

    let mut child = spawn_daemon(&config, &home, port);
    let client = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    wait_for_daemon(&client, port).await;

    let response = client
        .post(format!("http://127.0.0.1:{port}/native/hook"))
        .json(&serde_json::json!({
            "provider": "codex",
            "event_name": "SessionStart",
            "directory": non_repo,
            "cwd": non_repo,
            "event_payload": {
                "cwd": non_repo
            }
        }))
        .send()
        .await
        .expect("post native hook");

    let status = response.status();
    let body: Value = response.json().await.expect("native hook json");

    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(status.as_u16(), 202, "{body}");
    assert_eq!(body["ok"], Value::Bool(true), "{body}");
    assert_eq!(body["dropped"], Value::Bool(true), "{body}");
    assert_eq!(body["reason"], Value::String("non_git".into()), "{body}");
}
