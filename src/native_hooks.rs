use std::fs;
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::Result;
use crate::cli::EnableHookArgs;

const CLAWHIP_DIR: &str = ".clawhip";
const CLAWHIP_CONFIG: &str = "config.json";
const CLAUDE_SETTINGS: &str = ".claude/settings.json";
const CODEX_CONFIG: &str = ".codex/config.toml";
const HOOK_SCRIPT: &str = ".clawhip/hooks/native-hook.mjs";
const CLAUDE_HOOK_CMD: &str = "node ./.clawhip/hooks/native-hook.mjs --provider claude-code";
const CODEX_HOOK_CMD: &str = "node ./.clawhip/hooks/native-hook.mjs --provider codex";

pub fn enable(args: EnableHookArgs) -> Result<()> {
    let root = args.root.unwrap_or(std::env::current_dir()?);
    fs::create_dir_all(root.join(".clawhip/hooks"))?;
    fs::create_dir_all(root.join(".claude"))?;
    fs::create_dir_all(root.join(".codex"))?;

    fs::write(root.join(HOOK_SCRIPT), native_hook_script())?;
    write_repo_config(&root)?;
    write_claude_settings(&root)?;
    write_codex_config(&root)?;

    println!("Enabled repo-local native hooks in {}", root.display());
    println!(
        "  {}",
        root.join(CLAWHIP_DIR).join(CLAWHIP_CONFIG).display()
    );
    println!("  {}", root.join(CLAUDE_SETTINGS).display());
    println!("  {}", root.join(CODEX_CONFIG).display());
    println!("  {}", root.join(HOOK_SCRIPT).display());
    Ok(())
}

pub fn incoming_event_from_native_hook_json(
    payload: &Value,
) -> Result<crate::events::IncomingEvent> {
    let provider = first_string(
        payload,
        &["/provider", "/source/provider", "/context/provider"],
    )
    .unwrap_or_else(|| "unknown".to_string());
    let source = first_string(
        payload,
        &["/source", "/source/name", "/context/source", "/agent_name"],
    )
    .unwrap_or_else(|| provider.clone());
    let event_name = first_string(
        payload,
        &[
            "/event_name",
            "/event",
            "/hook_event_name",
            "/hookEventName",
        ],
    )
    .ok_or_else(|| "missing native hook event name".to_string())?;

    let canonical = map_common_event(&event_name)
        .ok_or_else(|| format!("unsupported native hook event '{event_name}'"))?;

    let authoritative_project = first_string(
        payload,
        &[
            "/project",
            "/project_name",
            "/projectName",
            "/context/project",
            "/context/project_name",
            "/context/projectName",
            "/source/project",
        ],
    );
    let directory = first_string(
        payload,
        &[
            "/directory",
            "/cwd",
            "/context/directory",
            "/context/cwd",
            "/source/directory",
            "/repo_path",
            "/projectPath",
            "/context/projectPath",
        ],
    );
    let project = authoritative_project.or_else(|| {
        directory.as_deref().and_then(|dir| {
            Path::new(dir)
                .file_name()
                .and_then(|name| name.to_str())
                .map(ToString::to_string)
        })
    });
    let session_id = first_string(
        payload,
        &[
            "/session_id",
            "/sessionId",
            "/context/session_id",
            "/context/sessionId",
            "/session_name",
            "/context/session_name",
        ],
    );

    let event_payload = payload
        .get("event_payload")
        .cloned()
        .or_else(|| payload.get("payload").cloned())
        .unwrap_or_else(|| json!({}));

    let mut normalized = Map::new();
    normalized.insert("provider".into(), json!(provider.clone()));
    normalized.insert("source".into(), json!(source.clone()));
    normalized.insert("tool".into(), json!(provider.clone()));
    normalized.insert("agent_name".into(), json!(source));
    normalized.insert("event_name".into(), json!(event_name));
    normalized.insert(
        "normalized_event".into(),
        json!(canonical.trim_start_matches("session.")),
    );
    normalized.insert("event_payload".into(), event_payload);
    if let Some(project) = project.clone() {
        normalized.insert("project".into(), json!(project.clone()));
        normalized.insert("repo_name".into(), json!(project));
    }
    if let Some(directory) = directory.clone() {
        normalized.insert("directory".into(), json!(directory.clone()));
        normalized.insert("repo_path".into(), json!(directory.clone()));
        normalized.insert("worktree_path".into(), json!(directory));
    }
    if let Some(session_id) = session_id {
        normalized.insert("session_id".into(), json!(session_id));
    }
    normalized.insert("payload".into(), payload.clone());

    Ok(crate::events::IncomingEvent {
        kind: canonical.to_string(),
        channel: None,
        mention: None,
        format: None,
        template: None,
        payload: Value::Object(normalized),
    })
}

pub fn native_hooks_installed(workdir: &Path) -> bool {
    workdir.join(".clawhip/config.json").is_file()
        || workdir.join(".claude/settings.json").is_file()
        || workdir.join(".codex/config.toml").is_file()
}

fn write_repo_config(root: &Path) -> Result<()> {
    let path = root.join(CLAWHIP_DIR).join(CLAWHIP_CONFIG);
    if path.exists() {
        return Ok(());
    }
    let project = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    fs::write(
        path,
        serde_json::to_string_pretty(&json!({
            "native_hook": {
                "enabled": true,
                "project": project,
                "providers": ["claude-code", "codex"],
                "events": ["SessionStart", "SessionEnd"]
            }
        }))? + "\n",
    )?;
    Ok(())
}

