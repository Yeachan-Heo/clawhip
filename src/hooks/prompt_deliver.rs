use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;
use tokio::time::sleep;

use crate::Result;
use crate::cli::DeliverArgs;
use crate::source::tmux::{content_hash, tmux_bin};

pub const DEFAULT_MAX_ENTERS: u32 = 4;
const DEFAULT_TUI_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(500);
const DEFAULT_VERIFY_DELAY: Duration = Duration::from_millis(350);
const PROMPT_SUBMIT_MARKER: &str = ".clawhip/state/prompt-submit.json";
const NATIVE_HOOK_SCRIPT: &str = ".clawhip/hooks/native-hook.mjs";
const PROMPT_CHARS: &[char] = &['$', '%', '>', '#', '❯', '›'];
const TARGET_PANE_FORMAT: &str =
    "#{session_name}\t#{pane_id}\t#{pane_pid}\t#{pane_current_command}\t#{pane_current_path}";

#[derive(Debug, Clone)]
pub struct PromptDeliverConfig {
    pub session: String,
    pub prompt: String,
    pub max_enters: u32,
    pub tui_timeout: Duration,
    pub poll_interval: Duration,
    pub verify_delay: Duration,
}

impl PromptDeliverConfig {
    pub fn new(session: String, prompt: String) -> Self {
        Self {
            session,
            prompt,
            max_enters: DEFAULT_MAX_ENTERS,
            tui_timeout: DEFAULT_TUI_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            verify_delay: DEFAULT_VERIFY_DELAY,
        }
    }
}

impl From<DeliverArgs> for PromptDeliverConfig {
    fn from(value: DeliverArgs) -> Self {
        Self {
            session: value.session,
            prompt: value.prompt,
            max_enters: value.max_enters.max(1),
            tui_timeout: DEFAULT_TUI_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            verify_delay: DEFAULT_VERIFY_DELAY,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeliveryResult {
    pub delivered: bool,
    pub enter_attempts: u32,
    pub provider: ProviderKind,
    pub pane_id: String,
    pub workdir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Omc,
    Omx,
}

impl ProviderKind {
    fn label(self) -> &'static str {
        match self {
            Self::Omc => "claude-code",
            Self::Omx => "codex",
        }
    }
}

#[derive(Debug, Clone)]
struct HookSetup {
    workdir: PathBuf,
    marker_path: PathBuf,
    supported_providers: Vec<ProviderKind>,
    sources: Vec<&'static str>,
}

#[derive(Debug, Clone)]
struct PaneTarget {
    session: String,
    pane_id: String,
    pane_pid: u32,
    current_command: String,
    cwd: PathBuf,
}

#[derive(Debug, Clone)]
struct ProcessInfo {
    pid: u32,
    ppid: u32,
    command: String,
    args: String,
}

pub async fn run(args: DeliverArgs) -> Result<()> {
    let config = PromptDeliverConfig::from(args);
    let result = deliver(&config).await?;
    println!(
        "Delivered prompt to {} session '{}' via {} after {} Enter attempt(s) (pane={}, cwd={})",
        result.provider.label(),
        config.session,
        result.provider.label(),
        result.enter_attempts,
        result.pane_id,
        result.workdir.display()
    );
    Ok(())
}

pub async fn deliver(config: &PromptDeliverConfig) -> Result<DeliveryResult> {
    let pane = resolve_target_pane(&config.session).await?;
    let hook_setup = detect_hook_setup(&pane.cwd)?;
    let provider = detect_active_provider(&pane, &hook_setup).await?;

    wait_for_tui_ready(&pane.pane_id, config.tui_timeout, config.poll_interval).await?;

    let baseline_marker = read_marker_hash(&hook_setup.marker_path)?;
    send_literal_keys(&pane.pane_id, &config.prompt).await?;

    for attempt in 1..=config.max_enters.max(1) {
        send_key(&pane.pane_id, "Enter").await?;
        sleep(config.verify_delay).await;

        if marker_changed(&hook_setup.marker_path, baseline_marker)? {
            return Ok(DeliveryResult {
                delivered: true,
                enter_attempts: attempt,
                provider,
                pane_id: pane.pane_id,
                workdir: hook_setup.workdir,
            });
        }
    }

    let last_line = capture_last_line(&pane.pane_id).await.unwrap_or_default();
    Err(format!(
        "prompt delivery to '{}' failed after {} Enter attempt(s): {} hook did not record {} (marker: {}, pane command: {}, sources: {}, last line: {})",
        pane.session,
        config.max_enters.max(1),
        provider.label(),
        PROMPT_SUBMIT_MARKER,
        hook_setup.marker_path.display(),
        pane.current_command,
        hook_setup.sources.join(", "),
        format_last_line(&last_line),
    )
    .into())
}

async fn resolve_target_pane(session: &str) -> Result<PaneTarget> {
    let output = Command::new(tmux_bin())
        .arg("display-message")
        .arg("-p")
        .arg("-t")
        .arg(session)
        .arg(TARGET_PANE_FORMAT)
        .output()
        .await?;
    if !output.status.success() {
        return Err(tmux_stderr(&output.stderr).into());
    }

    let line = String::from_utf8(output.stdout)?;
    let mut parts = line.trim_end().splitn(5, '\t');
    let resolved_session = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("tmux did not return a session name for '{session}'"))?;
    let pane_id = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("tmux did not return an active pane for '{resolved_session}'"))?;
    let pane_pid = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("tmux did not return a pane pid for '{resolved_session}'"))?
        .parse::<u32>()?;
    let current_command = parts.next().map(str::trim).unwrap_or_default().to_string();
    let cwd = parts
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("tmux did not return a pane cwd for '{resolved_session}'"))?;

    Ok(PaneTarget {
        session: resolved_session.to_string(),
        pane_id: pane_id.to_string(),
        pane_pid,
        current_command,
        cwd,
    })
}

