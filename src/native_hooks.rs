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
const NORMALIZATION_OUTCOME_FIELD: &str = "normalization_outcome";

pub const NATIVE_NORMALIZATION_OUTCOME_FIELD: &str = "normalization_outcome";
pub const NATIVE_NON_GIT_OUTCOME: &str = "non_git";
pub const NATIVE_GIT_OUTCOME: &str = "git";

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
    let directory = directory.and_then(|value| canonicalize_path(&value).or(Some(value)));
    let worktree_path = first_string(payload, &["/worktree_path", "/context/worktree_path"])
        .or_else(|| directory.clone())
        .and_then(|value| canonicalize_path(&value).or(Some(value)));
    let repo_path = first_string(payload, &["/repo_path", "/context/repo_path"])
        .and_then(|value| canonicalize_path(&value).or(Some(value)))
        .or_else(|| {
            worktree_path
                .as_deref()
                .and_then(infer_repo_root)
                .map(|path| path.to_string_lossy().into_owned())
        });
    let normalization_outcome = if repo_path.is_some() {
        "git"
    } else {
        "non_git"
    };

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
    .filter(|_| normalization_outcome == "git");
    let project_id = first_string(
        payload,
        &[
            "/project_id",
            "/projectId",
            "/context/project_id",
            "/context/projectId",
        ],
    )
    .filter(|_| normalization_outcome == "git");

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
    normalized.insert(
        NORMALIZATION_OUTCOME_FIELD.into(),
        json!(normalization_outcome),
    );
    normalized.insert("event_payload".into(), event_payload);
    normalized.insert("payload".into(), payload.clone());
    normalized.insert(
        "routeable".into(),
        json!(normalization_outcome == NATIVE_GIT_OUTCOME),
    );

    if let Some(directory) = directory {
        normalized.insert("directory".into(), json!(directory));
    }
    if let Some(worktree_path) = worktree_path.filter(|_| normalization_outcome == "git") {
        normalized.insert("worktree_path".into(), json!(worktree_path));
    }
    if let Some(repo_path) = repo_path {
        normalized.insert("repo_path".into(), json!(repo_path));
    }
    if let Some(repo_name) = repo_name.filter(|_| normalization_outcome == "git") {
        normalized.insert("repo_name".into(), json!(repo_name));
    }
    if let Some(project_name) = project_name {
        normalized.insert("project".into(), json!(project_name.clone()));
        normalized.insert("project_name".into(), json!(project_name));
    }
    if let Some(project_id) = project_id {
        normalized.insert("project_id".into(), json!(project_id));
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
    copy_string_field(
        &mut normalized,
        payload,
        "tmux_session",
        &[
            "/tmux_session",
            "/tmuxSession",
            "/context/tmux_session",
            "/context/tmuxSession",
            "/event_payload/tmux_session",
            "/event_payload/tmuxSession",
            "/event_payload/tmux/session",
        ],
    );
    copy_string_field(
        &mut normalized,
        payload,
        "tmux_window",
        &[
            "/tmux_window",
            "/tmuxWindow",
            "/context/tmux_window",
            "/context/tmuxWindow",
            "/event_payload/tmux_window",
            "/event_payload/tmuxWindow",
            "/event_payload/tmux/window",
        ],
    );
    copy_string_field(
        &mut normalized,
        payload,
        "tmux_pane",
        &[
            "/tmux_pane",
            "/tmuxPane",
            "/context/tmux_pane",
            "/context/tmuxPane",
            "/event_payload/tmux_pane",
            "/event_payload/tmuxPane",
            "/event_payload/tmux/pane",
        ],
    );
    copy_string_field(
        &mut normalized,
        payload,
        "tmux_pane_tty",
        &[
            "/tmux_pane_tty",
            "/tmuxPaneTty",
            "/context/tmux_pane_tty",
            "/context/tmuxPaneTty",
            "/event_payload/tmux_pane_tty",
            "/event_payload/tmuxPaneTty",
            "/event_payload/tmux/pane_tty",
            "/event_payload/tmux/paneTty",
        ],
    );
    copy_bool_field(
        &mut normalized,
        payload,
        "tmux_attached",
        &[
            "/tmux_attached",
            "/tmuxAttached",
            "/context/tmux_attached",
            "/context/tmuxAttached",
            "/event_payload/tmux_attached",
            "/event_payload/tmuxAttached",
            "/event_payload/tmux/attached",
        ],
    );
    copy_u64_field(
        &mut normalized,
        payload,
        "tmux_client_count",
        &[
            "/tmux_client_count",
            "/tmuxClientCount",
            "/context/tmux_client_count",
            "/context/tmuxClientCount",
            "/event_payload/tmux_client_count",
            "/event_payload/tmuxClientCount",
            "/event_payload/tmux/client_count",
            "/event_payload/tmux/clientCount",
        ],
    );

    apply_augmentation(
        &mut normalized,
        payload
            .get("augmentation")
            .or_else(|| payload.pointer("/event_payload/augmentation")),
    );

    apply_stop_context(
        &mut normalized,
        payload
            .get("stop_context")
            .or_else(|| payload.pointer("/event_payload/stop_context")),
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
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
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

function inferRepoRoot(cwd) {
  const commonDir = runGit(['rev-parse', '--path-format=absolute', '--git-common-dir'], cwd);
  if (commonDir) {
    return dirname(commonDir);
  }
  return runGit(['rev-parse', '--show-toplevel'], cwd);
}

function inferWorktreeRoot(cwd) {
  return runGit(['rev-parse', '--show-toplevel'], cwd);
}

function parseIntegerish(value) {
  if (typeof value === 'number' && Number.isFinite(value)) {
    return Math.trunc(value);
  }
  if (typeof value !== 'string') return null;
  const trimmed = value.trim();
  if (!/^-?\d+$/.test(trimmed)) return null;
  return Number.parseInt(trimmed, 10);
}

function parseBoolish(value) {
  if (typeof value === 'boolean') return value;
  if (typeof value === 'number' && Number.isFinite(value)) return value !== 0;
  if (typeof value !== 'string') return null;
  const normalized = value.trim().toLowerCase();
  if (!normalized) return null;
  if (['1', 'true', 'yes', 'attached'].includes(normalized)) return true;
  if (['0', 'false', 'no', 'detached'].includes(normalized)) return false;
  return null;
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

function collectTmuxMetadata(input, cwd) {
  const sources = [input, input?.context, input?.event_payload, input?.payload]
    .filter((value) => value && typeof value === 'object');
  const tmuxSources = [
    ...sources,
    ...sources
      .map((value) => value.tmux)
      .filter((value) => value && typeof value === 'object'),
  ];

  function pickString(keys) {
    for (const source of tmuxSources) {
      for (const key of keys) {
        const value = source[key];
        if (typeof value === 'string' && value.trim()) {
          return value.trim();
        }
      }
    }
    return '';
  }

  function pickInteger(keys) {
    for (const source of tmuxSources) {
      for (const key of keys) {
        const value = parseIntegerish(source[key]);
        if (value !== null) return value;
      }
    }
    return null;
  }

  function pickBoolean(keys) {
    for (const source of tmuxSources) {
      for (const key of keys) {
        const value = parseBoolish(source[key]);
        if (value !== null) return value;
      }
    }
    return null;
  }

  const direct = {};
  const tmuxSession = pickString(['tmux_session', 'tmuxSession', 'session']);
  const tmuxWindow = pickString(['tmux_window', 'tmuxWindow', 'window']);
  const tmuxPane = pickString(['tmux_pane', 'tmuxPane', 'pane']);
  const tmuxPaneTty = pickString(['tmux_pane_tty', 'tmuxPaneTty', 'pane_tty', 'paneTty']);
  const tmuxClientCount = pickInteger(['tmux_client_count', 'tmuxClientCount', 'client_count', 'clientCount']);
  const tmuxAttached = pickBoolean(['tmux_attached', 'tmuxAttached', 'attached']);

  if (tmuxSession) direct.tmux_session = tmuxSession;
  if (tmuxWindow) direct.tmux_window = tmuxWindow;
  if (tmuxPane) direct.tmux_pane = tmuxPane;
  if (tmuxPaneTty) direct.tmux_pane_tty = tmuxPaneTty;
  if (tmuxClientCount !== null) direct.tmux_client_count = tmuxClientCount;
  if (tmuxAttached !== null) direct.tmux_attached = tmuxAttached;

  const tmuxTarget = process.env.TMUX_PANE || '';
  if (process.env.TMUX || tmuxTarget) {
    const result = spawnSync(
      'tmux',
      [
        'display-message',
        '-p',
        ...(tmuxTarget ? ['-t', tmuxTarget] : []),
        '#{session_name}\u001f#{window_index}\u001f#{pane_id}\u001f#{pane_tty}\u001f#{session_attached}',
      ],
      { cwd, encoding: 'utf8' },
    );
    if (result.status === 0) {
      const [session, window, pane, paneTty, attachedCount] = result.stdout.trim().split('\u001f');
      const clientCount = parseIntegerish(attachedCount);
      if (session && !direct.tmux_session) direct.tmux_session = session;
      if (window && !direct.tmux_window) direct.tmux_window = window;
      if (pane && !direct.tmux_pane) direct.tmux_pane = pane;
      if (paneTty && !direct.tmux_pane_tty) direct.tmux_pane_tty = paneTty;
      if (clientCount !== null) {
        if (direct.tmux_client_count === undefined) direct.tmux_client_count = clientCount;
        if (direct.tmux_attached === undefined) direct.tmux_attached = clientCount > 0;
      }
    }
  }

  return Object.keys(direct).length > 0 ? direct : null;
}

function truncate(text, maxLen = 200) {
  if (!text || typeof text !== 'string') return '';
  const trimmed = text.trim();
  return trimmed.length <= maxLen ? trimmed : trimmed.slice(0, maxLen) + '…';
}

function maybeWritePromptSubmitState(repoRoot, provider, eventName, input) {
  if (!repoRoot) return;
  const normalizedEvent = String(eventName || '').trim().toLowerCase();
  if (
    normalizedEvent !== 'userpromptsubmit' &&
    normalizedEvent !== 'user-prompt-submit' &&
    normalizedEvent !== 'prompt-submitted' &&
    normalizedEvent !== 'session.prompt-submitted'
  ) {
    return;
  }

  try {
    if (!worktreeRoot) return;
    const promptText = input.prompt || input.user_prompt || input.message || '';
    const path = join(worktreeRoot, '.clawhip', 'state', 'prompt-submit.json');
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, JSON.stringify({
      observed_at: new Date().toISOString(),
      provider,
      event_name: eventName,
      session_id: input.session_id || input.sessionId || null,
      turn_id: input.turn_id || input.turnId || null,
      prompt_summary: truncate(promptText),
    }, null, 2) + '\n');
  } catch {}
}

function maybeEnrichStopEvent(repoRoot, payload, eventName) {
  if (!repoRoot) return;
  const normalizedEvent = String(eventName || '').trim().toLowerCase();
  if (normalizedEvent !== 'stop' && normalizedEvent !== 'sessionstop' && normalizedEvent !== 'session-stopped') {
    return;
  }
  try {
    if (!worktreeRoot) return;
    const path = join(worktreeRoot, '.clawhip', 'state', 'prompt-submit.json');
    if (!existsSync(path)) return;
    const raw = readFileSync(path, 'utf8');
    const state = parseJson(raw, null);
    if (!state) return;
    payload.stop_context = {
      last_prompt_at: state.observed_at || null,
      last_prompt_summary: state.prompt_summary || null,
      last_turn_id: state.turn_id || null,
    };
  } catch {}
}

async function main() {
  const provider = arg('--provider') || process.env.CLAWHIP_PROVIDER || 'unknown';
  const cwd = resolve(process.cwd());
  const raw = await readStdin();
  const input = parseJson(raw, {});
  const directory = resolve(input.cwd || cwd);
  const worktreeRoot = inferWorktreeRoot(directory);
  const repoRoot = inferRepoRoot(directory);
  const inGitRepo = Boolean(repoRoot);
  const tmuxMetadata = collectTmuxMetadata(input, cwd);
  const eventName =
    input.hook_event_name || input.hookEventName || input.event_name || input.event || 'unknown';
  const payload = {
    provider,
    source: provider,
    directory,
    event_name: eventName,
    hook_event_name: eventName,
    normalization_outcome: inGitRepo ? 'git' : 'non_git',
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
  if (worktreeRoot) {
    payload.worktree_path = worktreeRoot;
  }
  if (repoRoot) {
    payload.repo_path = repoRoot;
  }
  if (repoRoot || worktreeRoot) {
    payload.repo_name = basename(repoRoot || worktreeRoot);
    payload.normalization_outcome = 'git';
    payload.routeable = true;
  } else {
    payload.normalization_outcome = 'non_git';
    payload.routeable = false;
  }
  if (tmuxMetadata) {
    Object.assign(payload, tmuxMetadata);
  }
  if (inGitRepo) {
    payload.repo_path = repoRoot;
    payload.worktree_path = directory;
    payload.repo_name = basename(repoRoot);
  }

  const augmentation = inGitRepo ? await collectAugmentation(repoRoot, payload) : null;
  if (augmentation) {
    payload.augmentation = augmentation;
  }

  maybeWritePromptSubmitState(worktreeRoot, provider, eventName, input);
  maybeEnrichStopEvent(worktreeRoot, payload, eventName);

  const result = spawnSync('clawhip', ['native', 'hook', '--provider', provider], {
    input: JSON.stringify(payload),
    encoding: 'utf8',
    stdio: ['pipe', 'inherit', 'inherit'],
  });

  if (result.error) {
    const detail =
      typeof result.error?.message === 'string' && result.error.message.trim()
        ? result.error.message.trim()
        : String(result.error);
    console.error(`[clawhip] failed to launch native hook bridge: ${detail}`);
    process.exit(typeof result.status === 'number' ? result.status : 1);
  }

  if (typeof result.status === 'number' && result.status !== 0) {
    console.error(`[clawhip] native hook bridge exited with status ${result.status}`);
    process.exit(result.status);
  }

  if (result.signal) {
    console.error(`[clawhip] native hook bridge terminated by signal ${result.signal}`);
    process.exit(1);
  }
}

main().catch((error) => {
  const detail =
    typeof error?.stack === 'string' && error.stack.trim()
      ? error.stack.trim()
      : typeof error?.message === 'string' && error.message.trim()
        ? error.message.trim()
        : String(error);
  console.error(`[clawhip] native hook wrapper failed: ${detail}`);
  process.exit(1);
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

fn infer_repo_root(directory: &str) -> Option<PathBuf> {
    // Use --git-common-dir to derive the main repo root even when inside a
    // worktree.  --show-toplevel returns the worktree root which is wrong for
    // the repo_path field (issue #182).
    if let Some(common_dir) = Command::new("git")
        .args([
            "-C",
            directory,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        && let Some(repo_root) = Path::new(&common_dir).parent()
    {
        return Some(
            repo_root
                .canonicalize()
                .unwrap_or_else(|_| repo_root.to_path_buf()),
        );
    }

    // Fallback: --show-toplevel (correct for non-worktree checkouts).
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
        let path = PathBuf::from(trimmed);
        Some(path.canonicalize().unwrap_or(path))
    }
}

fn canonicalize_path(path: &str) -> Option<String> {
    let candidate = Path::new(path);
    if !candidate.exists() {
        return None;
    }
    candidate
        .canonicalize()
        .ok()
        .map(|value| value.to_string_lossy().into_owned())
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

/// For stop events, propagate the last-prompt context into top-level fields
/// so templates and renderers can reference them without digging into nested
/// objects.
fn apply_stop_context(payload: &mut Map<String, Value>, stop_context: Option<&Value>) {
    let Some(stop_context) = stop_context.and_then(Value::as_object) else {
        return;
    };

    if let Some(summary) = stop_context
        .get("last_prompt_summary")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !payload.contains_key("summary") {
            payload.insert("summary".into(), json!(summary));
        }
        payload.insert("last_prompt_summary".into(), json!(summary));
    }

    if let Some(at) = stop_context
        .get("last_prompt_at")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        payload.insert("last_prompt_at".into(), json!(at));
    }

    if let Some(turn_id) = stop_context
        .get("last_turn_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        payload.insert("last_turn_id".into(), json!(turn_id));
    }

    payload.insert("stop_context".into(), Value::Object(stop_context.clone()));
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

fn first_bool(payload: &Value, pointers: &[&str]) -> Option<bool> {
    pointers.iter().find_map(|pointer| {
        payload.pointer(pointer).and_then(|value| match value {
            Value::Bool(value) => Some(*value),
            Value::Number(value) => value.as_u64().map(|number| number != 0),
            Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "attached" => Some(true),
                "0" | "false" | "no" | "detached" => Some(false),
                _ => None,
            },
            _ => None,
        })
    })
}

fn first_u64(payload: &Value, pointers: &[&str]) -> Option<u64> {
    pointers.iter().find_map(|pointer| {
        payload.pointer(pointer).and_then(|value| match value {
            Value::Number(number) => number.as_u64(),
            Value::String(value) => value.trim().parse().ok(),
            _ => None,
        })
    })
}

fn copy_string_field(
    normalized: &mut Map<String, Value>,
    payload: &Value,
    key: &str,
    pointers: &[&str],
) {
    if let Some(value) = first_string(payload, pointers) {
        normalized.insert(key.to_string(), json!(value));
    }
}

fn copy_bool_field(
    normalized: &mut Map<String, Value>,
    payload: &Value,
    key: &str,
    pointers: &[&str],
) {
    if let Some(value) = first_bool(payload, pointers) {
        normalized.insert(key.to_string(), json!(value));
    }
}

fn copy_u64_field(
    normalized: &mut Map<String, Value>,
    payload: &Value,
    key: &str,
    pointers: &[&str],
) {
    if let Some(value) = first_u64(payload, pointers) {
        normalized.insert(key.to_string(), json!(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn maps_all_shared_hook_events() {
        let dir = tempdir().expect("tempdir");
        let repo = dir.path().join("clawhip");
        git_init_repo(&repo);
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
                "directory": repo,
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
    fn ignores_project_metadata_from_project_json_for_routing_identity() {
        let dir = tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".clawhip")).unwrap();
        let output = Command::new("git")
            .args(["init"])
            .current_dir(&repo)
            .output()
            .expect("git init");
        assert!(
            output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::write(
            repo.join(CLAWHIP_PROJECT_FILE),
            serde_json::to_string_pretty(&json!({
                "id": "clawhip-core",
                "name": "wrong-name",
                "repo_name": "wrong-name"
            }))
            .unwrap(),
        )
        .unwrap();

        let event = incoming_event_from_native_hook_json(&json!({
            "provider": "codex",
            "directory": repo,
            "event_name": "SessionStart",
            "event_payload": {}
        }))
        .expect("event");

        assert_eq!(event.payload["repo_name"], json!("repo"));
        assert_eq!(event.payload["normalization_outcome"], json!("git"));
        assert!(event.payload.get("project_metadata").is_none());
    }

    #[test]
    fn marks_non_git_native_payloads_and_omits_repo_fields() {
        let dir = tempdir().expect("tempdir");
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).expect("create scratch");

        let event = incoming_event_from_native_hook_json(&json!({
            "provider": "codex",
            "directory": scratch,
            "event_name": "SessionStart",
            "event_payload": {}
        }))
        .expect("event");

        assert_eq!(event.payload["normalization_outcome"], json!("non_git"));
        assert!(event.payload.get("repo_path").is_none());
        assert!(event.payload.get("worktree_path").is_none());
        assert!(event.payload.get("repo_name").is_none());
    }

    #[test]
    fn augmentation_can_add_context_without_overriding_base_fields() {
        let dir = tempdir().expect("tempdir");
        let repo = dir.path().join("clawhip");
        git_init_repo(&repo);
        let event = incoming_event_from_native_hook_json(&json!({
            "provider": "claude-code",
            "directory": repo,
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

    #[test]
    fn generated_hook_script_surfaces_bridge_failures() {
        let script = generated_hook_script();
        assert!(script.contains("stdio: ['pipe', 'inherit', 'inherit']"));
        assert!(script.contains("failed to launch native hook bridge"));
        assert!(script.contains("native hook bridge exited with status"));
        assert!(script.contains("native hook bridge terminated by signal"));
        assert!(script.contains("native hook wrapper failed"));
        assert!(script.contains("process.exit(1);"));
    }

    #[test]
    fn generated_hook_script_e2e_surfaces_bridge_stderr_and_exit_code() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Stdio;

        let node_check = Command::new("node").arg("--version").output();
        let Ok(node_check) = node_check else {
            eprintln!("skipping native hook e2e: node unavailable");
            return;
        };
        if !node_check.status.success() {
            eprintln!("skipping native hook e2e: node unavailable");
            return;
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let hook_dir = repo.join(".clawhip/hooks");
        std::fs::create_dir_all(&hook_dir).expect("create hook dir");

        let hook_path = hook_dir.join("native-hook.mjs");
        std::fs::write(&hook_path, generated_hook_script()).expect("write hook script");
        let mut hook_perms = std::fs::metadata(&hook_path)
            .expect("hook metadata")
            .permissions();
        hook_perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, hook_perms).expect("chmod hook");

        let fake_bin = temp.path().join("bin");
        std::fs::create_dir_all(&fake_bin).expect("create fake bin");
        let fake_clawhip = fake_bin.join("clawhip");
        std::fs::write(
            &fake_clawhip,
            "#!/bin/sh\necho 'fake native hook bridge failure' >&2\nexit 7\n",
        )
        .expect("write fake clawhip");
        let mut fake_perms = std::fs::metadata(&fake_clawhip)
            .expect("fake metadata")
            .permissions();
        fake_perms.set_mode(0o755);
        std::fs::set_permissions(&fake_clawhip, fake_perms).expect("chmod fake clawhip");

        let path = std::env::var("PATH").unwrap_or_default();
        let mut child = Command::new("node")
            .arg(&hook_path)
            .arg("--provider")
            .arg("codex")
            .current_dir(&repo)
            .env("PATH", format!("{}:{path}", fake_bin.display()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn node hook");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(br#"{"event_name":"SessionStart","cwd":"."}"#)
            .expect("write payload");
        let output = child.wait_with_output().expect("wait for hook");
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(7), "stderr: {stderr}");
        assert!(
            stderr.contains("fake native hook bridge failure"),
            "stderr: {stderr}"
        );
        assert!(
            stderr.contains("native hook bridge exited with status 7"),
            "stderr: {stderr}"
        );
    }

    #[test]
    fn preserves_tmux_metadata_from_native_payloads() {
        let event = incoming_event_from_native_hook_json(&json!({
            "provider": "codex",
            "directory": "/repo/clawhip",
            "event_name": "SessionStart",
            "tmux_session": "omx-clawhip-dev",
            "tmux_window": "3",
            "tmux_pane": "%17",
            "tmux_pane_tty": "/dev/pts/5",
            "tmux_attached": true,
            "tmux_client_count": 2,
            "event_payload": {}
        }))
        .expect("event");

        assert_eq!(event.payload["tmux_session"], json!("omx-clawhip-dev"));
        assert_eq!(event.payload["tmux_window"], json!("3"));
        assert_eq!(event.payload["tmux_pane"], json!("%17"));
        assert_eq!(event.payload["tmux_pane_tty"], json!("/dev/pts/5"));
        assert_eq!(event.payload["tmux_attached"], json!(true));
        assert_eq!(event.payload["tmux_client_count"], json!(2));
    }

    #[test]
    fn generated_hook_script_mentions_tmux_metadata_collection() {
        let script = generated_hook_script();
        assert!(script.contains("collectTmuxMetadata"));
        assert!(script.contains("tmux_session"));
        assert!(script.contains("tmux_client_count"));
        assert!(script.contains("tmux_attached"));
    }

    #[test]
    fn generated_hook_script_mentions_prompt_submit_state_recording() {
        let script = generated_hook_script();
        assert!(script.contains("maybeWritePromptSubmitState"));
        assert!(script.contains(".clawhip', 'state', 'prompt-submit.json"));
    }

    #[test]
    fn generated_hook_script_mentions_worktree_repo_root_fallback() {
        let script = generated_hook_script();
        assert!(script.contains("function inferRepoRoot(cwd)"));
        assert!(script.contains("function inferWorktreeRoot(cwd)"));
        assert!(script.contains("--git-common-dir"));
        assert!(script.contains("normalization_outcome"));
        assert!(!script.contains("loadProjectMetadata"));
    }

    #[test]
    fn infer_repo_root_returns_main_repo_for_worktree() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("create repo dir");
        init_git_repo(&repo);
        std::fs::write(repo.join("README.md"), "init\n").expect("write");
        git(&repo, &["add", "README.md"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "init",
            ],
        );
        git(&repo, &["branch", "issue-182"]);

        let wt = temp.path().join("wt-issue-182");
        git(
            &repo,
            &["worktree", "add", &wt.to_string_lossy(), "issue-182"],
        );

        let result = super::infer_repo_root(&wt.to_string_lossy());
        let expected = repo.canonicalize().expect("canonical");
        assert_eq!(
            result,
            Some(expected),
            "infer_repo_root should return the main repo, not the worktree"
        );
    }

    fn init_git_repo(dir: &std::path::Path) {
        git(dir, &["init"]);
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn stop_event_payload_surfaces_stop_context_summary() {
        let event = incoming_event_from_native_hook_json(&json!({
            "provider": "claude-code",
            "directory": "/repo/clawhip",
            "event_name": "Stop",
            "stop_context": {
                "last_prompt_at": "2026-04-10T12:34:56Z",
                "last_prompt_summary": "wire up event provenance for issue 188",
                "last_turn_id": "turn-99"
            }
        }))
        .expect("event");

        assert_eq!(event.kind, "session.stopped");
        assert_eq!(
            event.payload["last_prompt_summary"],
            json!("wire up event provenance for issue 188")
        );
        assert_eq!(
            event.payload["last_prompt_at"],
            json!("2026-04-10T12:34:56Z")
        );
        assert_eq!(event.payload["last_turn_id"], json!("turn-99"));
        // summary is backfilled from last_prompt_summary when absent, so the
        // default renderer's inline/compact mode has something meaningful to show.
        assert_eq!(
            event.payload["summary"],
            json!("wire up event provenance for issue 188")
        );
        // The original nested stop_context is retained for callers that want it.
        assert!(event.payload["stop_context"].is_object());
    }

    #[test]
    fn stop_event_without_stop_context_does_not_invent_summary() {
        let event = incoming_event_from_native_hook_json(&json!({
            "provider": "claude-code",
            "directory": "/repo/clawhip",
            "event_name": "Stop"
        }))
        .expect("event");

        assert_eq!(event.kind, "session.stopped");
        assert!(event.payload.get("stop_context").is_none());
        assert!(event.payload.get("last_prompt_summary").is_none());
        assert!(event.payload.get("summary").is_none());
    }

    #[test]
    fn stop_event_respects_preexisting_summary_over_stop_context() {
        let event = incoming_event_from_native_hook_json(&json!({
            "provider": "claude-code",
            "directory": "/repo/clawhip",
            "event_name": "Stop",
            "stop_context": {
                "last_prompt_summary": "older prompt"
            },
            "augmentation": {
                "summary": "explicit override"
            }
        }))
        .expect("event");

        // augmentation ran first and set summary; stop_context must not clobber it
        assert_eq!(event.payload["summary"], json!("explicit override"));
        // but the raw prompt context is still exposed for renderers that want it
        assert_eq!(event.payload["last_prompt_summary"], json!("older prompt"));
    }

    #[test]
    fn hook_script_saves_prompt_summary_and_enriches_stop_events() {
        // Sanity-check the embedded JS hook script text so refactors of the
        // string constant don't silently drop the stop-context plumbing.
        let script = super::generated_hook_script();
        assert!(script.contains("maybeEnrichStopEvent"));
        assert!(script.contains("prompt_summary"));
        assert!(script.contains("stop_context"));
        assert!(script.contains("last_prompt_summary"));
    }

    fn git_init_repo(repo: &Path) {
        std::fs::create_dir_all(repo).expect("create repo");
        let output = Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .expect("git init");
        assert!(
            output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
