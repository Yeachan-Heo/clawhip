#[allow(dead_code)]
pub mod prompt_deliver;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use serde_json::{Map, Value, json};

use crate::Result;
use crate::cli::{HookInstallScope, HookProvider, HooksInstallArgs};
use crate::native_hooks::{
    CLAUDE_SETTINGS_FILE, CODEX_HOOKS_FILE, HOOK_SCRIPT, SHARED_HOOK_EVENTS, generated_hook_script,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub generated_files: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

pub fn install(args: HooksInstallArgs) -> Result<()> {
    let report = run_install(&args)?;

    if args.scope == HookInstallScope::Project {
        println!("Project scope is deprecated; installing to the canonical global hook location.");
    }
    println!("Installed provider-native hook forwarding:");
    for path in &report.generated_files {
        println!("  {}", path.display());
    }
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    println!("Supported shared events: {}", SHARED_HOOK_EVENTS.join(", "));
    println!("Ingress: clawhip native hook --provider <codex|claude-code>");

    Ok(())
}

fn run_install(args: &HooksInstallArgs) -> Result<InstallReport> {
    let root = resolve_install_root(args)?;
    let providers = selected_providers(args);
    let mut generated_files = Vec::new();
    let warnings = Vec::new();

    let hook_script_path = root.join(HOOK_SCRIPT);
    write_generated_file(&hook_script_path, generated_hook_script(), args.force)?;
    generated_files.push(hook_script_path.clone());

    for provider in providers {
        let path = match provider {
            HookProvider::Codex => write_codex_hooks(&root, &hook_script_path)?,
            HookProvider::ClaudeCode => write_claude_settings(&root, &hook_script_path)?,
        };
        generated_files.push(path);
    }

    Ok(InstallReport {
        generated_files,
        warnings,
    })
}

fn resolve_install_root(args: &HooksInstallArgs) -> Result<PathBuf> {
    if args.scope == HookInstallScope::Project {
        let _ = args.root.as_ref();
    }
    home_dir()
}

fn selected_providers(args: &HooksInstallArgs) -> Vec<HookProvider> {
    if args.all || args.provider.is_empty() {
        vec![HookProvider::Codex, HookProvider::ClaudeCode]
    } else {
        args.provider.clone()
    }
}

fn write_codex_hooks(root: &Path, hook_script_path: &Path) -> Result<PathBuf> {
    let path = root.join(CODEX_HOOKS_FILE);
    let mut document = read_json_object(&path)?;
    let hooks = ensure_child_object(&mut document, "hooks")?;
    let command = hook_command(hook_script_path, HookProvider::Codex);

    for event in SHARED_HOOK_EVENTS {
        upsert_hook_event(hooks, event, &command, codex_matcher_for(event));
    }

    write_json(&path, Value::Object(document))?;
    Ok(path)
}

fn write_claude_settings(root: &Path, hook_script_path: &Path) -> Result<PathBuf> {
    let path = root.join(CLAUDE_SETTINGS_FILE);
    let mut document = read_json_object(&path)?;
    let hooks = ensure_child_object(&mut document, "hooks")?;
    let command = hook_command(hook_script_path, HookProvider::ClaudeCode);

    for event in SHARED_HOOK_EVENTS {
        upsert_hook_event(hooks, event, &command, claude_matcher_for(event));
    }

    write_json(&path, Value::Object(document))?;
    Ok(path)
}

fn codex_matcher_for(event: &str) -> Option<&'static str> {
    match event {
        "PreToolUse" | "PostToolUse" => Some(".*"),
        _ => None,
    }
}

fn claude_matcher_for(event: &str) -> Option<&'static str> {
    match event {
        "PreToolUse" | "PostToolUse" => Some(".*"),
        _ => None,
    }
}

fn hook_command(hook_script_path: &Path, provider: HookProvider) -> String {
    format!(
        "node {} --provider {}",
        shell_escape(&hook_script_path.display().to_string()),
        provider.as_str()
    )
}

