#![cfg(unix)]

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

fn install_test_binary(temp: &TempDir) {
    let cargo_bin = temp.path().join("cargo/bin");
    fs::create_dir_all(&cargo_bin).expect("create cargo bin");
    fs::copy(env!("CARGO_BIN_EXE_clawhip"), cargo_bin.join("clawhip"))
        .expect("copy clawhip binary");
}

fn run_direct_install(temp: &TempDir, config_path: &Path, home: &Path) -> Output {
    let bin_dir = temp.path().join("direct-install-bin");
    fs::create_dir_all(&bin_dir).expect("create direct install bin");
    write_executable(
        &bin_dir.join("cargo"),
        "#!/usr/bin/env bash\nset -euo pipefail\nprintf '%s\\n' \"$*\" >> \"$CARGO_LOG\"\n",
    );
    let existing_path = std::env::var("PATH").unwrap_or_default();
    Command::new(env!("CARGO_BIN_EXE_clawhip"))
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "install",
            "--skip-star-prompt",
        ])
        .current_dir(repo_root())
        .env("PATH", format!("{}:{existing_path}", bin_dir.display()))
        .env("HOME", home)
        .env("CARGO_HOME", temp.path().join("cargo"))
        .env("CARGO_LOG", temp.path().join("cargo.log"))
        .output()
        .expect("run direct clawhip install")
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
    command.current_dir(temp.path());
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
fn source_installer_preserves_relative_custom_config_with_unavailable_home() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("custom/nested");
    fs::create_dir_all(&config_dir).expect("create config dir");
    let config_path = config_dir.join("config.toml");
    let original = b"# operator-owned\n[update]\nrepo_root = \"/operator/explicit\"\n";
    fs::write(&config_path, original).expect("write config");
    let unavailable_home = temp.path().join("unavailable-home");
    fs::write(&unavailable_home, "not a directory").expect("write unavailable home");

    install_test_binary(&temp);

    let output = run_shell(
        &temp,
        "record_source_checkout",
        &[
            ("CLAWHIP_CONFIG", "custom/nested/config.toml"),
            ("HOME", unavailable_home.to_str().unwrap()),
        ],
    );
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
fn source_installer_creates_nested_selected_config_parent() {
    let temp = TempDir::new().expect("tempdir");
    let unavailable_home = temp.path().join("unavailable-home");
    fs::write(&unavailable_home, "not a directory").expect("write unavailable home");
    install_test_binary(&temp);

    let output = run_shell(
        &temp,
        "record_source_checkout",
        &[
            ("CLAWHIP_CONFIG", "fresh/deep/config.toml"),
            ("HOME", unavailable_home.to_str().unwrap()),
        ],
    );
    assert!(output.status.success(), "script failed: {output:?}");

    let config_path = temp.path().join("fresh/deep/config.toml");
    assert!(!config_path.exists(), "operator config must not be created");
    let state_dir = temp.path().join("fresh/deep/source-checkout.d");
    let records = fs::read_dir(state_dir)
        .expect("read managed state")
        .map(|entry| entry.expect("state entry"))
        .filter(|entry| entry.file_name() != ".lock")
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 1);
}

#[test]
fn full_source_script_flow_uses_only_selected_custom_config_dir() {
    let temp = TempDir::new().expect("tempdir");
    let unavailable_home = temp.path().join("unavailable-home");
    fs::write(&unavailable_home, "not a directory").expect("write unavailable home");
    let config_path = temp.path().join("full/custom/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let original = b"# operator-owned\n[update]\nrepo_root = \"/operator/explicit\"\n";
    fs::write(&config_path, original).unwrap();
    install_test_binary(&temp);

    let output = run_shell(
        &temp,
        r#"
install_prebuilt_binary() {
  return 1
}
install_from_source() {
  record_source_checkout
}
main --skip-star-prompt
"#,
        &[
            ("CLAWHIP_CONFIG", "full/custom/config.toml"),
            ("HOME", unavailable_home.to_str().unwrap()),
        ],
    );

    assert!(output.status.success(), "script failed: {output:?}");
    assert_eq!(fs::read(&config_path).unwrap(), original);
    assert!(
        config_path
            .parent()
            .unwrap()
            .join("source-checkout.d")
            .is_dir()
    );
    assert!(config_path.parent().unwrap().join("plugins").is_dir());
    assert_eq!(
        fs::read_to_string(&unavailable_home).unwrap(),
        "not a directory"
    );
}

#[test]
fn direct_install_creates_nested_custom_parent_without_default_home() {
    let temp = TempDir::new().expect("tempdir");
    let unavailable_home = temp.path().join("unavailable-home");
    fs::write(&unavailable_home, "not a directory").expect("write unavailable home");
    let config_path = temp.path().join("direct/fresh/nested/config.toml");

    let output = run_direct_install(&temp, &config_path, &unavailable_home);
    assert!(output.status.success(), "install failed: {output:?}");
    assert!(!config_path.exists(), "operator config must not be created");
    assert!(
        config_path
            .parent()
            .unwrap()
            .join("source-checkout.d")
            .is_dir()
    );
    assert_eq!(
        fs::read_to_string(&unavailable_home).unwrap(),
        "not a directory"
    );
}

#[test]
fn direct_install_preserves_custom_config_bytes_and_update_defaults() {
    let temp = TempDir::new().expect("tempdir");
    let unavailable_home = temp.path().join("unavailable-home");
    fs::write(&unavailable_home, "not a directory").expect("write unavailable home");
    let config_path = temp.path().join("direct/existing/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let original = b"# operator-owned\n[update]\nrepo_root = \"/operator/explicit\"\n";
    fs::write(&config_path, original).unwrap();

    let output = run_direct_install(&temp, &config_path, &unavailable_home);
    assert!(output.status.success(), "install failed: {output:?}");
    assert_eq!(fs::read(&config_path).unwrap(), original);
    assert!(
        config_path
            .parent()
            .unwrap()
            .join("source-checkout.d")
            .is_dir()
    );
    let config = String::from_utf8(original.to_vec()).unwrap();
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
  if [[ "${1:-}" == "pkgid" ]]; then
    printf 'path+file:///checkout#clawhip@0.0.0\n'
    return 0
  fi
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
  if [[ "${1:-}" == "pkgid" ]]; then
    printf 'path+file:///checkout#clawhip@0.0.0\n'
    return 0
  fi
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

#[test]
fn source_install_rejects_wrong_package_before_cargo_install() {
    let temp = TempDir::new().expect("tempdir");
    let output = run_shell(
        &temp,
        r#"
cargo() {
  if [[ "${1:-}" == "pkgid" ]]; then
    printf 'path+file:///checkout#not-clawhip@0.0.0\n'
    return 0
  fi
  echo invoked > "$HOME/cargo-install-called"
}
mkdir -p "$HOME"
install_from_source
"#,
        &[],
    );

    assert!(!output.status.success(), "script unexpectedly succeeded");
    assert!(!temp.path().join("home/cargo-install-called").exists());
}
