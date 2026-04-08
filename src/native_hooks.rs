use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Map, Value, json};

use crate::Result;

#[allow(dead_code)]
pub const CLAWHIP_DIR: &str = ".clawhip";
pub const CLAWHIP_PROJECT_FILE: &str = ".clawhip/project.json";
pub const HOOK_SCRIPT: &str = ".clawhip/hooks/native-hook.mjs";
#[allow(dead_code)]
pub const PROJECT_METADATA_RELATIVE_PATH: &str = CLAWHIP_PROJECT_FILE;
#[allow(dead_code)]
pub const NATIVE_HOOK_SCRIPT_RELATIVE_PATH: &str = HOOK_SCRIPT;
pub const CODEX_HOOKS_FILE: &str = ".codex/hooks.json";
#[allow(dead_code)]
pub const CODEX_CONFIG_FILE: &str = ".codex/config.toml";
pub const CLAUDE_SETTINGS_FILE: &str = ".claude/settings.json";
pub const SHARED_HOOK_EVENTS: [&str; 5] = [
    "SessionStart",
    "PreToolUse",
    "PostToolUse",
    "UserPromptSubmit",
    "Stop",
];

pub fn incoming_event_from_native_hook_json(
    payload: &Value,
) -> Result<crate::events::IncomingEvent> {
    let provider = first_string(
        payload,
        &["/provider", "/source/provider", "/context/provider"],
    )
    .unwrap_or_else(|| "unknown".to_string());
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
    let canonical_kind = map_shared_event(&event_name)
        .ok_or_else(|| format!("unsupported native hook event '{event_name}'"))?;

    let directory = first_string(
        payload,
        &[
            "/directory",
            "/cwd",
            "/context/directory",
            "/context/cwd",
            "/source/directory",
            "/projectPath",
            "/context/projectPath",
            "/repo_path",
            "/worktree_path",
        ],
    );
    let worktree_path = first_string(payload, &["/worktree_path", "/context/worktree_path"])
        .or_else(|| directory.clone());
    let repo_path = first_string(payload, &["/repo_path", "/context/repo_path"]).or_else(|| {
        worktree_path
            .as_deref()
            .and_then(infer_repo_root)
            .map(|path| path.to_string_lossy().into_owned())
    });
    let project_metadata = load_effective_project_metadata(
        payload,
        repo_path.as_deref(),
        worktree_path.as_deref().or(directory.as_deref()),
    );

    let repo_name = first_string(
        payload,
        &[
            "/repo_name",
            "/context/repo_name",
            "/project",
            "/project_name",
            "/projectName",
        ],
    )
    .or_else(|| project_metadata_string(&project_metadata, &["repo_name", "repo", "name"]))
    .or_else(|| {
        repo_path
            .as_deref()
            .or(worktree_path.as_deref())
            .and_then(path_basename)
    });
    let project_name = first_string(
        payload,
        &[
            "/project",
            "/project_name",
            "/projectName",
            "/context/project",
            "/context/project_name",
            "/context/projectName",
        ],
    )
    .or_else(|| project_metadata_string(&project_metadata, &["name", "project_name"]));
    let project_id = first_string(
        payload,
        &[
            "/project_id",
            "/projectId",
            "/context/project_id",
            "/context/projectId",
        ],
    )
    .or_else(|| project_metadata_string(&project_metadata, &["id", "project_id"]));

    let source = first_string(
        payload,
        &["/source", "/source/name", "/context/source", "/agent_name"],
    )
    .unwrap_or_else(|| provider.clone());
    let session_id = first_string(
        payload,
        &[
            "/session_id",
            "/sessionId",
            "/context/session_id",
            "/context/sessionId",
            "/event_payload/session_id",
            "/event_payload/sessionId",
        ],
    );
    let turn_id = first_string(payload, &["/turn_id", "/turnId", "/context/turn_id"]);
    let transcript_path = first_string(
        payload,
        &[
            "/transcript_path",
            "/transcriptPath",
            "/context/transcript_path",
            "/context/transcriptPath",
        ],
    );
    let model = first_string(payload, &["/model", "/context/model"]);
    let tool_name = first_string(
        payload,
        &[
            "/tool_name",
            "/toolName",
            "/context/tool_name",
            "/event_payload/tool_name",
            "/event_payload/toolName",
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
    normalized.insert("agent_name".into(), json!(provider.clone()));
    normalized.insert("event_name".into(), json!(event_name.clone()));
    normalized.insert("hook_event_name".into(), json!(event_name));
    normalized.insert(
        "normalized_event".into(),
        json!(normalized_event_label(canonical_kind)),
    );
    normalized.insert("event_payload".into(), event_payload);
    normalized.insert("payload".into(), payload.clone());

    if let Some(directory) = directory {
        normalized.insert("directory".into(), json!(directory));
    }
    if let Some(worktree_path) = worktree_path {
        normalized.insert("worktree_path".into(), json!(worktree_path));
    }
    if let Some(repo_path) = repo_path {
        normalized.insert("repo_path".into(), json!(repo_path));
    }
    if let Some(repo_name) = repo_name {
        normalized.insert("repo_name".into(), json!(repo_name));
    }
    if let Some(project_name) = project_name {
        normalized.insert("project".into(), json!(project_name.clone()));
        normalized.insert("project_name".into(), json!(project_name));
    }
    if let Some(project_id) = project_id {
        normalized.insert("project_id".into(), json!(project_id));
    }
    if let Some(project_metadata) = project_metadata {
        normalized.insert("project_metadata".into(), project_metadata);
    }
    if let Some(session_id) = session_id {
        normalized.insert("session_id".into(), json!(session_id));
    }
    if let Some(turn_id) = turn_id {
        normalized.insert("turn_id".into(), json!(turn_id));
    }
    if let Some(transcript_path) = transcript_path {
        normalized.insert("transcript_path".into(), json!(transcript_path));
    }
    if let Some(model) = model {
        normalized.insert("model".into(), json!(model));
    }
    if let Some(tool_name) = tool_name {
        normalized.insert("tool_name".into(), json!(tool_name));
    }

    apply_augmentation(
        &mut normalized,
        payload
            .get("augmentation")
            .or_else(|| payload.pointer("/event_payload/augmentation")),
    );

    Ok(crate::events::IncomingEvent {
        kind: canonical_kind.to_string(),
        channel: None,
        mention: None,
        format: None,
        template: None,
        payload: Value::Object(normalized),
    })
}

#[allow(dead_code)]
pub fn native_hooks_installed(workdir: &Path) -> bool {
    workdir.join(HOOK_SCRIPT).is_file()
        || workdir.join(CLAUDE_SETTINGS_FILE).is_file()
        || workdir.join(CODEX_HOOKS_FILE).is_file()
        || workdir.join(CODEX_CONFIG_FILE).is_file()
}

pub fn generated_hook_script() -> &'static str {
    r#"#!/usr/bin/env node
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

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

function parseJson(text, fallback = {}) {
  try {
    return text && text.trim() ? JSON.parse(text) : fallback;
  } catch {
    return fallback;
  }
}

function runGit(args, cwd) {
  const result = spawnSync('git', args, { cwd, encoding: 'utf8' });
  if (result.status === 0) {
    return result.stdout.trim();
  }
  return '';
}

function loadProjectMetadata(root) {
  const path = join(root, '.clawhip', 'project.json');
  if (!existsSync(path)) return null;
  return parseJson(readFileSync(path, 'utf8'), null);
}

function mergeAdditive(base, extra) {
  if (!extra || typeof extra !== 'object' || Array.isArray(extra)) return base;
  const output = { ...base };
  for (const [key, value] of Object.entries(extra)) {
    if (!(key in output)) {
      output[key] = value;
      continue;
    }
    if (Array.isArray(output[key]) && Array.isArray(value)) {
      output[key] = [...output[key], ...value];
      continue;
    }
    if (
      output[key] &&
      value &&
      typeof output[key] === 'object' &&
      typeof value === 'object' &&
      !Array.isArray(output[key]) &&
      !Array.isArray(value)
    ) {
      output[key] = mergeAdditive(output[key], value);
    }
  }
  return output;
}

async function collectAugmentation(root, payload) {
  const augmentDir = join(root, '.clawhip/hooks/augment');
  if (!existsSync(augmentDir)) return null;

  let merged = {};
  for (const entry of readdirSync(augmentDir)) {
    if (!entry.endsWith('.mjs') && !entry.endsWith('.js') && !entry.endsWith('.cjs')) continue;
    const modulePath = join(augmentDir, entry);
    const module = await import(pathToFileURL(modulePath).href);
    const fn = module.default || module.augment;
    if (typeof fn !== 'function') continue;
    const result = await fn(payload);
    if (result && typeof result === 'object') {
      merged = mergeAdditive(merged, result);
    }
  }

  return Object.keys(merged).length > 0 ? merged : null;
}

async function main() {
  const provider = arg('--provider') || process.env.CLAWHIP_PROVIDER || 'unknown';
  const cwd = process.cwd();
  const raw = await readStdin();
  const input = parseJson(raw, {});
  const repoRoot = runGit(['rev-parse', '--show-toplevel'], cwd) || cwd;
  const projectMetadata = loadProjectMetadata(repoRoot);
  const eventName =
    input.hook_event_name || input.hookEventName || input.event_name || input.event || 'unknown';
  const payload = {
    provider,
    source: provider,
    directory: input.cwd || cwd,
    repo_path: repoRoot,
    worktree_path: input.cwd || cwd,
    repo_name: basename(repoRoot),
    event_name: eventName,
    hook_event_name: eventName,
    session_id: input.session_id || input.sessionId,
    turn_id: input.turn_id || input.turnId,
    transcript_path: input.transcript_path || input.transcriptPath,
    model: input.model,
    tool_name: input.tool_name || input.toolName,
    tool_input: input.tool_input,
    tool_response: input.tool_response,
    prompt: input.prompt,
    event_payload: input,
  };

  if (projectMetadata && typeof projectMetadata === 'object') {
    payload.project_metadata = projectMetadata;
    if (projectMetadata.name) {
      payload.project = projectMetadata.name;
      payload.project_name = projectMetadata.name;
    }
    if (projectMetadata.id) {
      payload.project_id = projectMetadata.id;
    }
    if (projectMetadata.repo_name) {
      payload.repo_name = projectMetadata.repo_name;
    }
  }

  const augmentation = await collectAugmentation(repoRoot, payload);
  if (augmentation) {
    payload.augmentation = augmentation;
  }

  spawnSync('clawhip', ['native', 'hook', '--provider', provider], {
    input: JSON.stringify(payload),
    encoding: 'utf8',
    stdio: ['pipe', 'ignore', 'ignore'],
  });
}

main().catch(() => {
  process.exit(0);
});
"#
}

#[allow(dead_code)]
pub fn native_hook_script() -> &'static str {
    generated_hook_script()
}

