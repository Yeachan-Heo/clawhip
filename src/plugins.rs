use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::Result;

const PLUGIN_DIR_ENV: &str = "CLAWHIP_PLUGIN_DIR";

#[derive(Debug, Clone, Deserialize)]
struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_bridge")]
    pub bridge: String,
}

#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub description: Option<String>,
    pub bridge_path: PathBuf,
}

impl Plugin {
    fn from_manifest(dir: &Path, manifest: PluginManifest) -> Result<Self> {
        let bridge_path = dir.join(&manifest.bridge);
        if !bridge_path.is_file() {
            return Err(format!(
                "plugin '{}' is missing bridge script {}",
                manifest.name,
                bridge_path.display()
            )
            .into());
        }

        Ok(Self {
            name: manifest.name,
            description: manifest.description,
            bridge_path,
        })
    }
}

pub fn default_plugins_dir(config_path: &Path) -> Result<PathBuf> {
    Ok(resolve_plugins_dir(config_path).unwrap_or_else(|| app_plugins_dir(config_path)))
}

pub fn load_plugins(root: &Path) -> Result<Vec<Plugin>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut directories = fs::read_dir(root)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    directories.sort();

    let mut plugins = Vec::new();
    for dir in directories {
        let manifest_path = dir.join("plugin.toml");
        if !manifest_path.is_file() {
            continue;
        }

        let raw = fs::read_to_string(&manifest_path)?;
        let manifest: PluginManifest = toml::from_str(&raw)?;
        plugins.push(Plugin::from_manifest(&dir, manifest)?);
    }

    Ok(plugins)
}

pub fn install_bundled_plugins(destination_root: &Path) -> Result<()> {
    let source_root = bundled_plugins_dir();
    if !source_root.is_dir() {
        return Ok(());
    }

    copy_dir_all(&source_root, destination_root)
}

fn resolve_plugins_dir(config_path: &Path) -> Option<PathBuf> {
    plugin_dir_candidates(config_path)
        .into_iter()
        .find(|path| path.is_dir())
}

fn plugin_dir_candidates(config_path: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(dir) = env::var_os(PLUGIN_DIR_ENV)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        candidates.push(dir);
    }

    candidates.push(app_plugins_dir(config_path));
    candidates.push(bundled_plugins_dir());

    if let Ok(exe) = env::current_exe()
        && let Some(bin_dir) = exe.parent()
    {
        candidates.push(bin_dir.join("plugins"));
        if let Some(parent) = bin_dir.parent() {
            candidates.push(parent.join("plugins"));
        }
    }

    dedupe_paths(candidates)
}

fn bundled_plugins_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("plugins")
}

fn app_plugins_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("plugins")
}

fn copy_dir_all(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;

    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = entry.path();
        let target = destination.join(entry.file_name());

        if path.is_dir() {
            copy_dir_all(&path, &target)?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&path, &target)?;
        }
    }

    Ok(())
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique = Vec::new();
    for path in paths {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

fn default_bridge() -> String {
    "bridge.sh".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_plugins_follow_selected_config_parent() {
        assert_eq!(
            app_plugins_dir(Path::new("/srv/clawhip/custom.toml")),
            PathBuf::from("/srv/clawhip/plugins")
        );
        assert_eq!(
            app_plugins_dir(Path::new("custom.toml")),
            PathBuf::from("./plugins")
        );
    }

    #[test]
    fn loads_plugins_from_plugin_toml_files() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let plugins_dir = tempdir.path().join("plugins");
        let codex_dir = plugins_dir.join("codex");
        let claude_dir = plugins_dir.join("claude-code");

        fs::create_dir_all(&codex_dir).expect("create codex dir");
        fs::create_dir_all(&claude_dir).expect("create claude dir");

        fs::write(
            codex_dir.join("plugin.toml"),
            r#"
name = "codex"
description = "Codex bridge"
bridge = "bridge.sh"
"#,
        )
        .expect("write codex manifest");
        fs::write(
            codex_dir.join("bridge.sh"),
            "#!/usr/bin/env bash
",
        )
        .expect("write codex bridge");

        fs::write(
            claude_dir.join("plugin.toml"),
            r#"
name = "claude-code"
description = "Claude Code bridge"
"#,
        )
        .expect("write claude manifest");
        fs::write(
            claude_dir.join("bridge.sh"),
            "#!/usr/bin/env bash
",
        )
        .expect("write claude bridge");

        let plugins = load_plugins(&plugins_dir).expect("load plugins");

        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].name, "claude-code");
        assert_eq!(plugins[0].bridge_path, claude_dir.join("bridge.sh"));
        assert_eq!(plugins[1].name, "codex");
        assert_eq!(plugins[1].bridge_path, codex_dir.join("bridge.sh"));
    }

    #[test]
    fn rejects_plugin_without_bridge_script() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let plugins_dir = tempdir.path().join("plugins");
        let broken_dir = plugins_dir.join("broken");

        fs::create_dir_all(&broken_dir).expect("create broken dir");
        fs::write(
            broken_dir.join("plugin.toml"),
            r#"
name = "broken"
bridge = "bridge.sh"
"#,
        )
        .expect("write broken manifest");

        let error = load_plugins(&plugins_dir).expect_err("missing bridge should fail");
        let message = error.to_string();
        assert!(message.contains("missing bridge script"));
        assert!(message.contains("broken"));
    }

    #[test]
    fn installs_bundled_plugins_into_destination() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let destination = tempdir.path().join("installed-plugins");

        install_bundled_plugins(&destination).expect("install bundled plugins");

        assert!(destination.join("codex").join("plugin.toml").is_file());
        assert!(destination.join("codex").join("bridge.sh").is_file());
        assert!(
            destination
                .join("claude-code")
                .join("plugin.toml")
                .is_file()
        );
        assert!(destination.join("claude-code").join("bridge.sh").is_file());
    }
}
