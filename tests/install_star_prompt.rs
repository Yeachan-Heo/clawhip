use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn install_script() -> PathBuf {
    repo_root().join("install.sh")
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).expect("write file");
    let mut perms = fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod");
}

fn fake_gh_script() -> &'static str {
    r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$GH_LOG"
case "${1:-} ${2:-}" in
  "auth status")
    exit "${GH_AUTH_EXIT_CODE:-0}"
    ;;
  "api --method")
    exit "${GH_STAR_EXIT_CODE:-0}"
    ;;
esac
"#
}

fn run_shell(temp: &TempDir, script_body: &str, extra_env: &[(&str, &str)]) -> Output {
    let bin_dir = temp.path().join("bin");
    fs::create_dir_all(&bin_dir).expect("create bin dir");
    write_executable(&bin_dir.join("gh"), fake_gh_script());

    let script_path = temp.path().join("runner.sh");
    let script = format!(
        "#!/usr/bin/env bash\nset -euo pipefail\nsource \"{}\"\n{}\n",
        install_script().display(),
        script_body
    );
    write_executable(&script_path, &script);

    let existing_path = std::env::var("PATH").unwrap_or_default();
    let mut command = Command::new("bash");
    command.arg(script_path);
    command.env("PATH", format!("{}:{}", bin_dir.display(), existing_path));
    command.env("GH_LOG", temp.path().join("gh.log"));
    command.env("HOME", temp.path().join("home"));
    command.env("CARGO_HOME", temp.path().join("cargo"));
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().expect("run shell script")
}

#[test]
fn skips_star_prompt_when_not_interactive() {
    let temp = TempDir::new().expect("tempdir");
    let output = run_shell(
        &temp,
        r#"
can_use_github_cli_for_star() {
  echo invoked >> "$HOME/can-use.log"
  return 0
}
maybe_prompt_to_star_repo
"#,
        &[],
    );

    assert!(output.status.success(), "script failed: {output:?}");
    assert!(!temp.path().join("home/can-use.log").exists());
    assert!(!temp.path().join("gh.log").exists());
}

#[test]
fn skip_flag_or_env_disables_star_prompt() {
    let temp = TempDir::new().expect("tempdir");
    let output = run_shell(
        &temp,
        r#"
SKIP_STAR_PROMPT=1
is_interactive_install() {
  return 0
}
can_use_github_cli_for_star() {
  echo invoked >> "$HOME/can-use.log"
  return 0
}
maybe_prompt_to_star_repo <<'EOF_INPUT'
y
EOF_INPUT
"#,
        &[],
    );

    assert!(output.status.success(), "script failed: {output:?}");
    assert!(!temp.path().join("home/can-use.log").exists());
    assert!(!temp.path().join("gh.log").exists());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("skipping GitHub star prompt"),
        "stdout was: {stdout}"
    );
}

#[test]
fn skips_prompt_when_gh_is_unauthenticated() {
    let temp = TempDir::new().expect("tempdir");
    let output = run_shell(
        &temp,
        r#"
is_interactive_install() {
  return 0
}
maybe_prompt_to_star_repo
"#,
        &[("GH_AUTH_EXIT_CODE", "1")],
    );

    assert!(output.status.success(), "script failed: {output:?}");
    let gh_log = fs::read_to_string(temp.path().join("gh.log")).expect("gh log");
    assert_eq!(gh_log.trim(), "auth status");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("Would you like to star"),
        "stdout was: {stdout}"
    );
}

#[test]
fn stars_repo_only_after_explicit_yes() {
    let temp = TempDir::new().expect("tempdir");
    let output = run_shell(
        &temp,
        r#"
prompt_to_star_repo <<'EOF_INPUT'
y
EOF_INPUT
"#,
        &[],
    );

    assert!(output.status.success(), "script failed: {output:?}");
    let gh_log = fs::read_to_string(temp.path().join("gh.log")).expect("gh log");
    assert_eq!(
        gh_log.trim(),
        "api --method PUT /user/starred/Yeachan-Heo/clawhip --silent"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("thanks for starring"),
        "stdout was: {stdout}"
    );
}

#[test]
fn star_failure_does_not_fail_the_script() {
    let temp = TempDir::new().expect("tempdir");
    let output = run_shell(
        &temp,
        r#"
prompt_to_star_repo <<'EOF_INPUT'
yes
EOF_INPUT
echo after-prompt
"#,
        &[("GH_STAR_EXIT_CODE", "1")],
    );

    assert!(output.status.success(), "script failed: {output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("continuing without it"),
        "stdout was: {stdout}"
    );
    assert!(stdout.contains("after-prompt"), "stdout was: {stdout}");
}

#[test]
fn source_installer_records_checkout_without_mutating_explicit_config() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let config_dir = home.join(".clawhip");
    fs::create_dir_all(&config_dir).expect("create config dir");
    let config_path = config_dir.join("config.toml");
    let original = b"# operator-owned\n[update]\nrepo_root = \"/operator/explicit\"\n";
    fs::write(&config_path, original).expect("write config");

    let cargo_bin = temp.path().join("cargo/bin");
    fs::create_dir_all(&cargo_bin).expect("create cargo bin");
    fs::copy(env!("CARGO_BIN_EXE_clawhip"), cargo_bin.join("clawhip"))
        .expect("copy clawhip binary");

    let output = run_shell(&temp, "record_source_checkout", &[]);
    assert!(output.status.success(), "script failed: {output:?}");
    assert_eq!(fs::read(&config_path).expect("read config"), original);

    let state_dir = config_dir.join("source-checkout.d");
    let records = fs::read_dir(&state_dir)
        .expect("read managed state")
        .map(|entry| entry.expect("state entry"))
        .filter(|entry| entry.file_name() != ".lock")
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 1);
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(records[0].path()).expect("read managed checkout record"))
            .expect("parse managed checkout record");
    assert_eq!(
        state["repo_root"].as_str(),
        repo_root().canonicalize().unwrap().to_str()
    );

    let config = String::from_utf8(original.to_vec()).unwrap();
    assert!(config.contains("repo_root = \"/operator/explicit\""));
    assert!(!config.contains("enabled"));
    assert!(!config.contains("channel"));
}

#[test]
fn source_install_flow_records_checkout_after_cargo_install() {
    let temp = TempDir::new().expect("tempdir");
    let output = run_shell(
        &temp,
        r#"
cargo() {
  printf 'cargo\n' >> "$HOME/flow.log"
}
record_source_checkout() {
  printf 'record\n' >> "$HOME/flow.log"
}
mkdir -p "$HOME"
install_from_source
"#,
        &[],
    );

    assert!(output.status.success(), "script failed: {output:?}");
    assert_eq!(
        fs::read_to_string(temp.path().join("home/flow.log")).unwrap(),
        "cargo\nrecord\n"
    );
}

#[test]
fn source_install_flow_stops_when_checkout_persistence_fails() {
    let temp = TempDir::new().expect("tempdir");
    let output = run_shell(
        &temp,
        r#"
cargo() {
  return 0
}
record_source_checkout() {
  return 23
}
install_from_source
echo unsafe-continuation > "$HOME/continued"
"#,
        &[],
    );

    assert!(!output.status.success(), "script unexpectedly succeeded");
    assert!(!temp.path().join("home/continued").exists());
}
