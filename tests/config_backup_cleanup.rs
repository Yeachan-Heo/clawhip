use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn clawhip_bin() -> &'static str {
    env!("CARGO_BIN_EXE_clawhip")
}

fn run_setup(config_path: &std::path::Path, channel: &str) -> std::process::Output {
    Command::new(clawhip_bin())
        .env("HOME", config_path.parent().expect("config parent"))
        .arg("--config")
        .arg(config_path)
        .arg("setup")
        .arg("--bot-token")
        .arg("test-token")
        .arg("--default-channel")
        .arg(channel)
        .output()
        .expect("run clawhip setup")
}

#[cfg(unix)]
#[test]
fn config_backup_cleanup_converges_legacy_snapshots_on_changed_and_noop_saves() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("config.toml");

    let initial = run_setup(&config_path, "initial");
    assert!(
        initial.status.success(),
        "initial setup failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&initial.stdout),
        String::from_utf8_lossy(&initial.stderr)
    );

    for day in 1..=12 {
        fs::write(
            temp.path().join(format!("config.toml.bak-202001{day:02}")),
            format!("legacy-{day}"),
        )
        .expect("write legacy backup");
    }
    fs::write(
        temp.path().join("config.config.toml.bak-2020-01-13-0000"),
        "duplicated-family",
    )
    .expect("write duplicated family");
    fs::write(
        temp.path().join("config.toml.bak-unknown-format"),
        "unknown",
    )
    .expect("write unknown backup");

    let changed = run_setup(&config_path, "changed");
    assert!(
        changed.status.success(),
        "changed setup failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&changed.stdout),
        String::from_utf8_lossy(&changed.stderr)
    );
    assert!(
        fs::read_to_string(&config_path)
            .unwrap()
            .contains("changed")
    );
    assert!(
        fs::read_dir(temp.path().join(".clawhip-config-backups"))
            .expect("managed backups")
            .next()
            .is_some()
    );
    assert!(!temp.path().join("config.toml.bak-20200101").exists());
    assert!(temp.path().join("config.toml.bak-unknown-format").exists());
    let duplicated = temp.path().join("config.config.toml.bak-2020-01-13-0000");
    assert!(duplicated.exists());
    let noop_only = temp.path().join("config.toml.bak-20190101");
    fs::write(&noop_only, "no-op-only stale backup").expect("write no-op-only backup");
    assert!(noop_only.exists());

    let noop = run_setup(&config_path, "changed");
    assert!(
        noop.status.success(),
        "no-op setup failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&noop.stdout),
        String::from_utf8_lossy(&noop.stderr)
    );
    assert!(!noop_only.exists());
    assert!(temp.path().join("config.toml.bak-unknown-format").exists());
    assert!(duplicated.exists());
}

#[cfg(not(unix))]
#[test]
fn config_backup_cleanup_preserves_candidates_without_identity_proof() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("config.toml");
    let initial = run_setup(&config_path, "initial");
    assert!(initial.status.success());
    let legacy = temp.path().join("config.toml.bak-20200101");
    fs::write(&legacy, "legacy").expect("write legacy backup");

    let changed = run_setup(&config_path, "changed");
    assert!(
        changed.status.success(),
        "changed setup failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&changed.stdout),
        String::from_utf8_lossy(&changed.stderr)
    );
    assert!(legacy.exists());
    assert!(fs::read_to_string(config_path).unwrap().contains("changed"));
}
