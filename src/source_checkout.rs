#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::Result;

const STATE_DIR: &str = "source-checkout.d";
const LOCK_NAME: &str = ".lock";
const RECORD_PREFIX: &str = "state-";
const RECORD_SUFFIX: &str = ".json";
const MAX_RECORDS: usize = 16;
const MAX_STATE_BYTES: u64 = 16 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceCheckoutState {
    generation: u64,
    repo_root: String,
}

pub fn state_dir_for_config(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(STATE_DIR)
}

pub fn load(config_path: &Path) -> Result<Option<String>> {
    if let Err(error) = fs::symlink_metadata(state_dir_for_config(config_path)) {
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error.into());
    }
    platform::with_state_dir(config_path, false, |state_dir| {
        let records = platform::records(state_dir)?;
        let Some(record) = records.last() else {
            return Ok(None);
        };
        let raw = platform::read_record(state_dir, record)
            .map_err(|error| format!("failed to read managed record {record}: {error}"))?;
        let state: SourceCheckoutState = serde_json::from_slice(&raw)?;
        if record_generation(record) != Some(state.generation) {
            return Err("managed source checkout generation does not match its record name".into());
        }
        let canonical = validate_checkout(Path::new(&state.repo_root))
            .map_err(|error| format!("managed record {record} is invalid: {error}"))?;
        Ok(Some(path_string(&canonical)?))
    })
}

pub fn persist(config_path: &Path, repo_root: &Path) -> Result<()> {
    persist_with_hook(config_path, repo_root, &mut || Ok(()))
}

fn persist_with_hook<F>(config_path: &Path, repo_root: &Path, before_publish: &mut F) -> Result<()>
where
    F: FnMut() -> Result<()>,
{
    let canonical = validate_checkout(repo_root)?;
    platform::with_state_dir(config_path, true, |state_dir| {
        let _lock = platform::lock(state_dir)?;
        let records = platform::records(state_dir)?;
        let generation = records
            .last()
            .and_then(|record| record_generation(record))
            .unwrap_or(0)
            .checked_add(1)
            .ok_or("managed source checkout generation overflow")?;
        let state = SourceCheckoutState {
            generation,
            repo_root: path_string(&canonical)?,
        };
        let bytes = serde_json::to_vec_pretty(&state)?;
        let name = format!(
            "{RECORD_PREFIX}{generation:020}-{}{RECORD_SUFFIX}",
            uuid::Uuid::new_v4().simple()
        );
        platform::publish_record(state_dir, &name, &bytes, before_publish)?;
        platform::cleanup_records(state_dir, &records, MAX_RECORDS.saturating_sub(1))?;
        Ok(())
    })
}

fn record_generation(name: &str) -> Option<u64> {
    let body = name
        .strip_prefix(RECORD_PREFIX)?
        .strip_suffix(RECORD_SUFFIX)?;
    let (generation, uuid) = body.split_once('-')?;
    (generation.len() == 20
        && generation.chars().all(|ch| ch.is_ascii_digit())
        && uuid.len() == 32
        && uuid.chars().all(|ch| ch.is_ascii_hexdigit()))
    .then(|| generation.parse().ok())
    .flatten()
}

fn validate_checkout(path: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        format!(
            "managed repo_root '{}' is not readable: {error}",
            path.display()
        )
    })?;
    for marker in [canonical.join("Cargo.toml"), canonical.join("src")] {
        let metadata = fs::symlink_metadata(&marker)?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "checkout marker must not be a symlink: {}",
                marker.display()
            )
            .into());
        }
    }
    if !canonical.join("Cargo.toml").is_file() || !canonical.join("src").is_dir() {
        return Err(format!(
            "managed repo_root '{}' does not contain a clawhip checkout",
            canonical.display()
        )
        .into());
    }
    let manifest: toml::Value = toml::from_str(&fs::read_to_string(canonical.join("Cargo.toml"))?)?;
    if manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        != Some("clawhip")
    {
        return Err("managed repo_root Cargo package is not clawhip".into());
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(&canonical)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !output.status.success() {
        return Err("managed repo_root is not a git worktree".into());
    }
    let git_root = fs::canonicalize(String::from_utf8(output.stdout)?.trim())?;
    if git_root != canonical {
        return Err("managed repo_root must be the git worktree root".into());
    }
    Ok(canonical)
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "managed repo_root must be valid UTF-8".into())
}

