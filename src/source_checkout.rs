use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

const SIDECAR_NAME: &str = "source-checkout.json";
const TEMP_PREFIX: &str = ".source-checkout.json.tmp-";
const MAX_STALE_TEMPS: usize = 32;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceCheckoutState {
    repo_root: String,
}

pub fn path_for_config(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(SIDECAR_NAME)
}

pub fn load(config_path: &Path) -> Result<Option<String>> {
    let path = path_for_config(config_path);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "managed source checkout state must be a regular non-symlink file: {}",
            path.display()
        )
        .into());
    }
    let raw = fs::read_to_string(&path)?;
    let state: SourceCheckoutState = serde_json::from_str(&raw)?;
    let canonical = validate_checkout(Path::new(&state.repo_root))?;
    Ok(Some(path_string(&canonical)?))
}

pub fn persist(config_path: &Path, repo_root: &Path) -> Result<()> {
    let canonical = validate_checkout(repo_root)?;
    persist_with_hook(config_path, &canonical, &mut || Ok(()))
}

fn persist_with_hook<F>(config_path: &Path, canonical: &Path, before_rename: &mut F) -> Result<()>
where
    F: FnMut() -> Result<()>,
{
    let path = path_for_config(config_path);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    validate_parent(parent)?;
    validate_existing_sidecar(&path)?;

    let state = SourceCheckoutState {
        repo_root: path_string(canonical)?,
    };
    let bytes = serde_json::to_vec_pretty(&state)?;
    let temp_path = parent.join(format!("{TEMP_PREFIX}{}", uuid::Uuid::new_v4().simple()));
    let mut temp = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)?;
    if let Err(error) = (|| -> Result<()> {
        temp.write_all(&bytes)?;
        temp.write_all(b"\n")?;
        temp.sync_all()?;
        before_rename()?;
        validate_parent(parent)?;
        validate_existing_sidecar(&path)?;
        fs::rename(&temp_path, &path)?;
        File::open(&path)?.sync_all()?;
        File::open(parent)?.sync_all()?;
        cleanup_stale_temps(parent)?;
        Ok(())
    })() {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    Ok(())
}

fn validate_parent(parent: &Path) -> Result<()> {
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(format!(
            "managed source checkout parent must be a regular directory: {}",
            parent.display()
        )
        .into()),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(parent)?;
            validate_parent(parent)
        }
        Err(error) => Err(error.into()),
    }
}

fn validate_existing_sidecar(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(format!(
            "managed source checkout state must be a regular non-symlink file: {}",
            path.display()
        )
        .into()),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn cleanup_stale_temps(parent: &Path) -> Result<()> {
    let mut seen = 0usize;
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(TEMP_PREFIX) {
            continue;
        }
        seen += 1;
        if seen > MAX_STALE_TEMPS {
            break;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_file() && !metadata.file_type().is_symlink() {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn validate_checkout(path: &Path) -> Result<PathBuf> {
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
    Ok(canonical)
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "managed repo_root must be valid UTF-8".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    fn checkout(root: &Path) {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"clawhip\"\n").unwrap();
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
    fn interrupted_write_preserves_last_valid_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        checkout(&first);
        checkout(&second);
        let config_path = dir.path().join("config.toml");
        persist(&config_path, &first).unwrap();

        let error = persist_with_hook(&config_path, &second, &mut || {
            Err("injected interruption".into())
        })
        .unwrap_err();

        assert!(error.to_string().contains("injected interruption"));
        assert_eq!(load(&config_path).unwrap().as_deref(), first.to_str());
    }

    #[test]
    fn malformed_and_nonregular_sidecars_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let sidecar = path_for_config(&config_path);
        fs::write(&sidecar, "not json").unwrap();
        assert!(load(&config_path).is_err());

        fs::remove_file(&sidecar).unwrap();
        fs::create_dir(&sidecar).unwrap();
        assert!(load(&config_path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_fifo_sidecars_are_rejected_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let sidecar = path_for_config(&config_path);
        let outside = dir.path().join("outside");
        fs::write(&outside, "{}").unwrap();
        symlink(&outside, &sidecar).unwrap();
        assert!(load(&config_path).is_err());

        fs::remove_file(&sidecar).unwrap();
        let path = CString::new(sidecar.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(load(&config_path).is_err());
    }

    #[test]
    fn invalid_checkout_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        assert!(persist(&config_path, dir.path()).is_err());
        assert!(!path_for_config(&config_path).exists());
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
}