fn write_claude_settings(root: &Path) -> Result<()> {
    let path = root.join(CLAUDE_SETTINGS);
    if path.exists() {
        return Ok(());
    }
    let content = json!({
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": CLAUDE_HOOK_CMD}]}],
            "SessionEnd": [{"hooks": [{"type": "command", "command": CLAUDE_HOOK_CMD}]}]
        }
    });
    fs::write(path, serde_json::to_string_pretty(&content)? + "\n")?;
    Ok(())
}

fn write_codex_config(root: &Path) -> Result<()> {
    let path = root.join(CODEX_CONFIG);
    if path.exists() {
        return Ok(());
    }
    let content = format!(
        "# Generated by `clawhip enable-hook`\n[native_hooks]\nenabled = true\nproject = \"{}\"\n\n[native_hooks.events]\nSessionStart = \"{}\"\nSessionEnd = \"{}\"\n",
        root.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown"),
        CODEX_HOOK_CMD,
        CODEX_HOOK_CMD,
    );
    fs::write(path, content)?;
    Ok(())
}

fn map_common_event(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "sessionstart" | "session-start" | "session.started" | "started" => Some("session.started"),
        "sessionend" | "session-end" | "session.finished" | "finished" | "stop" => {
            Some("session.finished")
        }
        _ => None,
    }
}

fn first_string(payload: &Value, pointers: &[&str]) -> Option<String> {
    pointers.iter().find_map(|pointer| {
        payload
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

fn native_hook_script() -> &'static str {
    r#"#!/usr/bin/env node
import { existsSync, readFileSync } from 'node:fs';
import { basename, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

function arg(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : '';
}

function readStdin() {
  return new Promise((resolveOut) => {
    const chunks = [];
    process.stdin.on('data', (chunk) => chunks.push(chunk));
    process.stdin.on('end', () => resolveOut(Buffer.concat(chunks).toString('utf8')));
    process.stdin.on('error', () => resolveOut(''));
  });
}

function readRepoConfig(cwd) {
  const path = resolve(cwd, '.clawhip', 'config.json');
  if (!existsSync(path)) return null;
  try { return JSON.parse(readFileSync(path, 'utf8')); } catch { return null; }
}

async function main() {
  const provider = arg('--provider') || process.env.CLAWHIP_PROVIDER || 'unknown';
  const cwd = process.cwd();
  const config = readRepoConfig(cwd);
  const nativeHook = config?.native_hook;
  if (!nativeHook?.enabled) {
    console.log(JSON.stringify({ continue: true, suppressOutput: true, skipped: true, reason: 'native_hook_disabled' }));
    return;
  }

  const raw = await readStdin();
  let input = {};
  try { input = raw.trim() ? JSON.parse(raw) : {}; } catch {}

  const eventName = input.hook_event_name || input.hookEventName || input.event || process.env.CLAWHIP_HOOK_EVENT || 'unknown';
  const project = nativeHook.project || input.projectName || input.project || basename(cwd);
  const directory = input.cwd || input.directory || cwd;
  const payload = {
    provider,
    source: provider,
    project,
    directory,
    event_name: eventName,
    event_payload: input,
    session_id: input.session_id || input.sessionId || undefined,
  };

  spawnSync('clawhip', ['native', 'hook', '--provider', provider], {
    input: JSON.stringify(payload),
    encoding: 'utf8',
    stdio: ['pipe', 'ignore', 'ignore'],
  });

  console.log(JSON.stringify({ continue: true, suppressOutput: true }));
}

main().catch(() => {
  console.log(JSON.stringify({ continue: true, suppressOutput: true }));
});
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn native_hook_maps_session_start() {
        let event = incoming_event_from_native_hook_json(&json!({
            "provider": "claude-code",
            "project": "clawhip",
            "directory": "/repo/clawhip",
            "event_name": "SessionStart",
            "event_payload": {"session_id": "sess-1"}
        }))
        .expect("event");
        assert_eq!(event.kind, "session.started");
        assert_eq!(event.payload["project"], json!("clawhip"));
        assert_eq!(event.payload["repo_path"], json!("/repo/clawhip"));
        assert_eq!(event.payload["tool"], json!("claude-code"));
    }

    #[test]
    fn prefers_authoritative_project_metadata() {
        let event = incoming_event_from_native_hook_json(&json!({
            "provider": "codex",
            "project": "authoritative-project",
            "directory": "/repo/fallback-name",
            "event_name": "SessionEnd",
            "event_payload": {}
        }))
        .expect("event");
        assert_eq!(event.payload["project"], json!("authoritative-project"));
        assert_eq!(event.payload["repo_name"], json!("authoritative-project"));
    }

    #[test]
    fn enable_hook_writes_repo_local_files() {
        let dir = tempdir().expect("tempdir");
        enable(crate::cli::EnableHookArgs {
            root: Some(dir.path().to_path_buf()),
        })
        .expect("enable");
        assert!(dir.path().join(".clawhip/config.json").is_file());
        assert!(dir.path().join(".clawhip/hooks/native-hook.mjs").is_file());
        assert!(dir.path().join(".claude/settings.json").is_file());
        assert!(dir.path().join(".codex/config.toml").is_file());
    }
}
