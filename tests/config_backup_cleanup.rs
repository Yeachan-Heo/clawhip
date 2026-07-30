use std::fs;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::Read;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Stdio;
use std::process::{Command, Output};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::thread;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn clawhip_bin() -> &'static str {
    env!("CARGO_BIN_EXE_clawhip")
}

fn setup_command(config_path: &Path, channel: &str) -> Command {
    let mut command = Command::new(clawhip_bin());
    command
        .env("HOME", config_path.parent().expect("config parent"))
        .arg("--config")
        .arg(config_path)
        .arg("setup")
        .arg("--bot-token")
        .arg("test-token")
        .arg("--default-channel")
        .arg(channel);
    command
}

fn run_setup(config_path: &Path, channel: &str) -> Output {
    setup_command(config_path, channel)
        .output()
        .expect("run clawhip setup")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_setup_bounded(config_path: &Path, channel: &str) -> Output {
    let mut command = setup_command(config_path, channel);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    if unsafe { libc::geteuid() } == 0 {
        command.gid(65534).uid(65534);
    }
    let mut child = command.spawn().expect("spawn clawhip setup");
    let mut stdout = child.stdout.take().expect("setup stdout");
    let mut stderr = child.stderr.take().expect("setup stderr");
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).expect("read setup stdout");
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).expect("read setup stderr");
        bytes
    });
    let deadline = Instant::now() + Duration::from_secs(10);

    loop {
        if let Some(status) = child.try_wait().expect("poll clawhip setup") {
            return Output {
                status,
                stdout: stdout_reader.join().expect("join stdout reader"),
                stderr: stderr_reader.join().expect("join stderr reader"),
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child.wait().expect("reap timed out clawhip setup");
            let stdout = stdout_reader.join().expect("join timed out stdout reader");
            let stderr = stderr_reader.join().expect("join timed out stderr reader");
            panic!(
                "clawhip setup timed out with {status}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn managed_backup_state(config_path: &Path) -> Vec<(String, Vec<u8>)> {
    let backup_dir = config_path
        .parent()
        .expect("config parent")
        .join(".clawhip-config-backups");
    let mut entries = fs::read_dir(backup_dir)
        .expect("managed backup directory")
        .map(|entry| {
            let entry = entry.expect("managed backup entry");
            (
                entry.file_name().to_string_lossy().into_owned(),
                fs::read(entry.path()).expect("managed backup bytes"),
            )
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct PermissionRestore {
    path: PathBuf,
    mode: u32,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for PermissionRestore {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(self.mode));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn config_backup_cleanup_preserves_mode_000_candidate_on_changed_and_noop_setup() {
    let temp = TempDir::new().expect("tempdir");
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).expect("chmod temp root");
    let home = temp.path().join("home");
    fs::create_dir(&home).expect("create config home");
    fs::set_permissions(&home, fs::Permissions::from_mode(0o777)).expect("chmod config home");
    let config_path = home.join("config.toml");
    let outside = temp.path().join("outside-sentinel");
    fs::write(&outside, "outside").expect("write outside sentinel");

    let initial = run_setup_bounded(&config_path, "initial");
    assert!(
        initial.status.success(),
        "initial setup failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&initial.stdout),
        String::from_utf8_lossy(&initial.stderr)
    );

    for day in 1..=11 {
        fs::write(
            home.join(format!("config.toml.bak-202001{day:02}")),
            format!("legacy-{day}"),
        )
        .expect("write legacy backup");
    }
    let candidate = home.join("config.toml.bak-20200101");
    let candidate_bytes = fs::read(&candidate).expect("candidate bytes");
    let candidate_metadata = fs::metadata(&candidate).expect("candidate metadata");
    let candidate_identity = (candidate_metadata.dev(), candidate_metadata.ino());
    let candidate_mode = candidate_metadata.permissions().mode();
    let restore = PermissionRestore {
        path: candidate.clone(),
        mode: candidate_mode,
    };
    fs::set_permissions(&candidate, fs::Permissions::from_mode(0o000))
        .expect("make candidate unreadable");

    let changed = run_setup_bounded(&config_path, "changed");
    let changed_output = format!(
        "{}{}",
        String::from_utf8_lossy(&changed.stdout),
        String::from_utf8_lossy(&changed.stderr)
    );
    assert!(
        changed.status.success(),
        "changed setup failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&changed.stdout),
        String::from_utf8_lossy(&changed.stderr)
    );
    assert!(String::from_utf8_lossy(&changed.stdout).contains("Saved "));
    assert!(!changed_output.contains("backup retention cleanup remains incomplete"));
    let changed_bytes = fs::read(&config_path).expect("changed config bytes");
    assert!(String::from_utf8_lossy(&changed_bytes).contains("changed"));
    let changed_backups = managed_backup_state(&config_path);

    let noop = run_setup_bounded(&config_path, "changed");
    let noop_output = format!(
        "{}{}",
        String::from_utf8_lossy(&noop.stdout),
        String::from_utf8_lossy(&noop.stderr)
    );
    assert!(
        noop.status.success(),
        "no-op setup failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&noop.stdout),
        String::from_utf8_lossy(&noop.stderr)
    );
    assert!(String::from_utf8_lossy(&noop.stdout).contains("Saved "));
    assert!(!noop_output.contains("backup retention cleanup remains incomplete"));
    assert_eq!(fs::read(&config_path).unwrap(), changed_bytes);
    assert_eq!(managed_backup_state(&config_path), changed_backups);

    drop(restore);
    let preserved_metadata = fs::metadata(&candidate).expect("preserved candidate metadata");
    assert_eq!(
        (preserved_metadata.dev(), preserved_metadata.ino()),
        candidate_identity
    );
    assert_eq!(fs::read(&candidate).unwrap(), candidate_bytes);
    assert_eq!(fs::read_to_string(&outside).unwrap(), "outside");
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