mod platform {
    use super::*;

    pub struct StateDir {
        #[cfg(not(unix))]
        path: PathBuf,
        #[cfg(unix)]
        file: File,
    }

    pub struct StateLock(File);

    impl Drop for StateLock {
        fn drop(&mut self) {
            let _ = self.0.unlock();
        }
    }

    pub fn with_state_dir<T, F>(config_path: &Path, create: bool, operation: F) -> Result<T>
    where
        F: FnOnce(&StateDir) -> Result<T>,
    {
        let path = state_dir_for_config(config_path);
        if create {
            fs::create_dir_all(&path)?;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "managed source checkout state path is unsafe: {}",
                path.display()
            )
            .into());
        }
        #[cfg(unix)]
        let file = unix::open_directory(&path)?;
        operation(&StateDir {
            #[cfg(not(unix))]
            path,
            #[cfg(unix)]
            file,
        })
    }

    pub fn lock(dir: &StateDir) -> Result<StateLock> {
        #[cfg(unix)]
        let file = unix::open_lock(dir, LOCK_NAME)?;
        #[cfg(not(unix))]
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.path.join(LOCK_NAME))?;
        file.lock_exclusive()?;
        Ok(StateLock(file))
    }

    pub fn records(dir: &StateDir) -> Result<Vec<String>> {
        #[cfg(unix)]
        let read_path = PathBuf::from(format!(
            "/proc/self/fd/{}",
            std::os::fd::AsRawFd::as_raw_fd(&dir.file)
        ));
        #[cfg(not(unix))]
        let read_path = dir.path.clone();
        if !read_path.exists() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for entry in fs::read_dir(read_path)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if record_generation(&name).is_none() {
                continue;
            }
            #[cfg(unix)]
            unix::validate_record(dir, &name)?;
            #[cfg(not(unix))]
            {
                let metadata = fs::symlink_metadata(entry.path())?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(format!("managed source checkout record is unsafe: {name}").into());
                }
            }
            records.push(name);
        }
        records.sort();
        Ok(records)
    }

    pub fn read_record(dir: &StateDir, name: &str) -> Result<Vec<u8>> {
        #[cfg(unix)]
        return unix::read_record(dir, name);
        #[cfg(not(unix))]
        {
            let mut file = OpenOptions::new().read(true).open(dir.path.join(name))?;
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.len() > MAX_STATE_BYTES {
                return Err("managed source checkout record has invalid size or type".into());
            }
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        }
    }

    pub fn publish_record<F>(
        dir: &StateDir,
        name: &str,
        bytes: &[u8],
        before_publish: &mut F,
    ) -> Result<()>
    where
        F: FnMut() -> Result<()>,
    {
        #[cfg(unix)]
        return unix::publish_record(dir, name, bytes, before_publish);
        #[cfg(not(unix))]
        {
            let path = dir.path.join(name);
            let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
            file.write_all(bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            before_publish()?;
            File::open(&dir.path)?.sync_all()?;
            Ok(())
        }
    }

    pub fn cleanup_records(dir: &StateDir, records: &[String], retain_old: usize) -> Result<()> {
        let delete_count = records.len().saturating_sub(retain_old);
        for name in records.iter().take(delete_count) {
            #[cfg(unix)]
            unix::unlink_verified(dir, name)?;
            #[cfg(not(unix))]
            fs::remove_file(dir.path.join(name))?;
        }
        Ok(())
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::fs::MetadataExt;

        fn c_name(name: &str) -> Result<CString> {
            Ok(CString::new(name)?)
        }

        pub fn open_directory(path: &Path) -> Result<File> {
            let path = CString::new(path.as_os_str().as_encoded_bytes())?;
            let fd = unsafe {
                libc::open(
                    path.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(unsafe { File::from_raw_fd(fd) })
        }

        pub fn open_lock(dir: &StateDir, name: &str) -> Result<File> {
            let fd = unsafe {
                libc::openat(
                    dir.file.as_raw_fd(),
                    c_name(name)?.as_ptr(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            if !file.metadata()?.is_file() {
                return Err("managed source checkout lock is not a regular file".into());
            }
            Ok(file)
        }

        fn open_record(dir: &StateDir, name: &str) -> Result<File> {
            let fd = unsafe {
                libc::openat(
                    dir.file.as_raw_fd(),
                    c_name(name)?.as_ptr(),
                    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.len() > MAX_STATE_BYTES {
                return Err("managed source checkout record has invalid size or type".into());
            }
            Ok(file)
        }

        pub fn validate_record(dir: &StateDir, name: &str) -> Result<()> {
            open_record(dir, name).map(|_| ())
        }

        pub fn read_record(dir: &StateDir, name: &str) -> Result<Vec<u8>> {
            let mut file = open_record(dir, name)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        }

        pub fn publish_record<F>(
            dir: &StateDir,
            name: &str,
            bytes: &[u8],
            before_publish: &mut F,
        ) -> Result<()>
        where
            F: FnMut() -> Result<()>,
        {
            let dot = CString::new(".")?;
            let fd = unsafe {
                libc::openat(
                    dir.file.as_raw_fd(),
                    dot.as_ptr(),
                    libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut file = unsafe { File::from_raw_fd(fd) };
            file.write_all(bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            before_publish()?;
            let empty = CString::new("")?;
            let rc = unsafe {
                libc::linkat(
                    file.as_raw_fd(),
                    empty.as_ptr(),
                    dir.file.as_raw_fd(),
                    c_name(name)?.as_ptr(),
                    libc::AT_EMPTY_PATH,
                )
            };
            if rc != 0 {
                let proc_path = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
                if unsafe {
                    libc::linkat(
                        libc::AT_FDCWD,
                        proc_path.as_ptr(),
                        dir.file.as_raw_fd(),
                        c_name(name)?.as_ptr(),
                        libc::AT_SYMLINK_FOLLOW,
                    )
                } != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            dir.file.sync_all()?;
            Ok(())
        }

        pub fn unlink_verified(dir: &StateDir, name: &str) -> Result<()> {
            let file = open_record(dir, name)?;
            let expected = file.metadata()?;
            let mut current: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::fstatat(
                    dir.file.as_raw_fd(),
                    c_name(name)?.as_ptr(),
                    &mut current,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            if expected.dev() != current.st_dev || expected.ino() != current.st_ino {
                return Err("managed source checkout record changed before cleanup".into());
            }
            if unsafe { libc::unlinkat(dir.file.as_raw_fd(), c_name(name)?.as_ptr(), 0) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    fn checkout(root: &Path) {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"clawhip\"\n").unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("-q")
                .arg(root)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn fresh_sidecar_round_trips_canonical_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let checkout_path = dir.path().join("checkout");
        checkout(&checkout_path);
        let config_path = dir.path().join("config.toml");
        persist(&config_path, &checkout_path).unwrap();
        assert_eq!(
            load(&config_path).unwrap().as_deref(),
            checkout_path.to_str()
        );
    }

    #[test]
    fn explicit_config_overrides_managed_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("managed");
        checkout(&managed);
        let config_path = dir.path().join("config.toml");
        persist(&config_path, &managed).unwrap();
        fs::write(
            &config_path,
            "[update]\nrepo_root = \"/operator/explicit\"\n",
        )
        .unwrap();
        let config = AppConfig::load_or_default(&config_path).unwrap();
        assert_eq!(
            config.effective_update_repo_root(),
            Some("/operator/explicit")
        );
    }

    #[test]
    fn blank_explicit_value_uses_managed_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("managed");
        checkout(&managed);
        let config_path = dir.path().join("config.toml");
        persist(&config_path, &managed).unwrap();
        fs::write(&config_path, "[update]\nrepo_root = \"  \"\n").unwrap();
        let config = AppConfig::load_or_default(&config_path).unwrap();
        assert_eq!(config.effective_update_repo_root(), managed.to_str());
    }

    #[test]
    fn persisting_sidecar_leaves_operator_config_byte_stable() {
        let dir = tempfile::tempdir().unwrap();
        let checkout_path = dir.path().join("checkout");
        checkout(&checkout_path);
        let config_path = dir.path().join("config.toml");
        let original = "# operator comment\n[daemon]\nport = 25295 # keep\n";
        fs::write(&config_path, original).unwrap();
        persist(&config_path, &checkout_path).unwrap();
        assert_eq!(fs::read_to_string(&config_path).unwrap(), original);
        let config = AppConfig::load_or_default(&config_path).unwrap();
        assert_eq!(config.effective_update_repo_root(), checkout_path.to_str());
        assert!(!config.update.enabled);
        assert!(config.update.channel.is_none());
    }

    #[test]
    fn immutable_records_preserve_last_valid_state() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        checkout(&first);
        checkout(&second);
        let config_path = dir.path().join("config.toml");
        persist(&config_path, &first).unwrap();
        persist(&config_path, &second).unwrap();
        assert_eq!(load(&config_path).unwrap().as_deref(), second.to_str());
        assert_eq!(
            platform::with_state_dir(&config_path, false, platform::records)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn interrupted_checked_publish_preserves_last_valid_state() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        checkout(&first);
        checkout(&second);
        let config_path = dir.path().join("config.toml");
        persist(&config_path, &first).unwrap();

        assert!(
            persist_with_hook(&config_path, &second, &mut || {
                Err("injected interruption".into())
            })
            .is_err()
        );

        assert_eq!(load(&config_path).unwrap().as_deref(), first.to_str());
    }

    #[test]
    fn malformed_managed_state_can_be_repaired_by_lifecycle_loading_mode() {
        let dir = tempfile::tempdir().unwrap();
        let checkout_path = dir.path().join("checkout");
        checkout(&checkout_path);
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "# operator config\n").unwrap();
        fs::create_dir(state_dir_for_config(&config_path)).unwrap();
        fs::write(
            state_dir_for_config(&config_path)
                .join("state-00000000000000000001-00000000000000000000000000000001.json"),
            "not json",
        )
        .unwrap();
        assert!(AppConfig::load_or_default(&config_path).is_err());
        assert!(AppConfig::load_or_default_without_managed(&config_path).is_ok());

        persist(&config_path, &checkout_path).unwrap();

        assert_eq!(
            AppConfig::load_or_default(&config_path)
                .unwrap()
                .effective_update_repo_root(),
            checkout_path.to_str()
        );
    }

    #[test]
    fn cleanup_ignores_foreign_and_inflight_names() {
        let dir = tempfile::tempdir().unwrap();
        let checkout_path = dir.path().join("checkout");
        checkout(&checkout_path);
        let config_path = dir.path().join("config.toml");
        persist(&config_path, &checkout_path).unwrap();
        let foreign = state_dir_for_config(&config_path).join(".source-checkout.json.tmp-foreign");
        fs::write(&foreign, "foreign").unwrap();
        for _ in 0..MAX_RECORDS + 2 {
            persist(&config_path, &checkout_path).unwrap();
        }
        assert_eq!(fs::read_to_string(foreign).unwrap(), "foreign");
        assert!(
            platform::with_state_dir(&config_path, false, platform::records)
                .unwrap()
                .len()
                <= MAX_RECORDS
        );
    }

    #[cfg(unix)]
    #[test]
    fn bound_state_directory_survives_parent_path_swap() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let state_path = state_dir_for_config(&config_path);
        fs::create_dir(&state_path).unwrap();
        let moved = dir.path().join("moved-state");
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        platform::with_state_dir(&config_path, false, |state_dir| {
            fs::rename(&state_path, &moved)?;
            symlink(&outside, &state_path)?;
            platform::publish_record(
                state_dir,
                "state-00000000000000000001-00000000000000000000000000000001.json",
                br#"{"generation":1,"repo_root":"/unused"}"#,
                &mut || Ok(()),
            )
        })
        .unwrap();
        assert!(
            moved
                .join("state-00000000000000000001-00000000000000000000000000000001.json")
                .is_file()
        );
        assert!(fs::read_dir(outside).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_fifo_records_are_rejected_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::create_dir(state_dir_for_config(&config_path)).unwrap();
        let record = state_dir_for_config(&config_path)
            .join("state-00000000000000000001-00000000000000000000000000000001.json");
        let outside = dir.path().join("outside");
        fs::write(&outside, "{}").unwrap();
        symlink(&outside, &record).unwrap();
        assert!(load(&config_path).is_err());
        fs::remove_file(&record).unwrap();
        let path = CString::new(record.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(load(&config_path).is_err());
    }

    #[test]
    fn malformed_latest_record_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        fs::create_dir(state_dir_for_config(&config_path)).unwrap();
        fs::write(
            state_dir_for_config(&config_path)
                .join("state-00000000000000000001-00000000000000000000000000000001.json"),
            "not json",
        )
        .unwrap();
        assert!(load(&config_path).is_err());
    }

    #[test]
    fn invalid_checkout_and_marker_symlinks_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        assert!(persist(&config_path, dir.path()).is_err());
        assert!(!state_dir_for_config(&config_path).exists());
    }
}