fn upsert_hook_event(
    hooks: &mut Map<String, Value>,
    event: &str,
    command: &str,
    matcher: Option<&str>,
) {
    let entry = hooks
        .entry(event.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let groups = entry.as_array_mut().expect("hook event groups array");

    if let Some(existing_group) = groups
        .iter_mut()
        .find(|group| matcher_matches(group, matcher))
    {
        let hooks = ensure_group_hooks(existing_group);
        if !hooks.iter().any(|hook| hook_command_matches(hook, command)) {
            hooks.push(json!({
                "type": "command",
                "command": command,
            }));
        }
        return;
    }

    let mut group = Map::new();
    if let Some(matcher) = matcher {
        group.insert("matcher".into(), json!(matcher));
    }
    group.insert(
        "hooks".into(),
        json!([
            {
                "type": "command",
                "command": command,
            }
        ]),
    );
    groups.push(Value::Object(group));
}

fn matcher_matches(group: &Value, matcher: Option<&str>) -> bool {
    match (group.get("matcher").and_then(Value::as_str), matcher) {
        (None, None) => true,
        (Some(existing), Some(expected)) => existing == expected,
        _ => false,
    }
}

fn ensure_group_hooks(group: &mut Value) -> &mut Vec<Value> {
    let object = group.as_object_mut().expect("hook event group object");
    object
        .entry("hooks")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .expect("hooks array")
}

fn hook_command_matches(hook: &Value, command: &str) -> bool {
    hook.get("type").and_then(Value::as_str) == Some("command")
        && hook.get("command").and_then(Value::as_str) == Some(command)
}

fn write_generated_file(path: &Path, content: &str, force: bool) -> Result<()> {
    if path.exists() && !force {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    #[cfg(unix)]
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|ext| ext == "mjs")
    {
        set_executable(path)?;
    }
    Ok(())
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>> {
    if !path.exists() {
        return Ok(Map::new());
    }

    let content = fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&content)?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("{} must contain a JSON object", path.display()).into())
}

fn ensure_child_object<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>> {
    let entry = object
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    entry
        .as_object_mut()
        .ok_or_else(|| anyhow!("{key} must be a JSON object").into())
}

fn write_json(path: &Path, value: Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(&value)? + "\n")?;
    Ok(())
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow!("HOME environment variable not set").into())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    #[test]
    #[serial]
    fn install_project_scope_writes_global_provider_files_without_project_state() {
        let dir = tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        fs::create_dir_all(&repo).expect("create repo");
        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &home);
        }
        let report = run_install(&HooksInstallArgs {
            all: true,
            provider: Vec::new(),
            scope: HookInstallScope::Project,
            root: Some(repo.clone()),
            force: false,
        })
        .expect("install");

        assert!(report.generated_files.contains(&home.join(HOOK_SCRIPT)));
        assert!(
            report
                .generated_files
                .contains(&home.join(CODEX_HOOKS_FILE))
        );
        assert!(
            report
                .generated_files
                .contains(&home.join(CLAUDE_SETTINGS_FILE))
        );
        assert!(!dir.path().join(".clawhip/project.json").exists());

        if let Some(previous) = previous_home {
            unsafe {
                std::env::set_var("HOME", previous);
            }
        } else {
            unsafe {
                std::env::remove_var("HOME");
            }
        }
    }

    #[test]
    fn codex_install_writes_shared_events() {
        let dir = tempdir().expect("tempdir");
        let path =
            write_codex_hooks(dir.path(), &dir.path().join(HOOK_SCRIPT)).expect("codex hooks");
        let document: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        for event in SHARED_HOOK_EVENTS {
            assert!(document["hooks"][event].is_array(), "missing {event}");
        }
    }

    #[test]
    fn claude_install_writes_shared_events() {
        let dir = tempdir().expect("tempdir");
        let path = write_claude_settings(dir.path(), &dir.path().join(HOOK_SCRIPT))
            .expect("claude settings");
        let document: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        for event in SHARED_HOOK_EVENTS {
            assert!(document["hooks"][event].is_array(), "missing {event}");
        }
    }

    #[test]
    fn write_generated_file_preserves_existing_content_without_force() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("generated.mjs");
        fs::write(&path, "old\n").expect("seed file");

        write_generated_file(&path, "new\n", false).expect("write");

        assert_eq!(fs::read_to_string(&path).expect("read"), "old\n");
    }

    #[test]
    fn write_generated_file_overwrites_existing_content_with_force() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("generated.mjs");
        fs::write(&path, "old\n").expect("seed file");

        write_generated_file(&path, "new\n", true).expect("write");

        assert_eq!(fs::read_to_string(&path).expect("read"), "new\n");
    }
}