fn detect_hook_setup(cwd: &Path) -> Result<HookSetup> {
    for directory in cwd.ancestors() {
        let mut providers = Vec::new();
        let mut sources = Vec::new();
        let has_native_script = has_native_prompt_submit_hook_script(directory);

        if has_claude_prompt_submit_hook(directory) && has_native_script {
            providers.push(ProviderKind::Omc);
            sources.push(".claude/settings.json + .clawhip/hooks/native-hook.mjs");
        }
        if has_codex_prompt_submit_hook(directory) && has_native_script {
            providers.push(ProviderKind::Omx);
            sources.push(".codex/hooks.json + .clawhip/hooks/native-hook.mjs");
        }
        if has_omx_prompt_submit_hook(directory) && !providers.contains(&ProviderKind::Omx) {
            providers.push(ProviderKind::Omx);
            sources.push(".omx/hooks/clawhip.mjs");
        }

        if !providers.is_empty() {
            return Ok(HookSetup {
                workdir: directory.to_path_buf(),
                marker_path: directory.join(PROMPT_SUBMIT_MARKER),
                supported_providers: providers,
                sources,
            });
        }
    }

    Err(format!(
        "refusing delivery: '{}' is not inside a repo/workdir with prompt-submit-aware hook setup; run `clawhip hooks install --all --scope project` or install a prompt-submit-aware legacy OMX bridge in .omx/hooks/",
        cwd.display()
    )
    .into())
}

fn has_claude_prompt_submit_hook(root: &Path) -> bool {
    let path = root.join(".claude/settings.json");
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    value
        .pointer("/hooks/UserPromptSubmit")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|entries| entries.iter().any(json_hook_entry_mentions_clawhip))
}

fn json_hook_entry_mentions_clawhip(entry: &serde_json::Value) -> bool {
    entry
        .get("hooks")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(command_mentions_clawhip)
            })
        })
}

fn has_codex_prompt_submit_hook(root: &Path) -> bool {
    has_codex_prompt_submit_hook_json(root) || has_codex_prompt_submit_hook_toml(root)
}

fn has_codex_prompt_submit_hook_json(root: &Path) -> bool {
    let path = root.join(".codex/hooks.json");
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    value
        .pointer("/hooks/UserPromptSubmit")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|entries| entries.iter().any(json_hook_entry_mentions_clawhip))
}

