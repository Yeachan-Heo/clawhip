#[cfg(not(any(target_os = "linux", target_os = "android")))]
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
        let _lock = platform::lock_shared(state_dir)?;
        let records = platform::records(state_dir)?;
        let mut latest = None;
        for record in records {
            latest = Some(load_managed_record(state_dir, &record)?);
        }
        Ok(latest)
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
        let records = platform::record_names(state_dir)?;
        let mut valid_records = Vec::with_capacity(records.len());
        for record in records {
            if record_generation(&record) != Some(u64::MAX)
                && managed_record_is_valid(state_dir, &record)
            {
                valid_records.push(record);
            } else {
                platform::cleanup_records(state_dir, &[record], 0)?;
            }
        }
        let generation = valid_records
            .iter()
            .filter_map(|record| record_generation(record))
            .max()
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
        platform::cleanup_records(state_dir, &valid_records, MAX_RECORDS.saturating_sub(1))?;
        Ok(())
    })
}

fn managed_record_is_valid(state_dir: &platform::StateDir, record: &str) -> bool {
    load_managed_record(state_dir, record).is_ok()
}

fn load_managed_record(state_dir: &platform::StateDir, record: &str) -> Result<String> {
    let raw = platform::read_record(state_dir, record)
        .map_err(|error| format!("failed to read managed record {record}: {error}"))?;
    let state: SourceCheckoutState = serde_json::from_slice(&raw)?;
    if record_generation(record) != Some(state.generation) {
        return Err("managed source checkout generation does not match its record name".into());
    }
    let canonical = validate_checkout(Path::new(&state.repo_root))
        .map_err(|error| format!("managed record {record} is invalid: {error}"))?;
    path_string(&canonical)
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

pub(crate) fn validate_checkout(path: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        format!(
            "managed repo_root '{}' is not readable: {error}",
            path.display()
        )
    })?;
    if !canonical.join("Cargo.toml").is_file() || !canonical.join("src").is_dir() {
        return Err(format!(
            "managed repo_root '{}' does not contain a clawhip checkout",
            canonical.display()
        )
        .into());
    }
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
    let manifest: toml::Value = toml::from_str(&fs::read_to_string(canonical.join("Cargo.toml"))?)?;
    if manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        != Some("clawhip")
    {
        return Err("managed repo_root Cargo package is not clawhip".into());
    }
    let output = isolated_git_command()
        .arg("-C")
        .arg(&canonical)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !output.status.success() {
        return Err("managed repo_root is not a git worktree".into());
    }
    let git_output = String::from_utf8(output.stdout)?;
    let git_root = fs::canonicalize(git_output.trim_end_matches(['\r', '\n']))?;
    if git_root != canonical {
        return Err("managed repo_root must be the git worktree root".into());
    }
    Ok(canonical)
}

pub(crate) fn isolated_git_command() -> Command {
    let mut command = Command::new("git");
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_EXEC_PATH",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_ASKPASS",
        "GIT_PROXY_COMMAND",
    ] {
        command.env_remove(variable);
    }
    command
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "managed repo_root must be valid UTF-8".into())
}

mod platform {
    use super::*;

    pub struct StateDir {
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        path: PathBuf,
        #[cfg(any(target_os = "linux", target_os = "android"))]
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
            create_state_dir(&path)?;
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
        validate_state_dir_trust(&path, &metadata)?;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let file = linux::open_directory(&path)?;
        operation(&StateDir {
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            path,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            file,
        })
    }

    #[cfg(unix)]
    fn create_state_dir(path: &Path) -> Result<()> {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(not(unix))]
    fn create_state_dir(path: &Path) -> Result<()> {
        match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(unix)]
    fn validate_state_dir_trust(path: &Path, metadata: &fs::Metadata) -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
            return Err(format!(
                "managed source checkout state directory must be owned by the current user with mode 0700: {}",
                path.display()
            )
            .into());
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn validate_state_dir_trust(_path: &Path, _metadata: &fs::Metadata) -> Result<()> {
        Ok(())
    }

    pub fn lock(dir: &StateDir) -> Result<StateLock> {
        lock_file(dir, true, true)
    }

    pub fn lock_shared(dir: &StateDir) -> Result<StateLock> {
        lock_file(dir, false, false)
    }

    fn lock_file(dir: &StateDir, create: bool, exclusive: bool) -> Result<StateLock> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let file = linux::open_lock(dir, LOCK_NAME, create)?;
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;

