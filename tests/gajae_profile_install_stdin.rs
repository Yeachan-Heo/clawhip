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
