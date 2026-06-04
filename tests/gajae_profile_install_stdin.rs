use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

fn clawhip_bin() -> &'static str {
    env!("CARGO_BIN_EXE_clawhip")
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write executable");
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod");
}

fn gajae_stub(temp: &TempDir, contents: &str) -> std::path::PathBuf {
    let stub = temp.path().join("gajae");
    write_executable(&stub, contents);
    stub
}

#[test]
fn gajae_profile_install_does_not_forward_parent_stdin() {
    let temp = TempDir::new().expect("tempdir");
    let stub = gajae_stub(
        &temp,
        r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" > "$GAJAE_ARG_LOG"
if IFS= read -r inherited_input; then
  printf 'unexpected stdin: %s\n' "$inherited_input" >&2
  exit 66
fi
printf 'profile installed\n'
printf 'install diagnostics\n' >&2
"#,
    );

    let mut child = Command::new(clawhip_bin())
        .args(["gajae", "profile", "install"])
        .env("GAJAE_BIN", &stub)
        .env("GAJAE_ARG_LOG", temp.path().join("gajae.args"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn clawhip");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"sensitive operator input\n")
        .expect("write stdin");

    let output = child.wait_with_output().expect("wait for clawhip");

    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("gajae.args")).expect("arg log"),
        "clawhip profile install\n"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "profile installed\n"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "install diagnostics\n"
    );
}

#[test]
fn gajae_profile_install_propagates_child_exit_code() {
    let temp = TempDir::new().expect("tempdir");
    let stub = gajae_stub(
        &temp,
        r#"#!/usr/bin/env bash
printf 'child failed\n' >&2
exit 23
"#,
    );

    let output = Command::new(clawhip_bin())
        .args(["gajae", "profile", "install"])
        .env("GAJAE_BIN", &stub)
        .output()
        .expect("run clawhip");

    assert_eq!(
        output.status.code(),
        Some(23),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("child failed"), "stderr={stderr}");
    assert!(stderr.contains("exit code 23"), "stderr={stderr}");
}

#[test]
fn gajae_receipt_ingest_maps_valid_receipt_to_public_safe_event() {
    let temp = TempDir::new().expect("tempdir");
    let receipt = temp.path().join("receipt.json");
    fs::write(&receipt, r#"{"raw":"private body"}"#).expect("write receipt");
    let stub = gajae_stub(
        &temp,
        r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" > "$GAJAE_ARG_LOG"
printf '{"receipt_id":"public-1","verdict":"hold","summary":"needs reviewer evidence","private_path":"/secret/home/token"}\n'
"#,
    );

    let output = Command::new(clawhip_bin())
        .args([
            "gajae",
            "receipt",
            "ingest",
            "--family",
            "merge-hold-decision",
            "--file",
            receipt.to_str().expect("receipt path"),
            "--channel",
            "ops",
        ])
        .env("GAJAE_BIN", &stub)
        .env("GAJAE_ARG_LOG", temp.path().join("gajae.args"))
        .output()
        .expect("run clawhip");

    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("gajae.args")).expect("arg log"),
        format!(
            "merge-hold-decision validate --file {}\n",
            receipt.display()
        )
    );
    let event: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("event json should parse");
    assert_eq!(event["type"], "gajae.merge.hold");
    assert_eq!(event["channel"], "ops");
    assert_eq!(event["payload"]["family"], "merge-hold-decision");
    assert_eq!(event["payload"]["receipt_id"], "public-1");
    assert_eq!(event["payload"]["verdict"], "hold");
    assert_eq!(event["payload"]["summary"], "needs reviewer evidence");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("private_path"),
        "stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn gajae_receipt_ingest_accepts_stdin_without_forwarding_raw_body() {
    let temp = TempDir::new().expect("tempdir");
    let stub = gajae_stub(
        &temp,
        r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" > "$GAJAE_ARG_LOG"
if IFS= read -r inherited_input; then
  printf 'unexpected stdin: %s\n' "$inherited_input" >&2
  exit 66
fi
receipt_file="${4:?missing file}"
printf '%s' "$receipt_file" > "$GAJAE_FILE_LOG"
stat -c '%a' "$receipt_file" > "$GAJAE_MODE_LOG"
printf '{"receipt_id":"stdin-1","summary":"stdin receipt ok"}\n'
"#,
    );

    let mut child = Command::new(clawhip_bin())
        .args([
            "gajae",
            "receipt",
            "ingest",
            "--family",
            "runtime-followup-receipt",
            "--stdin",
        ])
        .env("GAJAE_BIN", &stub)
        .env("GAJAE_ARG_LOG", temp.path().join("gajae.args"))
        .env("GAJAE_FILE_LOG", temp.path().join("gajae.file"))
        .env("GAJAE_MODE_LOG", temp.path().join("gajae.mode"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn clawhip");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"{\"secret\":\"do not route\"}\n")
        .expect("write stdin");

    let output = child.wait_with_output().expect("wait for clawhip");
    assert!(
        output.status.success(),
        "status={:?}\nstdout={}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("gajae.args")).expect("arg log"),
        "runtime-followup-receipt validate --file ".to_string()
            + &fs::read_to_string(temp.path().join("gajae.file")).expect("file log")
            + "\n"
    );
    let receipt_file = fs::read_to_string(temp.path().join("gajae.file")).expect("file log");
    assert_eq!(
        fs::read_to_string(temp.path().join("gajae.mode")).expect("mode log"),
        "600\n"
    );
    assert!(
        !Path::new(receipt_file.as_str()).exists(),
        "temporary stdin receipt file should be deleted: {receipt_file}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("gajae.receipt.validated"),
        "stdout={stdout}"
    );
    assert!(!stdout.contains("do not route"), "stdout={stdout}");
}

#[test]
fn gajae_receipt_ingest_failure_is_bounded_and_public_safe() {
    let temp = TempDir::new().expect("tempdir");
    let receipt = temp.path().join("receipt.json");
    fs::write(&receipt, r#"{"secret":"raw body"}"#).expect("write receipt");
    let stub = gajae_stub(
        &temp,
        r#"#!/usr/bin/env bash
printf '/secret/token/path %0500d\n' 1 >&2
exit 42
"#,
    );

    let output = Command::new(clawhip_bin())
        .args([
            "gajae",
            "receipt",
            "ingest",
            "--family",
            "runtime-followup-receipt",
            "--file",
            receipt.to_str().expect("receipt path"),
        ])
        .env("GAJAE_BIN", &stub)
        .output()
        .expect("run clawhip");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("gajae receipt validation failed"),
        "stderr={stderr}"
    );
    assert!(!stderr.contains("/secret/token/path"), "stderr={stderr}");
    assert!(stderr.len() < 420, "stderr too long: {}", stderr.len());
}