fn has_codex_prompt_submit_hook_toml(root: &Path) -> bool {
    let path = root.join(".codex/config.toml");
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&content) else {
        return false;
    };
    value
        .get("native_hooks")
        .and_then(|native| native.get("events"))
        .and_then(|events| events.get("UserPromptSubmit"))
        .and_then(toml::Value::as_str)
        .is_some_and(command_mentions_clawhip)
}

fn has_native_prompt_submit_hook_script(root: &Path) -> bool {
    let path = root.join(NATIVE_HOOK_SCRIPT);
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    content.contains("prompt-submit.json") || content.contains("maybeWritePromptSubmitState")
}

fn has_omx_prompt_submit_hook(root: &Path) -> bool {
    let path = root.join(".omx/hooks/clawhip.mjs");
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    content.contains("prompt-submit.json") || content.contains("prompt_submit_recorded")
}

fn command_mentions_clawhip(command: &str) -> bool {
    let normalized = command.trim().to_ascii_lowercase();
    normalized.contains("clawhip native hook")
        || normalized.contains(".clawhip/hooks/native-hook.mjs")
        || normalized.contains("native-hook.mjs")
}

async fn detect_active_provider(pane: &PaneTarget, hook_setup: &HookSetup) -> Result<ProviderKind> {
    let pane_command = pane.current_command.to_ascii_lowercase();
    if let Some(provider) = hook_setup
        .supported_providers
        .iter()
        .copied()
        .find(|provider| provider_matches_command(*provider, &pane_command))
    {
        return Ok(provider);
    }

    let process_tree = read_process_tree(pane.pane_pid).await.unwrap_or_default();
    if let Some(provider) = hook_setup
        .supported_providers
        .iter()
        .copied()
        .find(|provider| process_tree_matches_provider(&process_tree, *provider))
    {
        return Ok(provider);
    }

    Err(format!(
        "refusing delivery: '{}' is not an active Codex/Claude (OMC/OMX-compatible) pane (cwd={}, command={}, pane_pid={})",
        pane.session,
        pane.cwd.display(),
        if pane.current_command.is_empty() {
            "<unknown>"
        } else {
            pane.current_command.as_str()
        },
        pane.pane_pid,
    )
    .into())
}

fn provider_matches_command(provider: ProviderKind, command: &str) -> bool {
    let aliases = match provider {
        ProviderKind::Omc => ["omc", "claude", "claude-code", "openclaw"].as_slice(),
        ProviderKind::Omx => ["omx", "codex", "oh-my-codex"].as_slice(),
    };

    aliases.iter().any(|alias| command.contains(alias))
}

async fn read_process_tree(root_pid: u32) -> Result<Vec<ProcessInfo>> {
    let output = Command::new("ps")
        .args(["-ax", "-o", "pid=,ppid=,comm=,args="])
        .output()
        .await?;
    if !output.status.success() {
        return Err(format!(
            "ps failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }

    let stdout = String::from_utf8(output.stdout)?;
    let processes = stdout
        .lines()
        .filter_map(parse_process_line)
        .collect::<Vec<_>>();

    let mut by_parent: HashMap<u32, Vec<ProcessInfo>> = HashMap::new();
    for process in &processes {
        by_parent
            .entry(process.ppid)
            .or_default()
            .push(process.clone());
    }

    let mut collected = Vec::new();
    let mut stack = vec![root_pid];
    let mut seen = HashSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some(children) = by_parent.get(&pid) {
            for child in children {
                collected.push(child.clone());
                stack.push(child.pid);
            }
        }
    }

    Ok(collected)
}

fn parse_process_line(line: &str) -> Option<ProcessInfo> {
    let (pid_field, rest) = take_process_field(line)?;
    let (ppid_field, rest) = take_process_field(rest)?;
    let (command_field, rest) = take_process_field(rest)?;

    Some(ProcessInfo {
        pid: pid_field.parse().ok()?,
        ppid: ppid_field.parse().ok()?,
        command: command_field.to_string(),
        args: rest.trim().to_string(),
    })
}

fn take_process_field(input: &str) -> Option<(&str, &str)> {
    let trimmed = input.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    let mut end = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        if ch.is_whitespace() {
            end = idx;
            break;
        }
    }

    let field = &trimmed[..end];
    let remainder = &trimmed[end..];
    Some((field, remainder))
}

fn process_tree_matches_provider(processes: &[ProcessInfo], provider: ProviderKind) -> bool {
    processes.iter().any(|process| {
        let command = process.command.to_ascii_lowercase();
        let args = process.args.to_ascii_lowercase();
        provider_matches_command(provider, &command) || provider_matches_command(provider, &args)
    })
}

fn read_marker_hash(path: &Path) -> Result<Option<u64>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(content_hash(&fs::read_to_string(path)?)))
}