fn map_shared_event(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "sessionstart" | "session-start" | "session.started" | "started" => Some("session.started"),
        "pretooluse" | "pre-tool-use" => Some("tool.pre"),
        "posttooluse" | "post-tool-use" => Some("tool.post"),
        "userpromptsubmit"
        | "user-prompt-submit"
        | "prompt-submitted"
        | "session.prompt-submitted" => Some("session.prompt-submitted"),
        "stop" | "sessionstop" | "session-stopped" => Some("session.stopped"),
        _ => None,
    }
}

fn normalized_event_label(kind: &str) -> &str {
    match kind {
        "session.started" => "started",
        "tool.pre" => "pre-tool-use",
        "tool.post" => "post-tool-use",
        "session.prompt-submitted" => "prompt-submitted",
        "session.stopped" => "stop",
        _ => kind,
    }
}

fn load_effective_project_metadata(
    payload: &Value,
    repo_path: Option<&str>,
    worktree_path: Option<&str>,
) -> Option<Value> {
    payload
        .get("project_metadata")
        .cloned()
        .or_else(|| repo_path.and_then(load_project_metadata_file))
        .or_else(|| worktree_path.and_then(load_project_metadata_file))
}

fn load_project_metadata_file(root: &str) -> Option<Value> {
    let path = Path::new(root).join(CLAWHIP_PROJECT_FILE);
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&raw).ok()
}