            let mut options = OpenOptions::new();
            options
                .read(true)
                .write(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            options.create(create).open(dir.path.join(LOCK_NAME))?
        };
        #[cfg(windows)]
        let file = {
            let path = dir.path.join(LOCK_NAME);
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                    return Err("managed source checkout lock is not a regular file".into());
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {}
                Err(error) => return Err(error.into()),
            }
            let mut options = OpenOptions::new();
            options
                .read(true)
                .write(true)
                .truncate(false)
                .create(create);
            options.open(path)?
        };
        if !file.metadata()?.is_file() {
            return Err("managed source checkout lock is not a regular file".into());
        }
        validate_private_file(&file.metadata()?, "lock")?;
        if exclusive {
            file.lock_exclusive()?;
        } else {
            FileExt::lock_shared(&file)?;
        }
        Ok(StateLock(file))
    }

    #[cfg(unix)]
    fn validate_private_file(metadata: &fs::Metadata, kind: &str) -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(format!(
                "managed source checkout {kind} must be a current-user-owned, singly linked mode 0600 regular file"
            )
            .into());
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn validate_private_file(_metadata: &fs::Metadata, _kind: &str) -> Result<()> {
        Ok(())
    }

    pub fn records(dir: &StateDir) -> Result<Vec<String>> {
        let records = record_names(dir)?;
        for name in &records {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            linux::validate_record(dir, name)?;
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            {
                let metadata = fs::symlink_metadata(dir.path.join(name))?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(format!("managed source checkout record is unsafe: {name}").into());
                }
            }
        }
        Ok(records)
    }

    pub fn record_names(dir: &StateDir) -> Result<Vec<String>> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return linux::record_names(dir);
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let read_path = dir.path.clone();
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let mut records = Vec::new();
            for entry in fs::read_dir(read_path)? {
                let entry = entry?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if record_generation(&name).is_none() {
                    continue;
                }
                records.push(name);
            }
            records.sort();
            Ok(records)
        }
    }

    pub fn read_record(dir: &StateDir, name: &str) -> Result<Vec<u8>> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return linux::read_record(dir, name);
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;

            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(dir.path.join(name))?
        };
        #[cfg(windows)]
        let mut file = {
            let path = dir.path.join(name);
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err("managed source checkout record has invalid size or type".into());
            }
            OpenOptions::new().read(true).open(path)?
        };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.len() > MAX_STATE_BYTES {
                return Err("managed source checkout record has invalid size or type".into());
            }
            validate_private_file(&metadata, "record")?;
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
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return linux::publish_record(dir, name, bytes, before_publish);
        #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
        {
            use std::os::unix::fs::OpenOptionsExt;

            let temp_name = format!(".{name}.tmp-{}", uuid::Uuid::new_v4().simple());
            let temp_path = dir.path.join(&temp_name);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temp_path)?;
            file.write_all(bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            if let Err(error) = before_publish() {
                let _ = fs::remove_file(&temp_path);
                return Err(error);
            }
            let publish_result = fs::hard_link(&temp_path, dir.path.join(name));
            let cleanup_result = fs::remove_file(temp_path);
            publish_result?;
            cleanup_result?;
            File::open(&dir.path)?.sync_all()?;
            Ok(())
        }
        #[cfg(windows)]
        {
            let temp_path = dir
                .path
                .join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4().simple()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            file.write_all(bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            if let Err(error) = before_publish() {
                let _ = fs::remove_file(&temp_path);
                return Err(error);
            }
            if let Err(error) = fs::rename(&temp_path, dir.path.join(name)) {
                let _ = fs::remove_file(&temp_path);
                return Err(error.into());
            }
            Ok(())
        }
    }

    pub fn cleanup_records(dir: &StateDir, records: &[String], retain_old: usize) -> Result<()> {
        let delete_count = records.len().saturating_sub(retain_old);
        for name in records.iter().take(delete_count) {
            unlink_verified(dir, name)?;
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn unlink_verified(dir: &StateDir, name: &str) -> Result<()> {
        linux::unlink_verified(dir, name)
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    fn unlink_verified(dir: &StateDir, name: &str) -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        let path = dir.path.join(name);
        let expected = fs::symlink_metadata(&path)?;
        let current = fs::symlink_metadata(&path)?;
        if expected.dev() != current.dev() || expected.ino() != current.ino() {
            return Err("managed source checkout record changed before cleanup".into());
        }
        fs::remove_file(path)?;
        Ok(())
    }

    #[cfg(windows)]
    fn unlink_verified(dir: &StateDir, name: &str) -> Result<()> {
        fs::remove_file(dir.path.join(name))?;
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    mod linux {
        use super::*;
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};

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

        pub fn open_lock(dir: &StateDir, name: &str, create: bool) -> Result<File> {
            let create_flag = if create { libc::O_CREAT } else { 0 };
            let fd = unsafe {
                libc::openat(
                    dir.file.as_raw_fd(),
                    c_name(name)?.as_ptr(),
                    libc::O_RDWR
                        | create_flag
                        | libc::O_NONBLOCK
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err("managed source checkout lock is not a regular file".into());
            }
            validate_private_file(&metadata, "lock")?;
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
            validate_private_file(&metadata, "record")?;
            Ok(file)
        }

        pub fn validate_record(dir: &StateDir, name: &str) -> Result<()> {
            open_record(dir, name).map(|_| ())
        }

        pub fn record_names(dir: &StateDir) -> Result<Vec<String>> {
            use std::ffi::CStr;

            let dot = CString::new(".")?;
            let fd = unsafe {
                libc::openat(
                    dir.file.as_raw_fd(),
                    dot.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let stream = unsafe { libc::fdopendir(fd) };
            if stream.is_null() {
                let error = std::io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(error.into());
            }
            let mut records = Vec::new();
            loop {
                set_errno(0);
                let entry = unsafe { libc::readdir(stream) };
                if entry.is_null() {
                    let error = errno();
                    unsafe { libc::closedir(stream) };
                    if error != 0 {
                        return Err(std::io::Error::from_raw_os_error(error).into());
                    }
                    break;
                }
                let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
                let Ok(name) = name.to_str() else {
                    continue;
                };
                if record_generation(name).is_some() {
                    records.push(name.to_owned());
                }
            }
            records.sort();
            Ok(records)
        }

        #[cfg(target_os = "linux")]
        fn errno() -> i32 {
            unsafe { *libc::__errno_location() }
        }

        #[cfg(target_os = "linux")]
        fn set_errno(value: i32) {
            unsafe { *libc::__errno_location() = value };
        }

        #[cfg(target_os = "android")]
        fn errno() -> i32 {
            unsafe { *libc::__errno() }
        }

        #[cfg(target_os = "android")]
        fn set_errno(value: i32) {
            unsafe { *libc::__errno() = value };
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
                let error = std::io::Error::last_os_error();
                if error.raw_os_error().is_some_and(|code| {
                    [libc::EOPNOTSUPP, libc::EINVAL, libc::EISDIR, libc::ENOSYS].contains(&code)
                }) {
                    return publish_named_temp(dir, name, bytes, before_publish, true);
                }
                return Err(error.into());
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
                    return publish_named_temp(dir, name, bytes, before_publish, false);
                }
            }
            dir.file.sync_all()?;
            Ok(())
        }

        fn publish_named_temp<F>(
            dir: &StateDir,
            name: &str,
            bytes: &[u8],
            before_publish: &mut F,
            run_hook: bool,
        ) -> Result<()>
        where
            F: FnMut() -> Result<()>,
        {
            let temp_name = format!(".{name}.tmp-{}", uuid::Uuid::new_v4().simple());
            let temp = c_name(&temp_name)?;
            let fd = unsafe {
                libc::openat(
                    dir.file.as_raw_fd(),
                    temp.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut file = unsafe { File::from_raw_fd(fd) };
            let operation = (|| -> Result<()> {
                file.write_all(bytes)?;
                file.write_all(b"\n")?;
                file.sync_all()?;
                if run_hook {
                    before_publish()?;
                }
                if unsafe {
                    libc::linkat(
                        dir.file.as_raw_fd(),
                        temp.as_ptr(),
                        dir.file.as_raw_fd(),
                        c_name(name)?.as_ptr(),
                        0,
                    )
                } != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                Ok(())
            })();
            let cleanup = unsafe { libc::unlinkat(dir.file.as_raw_fd(), temp.as_ptr(), 0) };
            operation?;
            if cleanup != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            dir.file.sync_all()?;
            Ok(())
        }

        pub fn unlink_verified(dir: &StateDir, name: &str) -> Result<()> {
            let mut expected: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::fstatat(
                    dir.file.as_raw_fd(),
                    c_name(name)?.as_ptr(),
                    &mut expected,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
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
            if expected.st_dev != current.st_dev || expected.st_ino != current.st_ino {
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

    fn create_managed_state_dir(config_path: &Path) {
        platform::with_state_dir(config_path, true, |_| Ok(())).unwrap();
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
    fn cooperating_writers_serialize_generations() {
        let dir = tempfile::tempdir().unwrap();
        let checkout_path = dir.path().join("checkout");
        checkout(&checkout_path);
        let config_path = dir.path().join("config.toml");
        let mut writers = Vec::new();
        for _ in 0..8 {
            let checkout_path = checkout_path.clone();
            let config_path = config_path.clone();
            writers.push(std::thread::spawn(move || {
                persist(&config_path, &checkout_path).unwrap();
            }));
        }
        for writer in writers {
            writer.join().unwrap();
        }

        let records = platform::with_state_dir(&config_path, false, platform::records).unwrap();
        assert_eq!(records.len(), 8);
        assert_eq!(record_generation(&records[0]), Some(1));
        assert_eq!(record_generation(&records[7]), Some(8));
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
        create_managed_state_dir(&config_path);
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
    fn malformed_max_generation_record_can_be_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let checkout_path = dir.path().join("checkout");
        checkout(&checkout_path);
        let config_path = dir.path().join("config.toml");
        create_managed_state_dir(&config_path);
        fs::write(
            state_dir_for_config(&config_path).join(format!(
                "state-{:020}-00000000000000000000000000000001.json",
                u64::MAX
            )),
            "not json",
        )
        .unwrap();

        persist(&config_path, &checkout_path).unwrap();
        let records = platform::with_state_dir(&config_path, false, platform::records).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(record_generation(&records[0]), Some(1));
    }

    #[test]
    fn valid_max_generation_record_can_be_rebased_by_writer() {
        let dir = tempfile::tempdir().unwrap();
        let checkout_path = dir.path().join("checkout");
        checkout(&checkout_path);
        let config_path = dir.path().join("config.toml");
        persist(&config_path, &checkout_path).unwrap();
        let records = platform::with_state_dir(&config_path, false, platform::records).unwrap();
        let max_name = format!(
            "state-{:020}-00000000000000000000000000000002.json",
            u64::MAX
        );
        let state_dir = state_dir_for_config(&config_path);
        fs::rename(state_dir.join(&records[0]), state_dir.join(&max_name)).unwrap();
        fs::write(
            state_dir.join(&max_name),
            serde_json::to_vec_pretty(&SourceCheckoutState {
                generation: u64::MAX,
                repo_root: path_string(&checkout_path.canonicalize().unwrap()).unwrap(),
            })
            .unwrap(),
        )
        .unwrap();

        assert_eq!(
            load(&config_path).unwrap().as_deref(),
            checkout_path.to_str()
        );
        persist(&config_path, &checkout_path).unwrap();
        let records = platform::with_state_dir(&config_path, false, platform::records).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(record_generation(&records[0]), Some(1));
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

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn bound_state_directory_survives_parent_path_swap() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let state_path = state_dir_for_config(&config_path);
        create_managed_state_dir(&config_path);
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
        let checkout_path = dir.path().join("checkout");
        checkout(&checkout_path);
        create_managed_state_dir(&config_path);
        let record = state_dir_for_config(&config_path)
            .join("state-00000000000000000001-00000000000000000000000000000001.json");
        let outside = dir.path().join("outside");
        fs::write(&outside, "{}").unwrap();
        symlink(&outside, &record).unwrap();
        assert!(load(&config_path).is_err());
        persist(&config_path, &checkout_path).unwrap();
        assert_eq!(
            load(&config_path).unwrap().as_deref(),
            checkout_path.to_str()
        );

        let record = state_dir_for_config(&config_path)
            .join("state-00000000000000000003-00000000000000000000000000000003.json");
        let path = CString::new(record.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(load(&config_path).is_err());
        persist(&config_path, &checkout_path).unwrap();
        assert_eq!(
            load(&config_path).unwrap().as_deref(),
            checkout_path.to_str()
        );
    }

    #[test]
    fn malformed_latest_record_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        create_managed_state_dir(&config_path);
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

        let wrong_package = dir.path().join("wrong-package");
        checkout(&wrong_package);
        fs::write(
            wrong_package.join("Cargo.toml"),
            "[package]\nname = \"not-clawhip\"\n",
        )
        .unwrap();
        assert!(persist(&config_path, &wrong_package).is_err());

        let nested = wrong_package.join("nested");
        fs::create_dir_all(nested.join("src")).unwrap();
        fs::write(nested.join("Cargo.toml"), "[package]\nname = \"clawhip\"\n").unwrap();
        assert!(persist(&config_path, &nested).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn checkout_marker_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("checkout");
        fs::create_dir_all(root.join("src")).unwrap();
        assert!(
            Command::new("git")
                .arg("init")
                .arg("-q")
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        let manifest = dir.path().join("Cargo.toml");
        fs::write(&manifest, "[package]\nname = \"clawhip\"\n").unwrap();
        symlink(&manifest, root.join("Cargo.toml")).unwrap();

        assert!(validate_checkout(&root).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn state_directory_requires_private_current_user_ownership() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let state_dir = state_dir_for_config(&config_path);
        fs::create_dir(&state_dir).unwrap();
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(load(&config_path).is_err());
    }
}