fn marker_changed(path: &Path, baseline: Option<u64>) -> Result<bool> {
    let current = read_marker_hash(path)?;
    Ok(match (baseline, current) {
        (None, Some(_)) => true,
        (Some(before), Some(after)) => before != after,
        _ => false,
    })
}

async fn wait_for_tui_ready(
    target: &str,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Ok(());
        }

        match capture_last_line(target).await {
            Ok(line) if has_prompt_char(&line) => return Ok(()),
            Ok(_) => {}
            Err(_) => {}
        }

        sleep(poll_interval).await;
    }
}

fn has_prompt_char(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    PROMPT_CHARS
        .iter()
        .any(|ch| trimmed.ends_with(*ch) || trimmed.ends_with(&format!("{ch} ")))
}

async fn capture_last_line(target: &str) -> Result<String> {
    let output = Command::new(tmux_bin())
        .arg("capture-pane")
        .arg("-p")
        .arg("-t")
        .arg(target)
        .arg("-S")
        .arg("-1")
        .output()
        .await?;
    if !output.status.success() {
        return Err(tmux_stderr(&output.stderr).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn send_literal_keys(target: &str, text: &str) -> Result<()> {
    let output = Command::new(tmux_bin())
        .arg("send-keys")
        .arg("-t")
        .arg(target)
        .arg("-l")
        .arg(text)
        .output()
        .await?;
    if !output.status.success() {
        return Err(tmux_stderr(&output.stderr).into());
    }
    Ok(())
}

async fn send_key(target: &str, key: &str) -> Result<()> {
    let output = Command::new(tmux_bin())
        .arg("send-keys")
        .arg("-t")
        .arg(target)
        .arg(key)
        .output()
        .await?;
    if !output.status.success() {
        return Err(tmux_stderr(&output.stderr).into());
    }
    Ok(())
}

fn format_last_line(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        "<empty>".into()
    } else {
        trimmed.into()
    }
}

fn tmux_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn has_prompt_char_detects_common_shells() {
        assert!(has_prompt_char("user@host:~$ "));
        assert!(has_prompt_char("~ %"));
        assert!(has_prompt_char(">>> "));
        assert!(has_prompt_char("root@host:/# "));
        assert!(has_prompt_char("❯"));
        assert!(has_prompt_char("›"));
    }

    #[test]
    fn has_prompt_char_rejects_empty_and_output_lines() {
        assert!(!has_prompt_char(""));
        assert!(!has_prompt_char("   "));
        assert!(!has_prompt_char("compiling clawhip v0.5.0"));
        assert!(!has_prompt_char("error[E0308]: mismatched types"));
    }

    #[test]
    fn config_defaults_are_sensible() {
        let config = PromptDeliverConfig::new("test".into(), "hello".into());
        assert_eq!(config.max_enters, DEFAULT_MAX_ENTERS);
        assert_eq!(config.tui_timeout, Duration::from_secs(30));
        assert_eq!(config.poll_interval, Duration::from_millis(500));
        assert_eq!(config.verify_delay, Duration::from_millis(350));
    }

    #[test]
    fn detect_hook_setup_walks_to_parent_workdir() {
        let tempdir = tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        let nested = repo.join("src/bin");
        fs::create_dir_all(repo.join(".codex")).expect("create codex dir");
        fs::create_dir_all(repo.join(".clawhip/hooks")).expect("create hook dir");
        fs::create_dir_all(&nested).expect("create nested dir");
        fs::write(
            repo.join(".codex/hooks.json"),
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"node ./.clawhip/hooks/native-hook.mjs --provider codex"}]}]}}"#,
        )
        .expect("write codex hooks");
        fs::write(
            repo.join(".clawhip/hooks/native-hook.mjs"),
            "function maybeWritePromptSubmitState() { return '.clawhip/state/prompt-submit.json'; }\n",
        )
        .expect("write native hook");

        let setup = detect_hook_setup(&nested).expect("hook setup");
        assert_eq!(setup.workdir, repo);
        assert_eq!(setup.supported_providers, vec![ProviderKind::Omx]);
    }

    #[test]
    fn detect_hook_setup_recognizes_omc_user_prompt_submit_hooks() {
        let tempdir = tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        fs::create_dir_all(repo.join(".claude")).expect("create claude dir");
        fs::create_dir_all(repo.join(".clawhip/hooks")).expect("create hook dir");
        fs::write(
            repo.join(".claude/settings.json"),
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"node ./.clawhip/hooks/native-hook.mjs --provider claude-code"}]}]}}"#,
        )
        .expect("write settings");
        fs::write(
            repo.join(".clawhip/hooks/native-hook.mjs"),
            "function maybeWritePromptSubmitState() { return '.clawhip/state/prompt-submit.json'; }\n",
        )
        .expect("write native hook");

        let setup = detect_hook_setup(&repo).expect("hook setup");
        assert_eq!(setup.supported_providers, vec![ProviderKind::Omc]);
    }

    #[test]
    fn detect_hook_setup_rejects_old_omx_bridge_without_prompt_submit_support() {
        let tempdir = tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        let hook_dir = repo.join(".omx/hooks");
        fs::create_dir_all(&hook_dir).expect("create hook dir");
        fs::write(
            hook_dir.join("clawhip.mjs"),
            "import { createClawhipOmxClient } from './clawhip-sdk.mjs';\nexport async function onHookEvent(event, sdk) { return { ok: true }; }\n",
        )
        .expect("write old hook");

        let error = detect_hook_setup(&repo).expect_err("old bridge should be rejected");
        assert!(error.to_string().contains("prompt-submit-aware hook setup"));
    }

    #[test]
    fn parse_process_line_handles_whitespace_padded_ps_output() {
        let parsed =
            parse_process_line("  4242   1337 codex /usr/bin/codex --sandbox workspace-write")
                .expect("process");
        assert_eq!(parsed.pid, 4242);
        assert_eq!(parsed.ppid, 1337);
        assert_eq!(parsed.command, "codex");
        assert_eq!(parsed.args, "/usr/bin/codex --sandbox workspace-write");
    }

    #[test]
    fn process_tree_matches_provider_detects_wrapped_omc_processes() {
        let processes = vec![ProcessInfo {
            pid: 42,
            ppid: 1,
            command: "python3".into(),
            args: "/tmp/wrapper.py -- launch claude-code --resume".into(),
        }];

        assert!(process_tree_matches_provider(&processes, ProviderKind::Omc));
        assert!(!process_tree_matches_provider(
            &processes,
            ProviderKind::Omx
        ));
    }

    #[tokio::test]
    async fn deliver_retries_enter_until_prompt_submit_marker_changes() {
        let tempdir = tempdir().expect("tempdir");
        let workdir = tempdir.path().join("repo");
        fs::create_dir_all(workdir.join(".codex")).expect("create codex dir");
        fs::create_dir_all(workdir.join(".clawhip/hooks")).expect("create hook dir");
        fs::write(
            workdir.join(".codex/hooks.json"),
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"node ./.clawhip/hooks/native-hook.mjs --provider codex"}]}]}}"#,
        )
        .expect("write codex hooks");
        fs::write(
            workdir.join(".clawhip/hooks/native-hook.mjs"),
            "function maybeWritePromptSubmitState() { return '.clawhip/state/prompt-submit.json'; }\n",
        )
        .expect("write native hook");

        let state_dir = tempdir.path().join("fake-tmux");
        fs::create_dir_all(&state_dir).expect("create fake state dir");
        let marker_path = workdir.join(PROMPT_SUBMIT_MARKER);
        let marker_dir = marker_path.parent().expect("marker dir");
        let tmux_path = tempdir.path().join("fake-tmux.sh");
        fs::write(
            &tmux_path,
            format!(
                "#!/usr/bin/env bash\nset -euo pipefail\nSTATE_DIR={state}\nMARKER={marker}\nMARKER_DIR={marker_dir}\nCMD=\"$1\"\nshift\ncase \"$CMD\" in\n  display-message)\n    while [ $# -gt 0 ]; do\n      case \"$1\" in\n        -p) shift ;;\n        -t) shift 2 ;;\n        *) FORMAT=\"$1\"; shift ;;\n      esac\n    done\n    printf 'issue-184\\t%%1\\t999999\\tcodex\\t%s\\n' {cwd}\n    ;;\n  capture-pane)\n    cat \"$STATE_DIR/capture.txt\" 2>/dev/null || true\n    ;;\n  send-keys)\n    LITERAL=0\n    while [ $# -gt 0 ]; do\n      case \"$1\" in\n        -t) shift 2 ;;\n        -l) LITERAL=1; shift; TEXT=\"$1\"; shift ;;\n        *) KEY=\"$1\"; shift ;;\n      esac\n    done\n    if [ \"$LITERAL\" -eq 1 ]; then\n      printf '%s\\n' \"$TEXT\" > \"$STATE_DIR/prompt.txt\"\n      printf '%s\\n' \"$TEXT\" > \"$STATE_DIR/capture.txt\"\n    else\n      COUNT=$(cat \"$STATE_DIR/enters.txt\" 2>/dev/null || echo 0)\n      COUNT=$((COUNT + 1))\n      printf '%s' \"$COUNT\" > \"$STATE_DIR/enters.txt\"\n      if [ \"$COUNT\" -ge 2 ]; then\n        mkdir -p \"$MARKER_DIR\"\n        printf '{{\"attempt\":%s}}\\n' \"$COUNT\" > \"$MARKER\"\n        printf 'submitted\\n' > \"$STATE_DIR/capture.txt\"\n      else\n        cat \"$STATE_DIR/prompt.txt\" > \"$STATE_DIR/capture.txt\"\n      fi\n    fi\n    ;;\n  *)\n    echo \"unsupported fake tmux command: $CMD\" >&2\n    exit 1\n    ;;\nesac\n",
                state = shell_escape_path(&state_dir),
                marker = shell_escape_path(&marker_path),
                marker_dir = shell_escape_path(marker_dir),
                cwd = shell_escape_path(&workdir),
            ),
        )
        .expect("write fake tmux");
        let mut perms = fs::metadata(&tmux_path).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tmux_path, perms).expect("chmod fake tmux");

        let previous_tmux = std::env::var_os("CLAWHIP_TMUX_BIN");
        unsafe {
            std::env::set_var("CLAWHIP_TMUX_BIN", &tmux_path);
        }

        let config = PromptDeliverConfig {
            session: "issue-184".into(),
            prompt: "Ship the fix".into(),
            max_enters: 3,
            tui_timeout: Duration::from_millis(50),
            poll_interval: Duration::from_millis(10),
            verify_delay: Duration::from_millis(10),
        };

        let result = deliver(&config).await.expect("deliver");
        assert!(result.delivered);
        assert_eq!(result.enter_attempts, 2);
        assert_eq!(result.provider, ProviderKind::Omx);
        assert!(marker_path.is_file());

        if let Some(previous) = previous_tmux {
            unsafe {
                std::env::set_var("CLAWHIP_TMUX_BIN", previous);
            }
        } else {
            unsafe {
                std::env::remove_var("CLAWHIP_TMUX_BIN");
            }
        }
    }

    fn shell_escape_path(path: &Path) -> String {
        let value = path.display().to_string();
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}