fn project_metadata_string(project_metadata: &Option<Value>, keys: &[&str]) -> Option<String> {
    let metadata = project_metadata.as_ref()?.as_object()?;
    keys.iter().find_map(|key| {
        metadata
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

fn infer_repo_root(directory: &str) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["-C", directory, "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let trimmed = root.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn path_basename(path: &str) -> Option<String> {
    Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn apply_augmentation(payload: &mut Map<String, Value>, augmentation: Option<&Value>) {
    let Some(augmentation) = augmentation.and_then(Value::as_object) else {
        return;
    };

    if let Some(summary) = augmentation.get("summary").and_then(Value::as_str)
        && !summary.trim().is_empty()
        && !payload.contains_key("summary")
    {
        payload.insert("summary".into(), json!(summary.trim()));
    }

    if let Some(additional_context) = augmentation.get("additional_context") {
        merge_object_like(payload, "additional_context", additional_context.clone());
    }
    if let Some(recent_context) = augmentation.get("recent_context") {
        merge_array_like(payload, "recent_context", recent_context.clone());
    }
    if let Some(frontmatter) = augmentation.get("frontmatter") {
        merge_object_like(payload, "frontmatter", frontmatter.clone());
    }
    if let Some(message) = augmentation.get("message") {
        merge_object_like(payload, "message", message.clone());
    }
    if let Some(context) = augmentation.get("context") {
        merge_object_like(payload, "message_context", context.clone());
    }

    payload.insert("augmentation".into(), Value::Object(augmentation.clone()));
}

fn merge_object_like(payload: &mut Map<String, Value>, key: &str, incoming: Value) {
    match (payload.get_mut(key), incoming) {
        (Some(Value::Object(existing)), Value::Object(incoming)) => {
            for (incoming_key, incoming_value) in incoming {
                existing.entry(incoming_key).or_insert(incoming_value);
            }
        }
        (None, Value::Object(incoming)) => {
            payload.insert(key.into(), Value::Object(incoming));
        }
        (None, value) => {
            payload.insert(key.into(), value);
        }
        _ => {}
    }
}

fn merge_array_like(payload: &mut Map<String, Value>, key: &str, incoming: Value) {
    match (payload.get_mut(key), incoming) {
        (Some(Value::Array(existing)), Value::Array(mut incoming)) => {
            existing.append(&mut incoming)
        }
        (None, Value::Array(incoming)) => {
            payload.insert(key.into(), Value::Array(incoming));
        }
        _ => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn maps_all_shared_hook_events() {
        let cases = [
            ("SessionStart", "codex", "session.started"),
            ("PreToolUse", "codex", "tool.pre"),
            ("PostToolUse", "claude-code", "tool.post"),
            (
                "UserPromptSubmit",
                "claude-code",
                "session.prompt-submitted",
            ),
            ("Stop", "codex", "session.stopped"),
        ];

        for (event_name, provider, expected_kind) in cases {
            let event = incoming_event_from_native_hook_json(&json!({
                "provider": provider,
                "directory": "/repo/clawhip",
                "event_name": event_name,
                "event_payload": {
                    "tool_name": "Bash",
                    "tool_input": {"command": "echo hi"}
                }
            }))
            .expect("event");
            assert_eq!(
                event.kind, expected_kind,
                "unexpected kind for {event_name}"
            );
            assert_eq!(event.payload["provider"], json!(provider));
            assert_eq!(event.payload["repo_name"], json!("clawhip"));
        }
    }

    #[test]
    fn loads_project_metadata_from_project_json() {
        let dir = tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join(".clawhip")).unwrap();
        fs::write(
            dir.path().join(CLAWHIP_PROJECT_FILE),
            serde_json::to_string_pretty(&json!({
                "id": "clawhip-core",
                "name": "clawhip",
                "repo_name": "clawhip"
            }))
            .unwrap(),
        )
        .unwrap();

        let event = incoming_event_from_native_hook_json(&json!({
            "provider": "codex",
            "directory": dir.path(),
            "event_name": "SessionStart",
            "event_payload": {}
        }))
        .expect("event");

        assert_eq!(event.payload["project_id"], json!("clawhip-core"));
        assert_eq!(event.payload["project_name"], json!("clawhip"));
        assert_eq!(
            event.payload["project_metadata"]["repo_name"],
            json!("clawhip")
        );
    }

    #[test]
    fn augmentation_can_add_context_without_overriding_base_fields() {
        let event = incoming_event_from_native_hook_json(&json!({
            "provider": "claude-code",
            "directory": "/repo/clawhip",
            "event_name": "SessionStart",
            "augmentation": {
                "summary": "extra setup context",
                "context": {
                    "repo_name": "should-not-replace-base",
                    "recent_issue": 163
                },
                "frontmatter": {
                    "owner": "worker-2"
                }
            },
            "event_payload": {}
        }))
        .expect("event");

        assert_eq!(event.payload["repo_name"], json!("clawhip"));
        assert_eq!(event.payload["summary"], json!("extra setup context"));
        assert_eq!(
            event.payload["message_context"]["repo_name"],
            json!("should-not-replace-base")
        );
        assert_eq!(event.payload["frontmatter"]["owner"], json!("worker-2"));
    }

    #[test]
    fn generated_hook_script_mentions_augment_pipeline() {
        let script = generated_hook_script();
        assert!(script.contains(".clawhip/hooks/augment"));
        assert!(script.contains("clawhip', ['native', 'hook'"));
    }
}
