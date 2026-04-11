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
const DEFAULT_PROGRESS_TIMEOUT: Duration = Duration::from_secs(4);
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
    pub progress_timeout: Duration,
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
            progress_timeout: DEFAULT_PROGRESS_TIMEOUT,
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
            progress_timeout: DEFAULT_PROGRESS_TIMEOUT,
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
    install_scope: HookDetectionScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookDetectionScope {
    Global,
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
    let mut pane = resolve_target_pane(&config.session).await?;
    let hook_setup = detect_hook_setup(&pane.cwd)?;
    let provider = ensure_provider_ready(&mut pane, &hook_setup, config).await?;

    wait_for_tui_ready(&pane.pane_id, config.tui_timeout, config.poll_interval).await?;

    let baseline_marker = read_marker_hash(&hook_setup.marker_path)?;
    if hook_setup.install_scope == HookDetectionScope::Global {
        ensure_global_workdir_marker(&hook_setup)?;
    }
    send_literal_keys(&pane.pane_id, &config.prompt).await?;
    let baseline_pane = capture_pane_hash(&pane.pane_id).await.ok();

    for attempt in 1..=config.max_enters.max(1) {
        send_key(&pane.pane_id, "Enter").await?;
        sleep(config.verify_delay).await;

        if marker_changed(&hook_setup.marker_path, baseline_marker)? {
            wait_for_progress_signal(
                &pane.pane_id,
                baseline_pane,
                config.progress_timeout,
                config.poll_interval,
            )
            .await?;
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
    let repo_root = infer_repo_root(cwd).ok_or_else(|| {
        format!(
            "refusing delivery: '{}' is not inside a repo/workdir with git-derived routing metadata; prompt delivery requires a repo/workdir-backed session and a global hook install",
            cwd.display()
        )
    })?;

    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Some(setup) = hook_setup_at(&home, HookDetectionScope::Global)
    {
        return Ok(HookSetup {
            workdir: repo_root.clone(),
            marker_path: repo_root.join(PROMPT_SUBMIT_MARKER),
            supported_providers: setup.supported_providers,
            sources: setup.sources,
            install_scope: HookDetectionScope::Global,
        });
    }

    Err(format!(
        "refusing delivery: '{}' is not inside a repo/workdir with prompt-submit-aware global hook setup; no global ~/.codex / ~/.claude clawhip hook install was detected",
        cwd.display()
    )
    .into())
}

fn hook_setup_at(root: &Path, install_scope: HookDetectionScope) -> Option<HookSetup> {
    let mut providers = Vec::new();
    let mut sources = Vec::new();
    let has_native_script = has_native_prompt_submit_hook_script(root);

    if has_claude_prompt_submit_hook(root) && has_native_script {
        providers.push(ProviderKind::Omc);
        sources.push(".claude/settings.json + .clawhip/hooks/native-hook.mjs");
    }
    if has_codex_prompt_submit_hook(root) && has_native_script {
        providers.push(ProviderKind::Omx);
        sources.push(".codex/hooks.json + .clawhip/hooks/native-hook.mjs");
    }
    if providers.is_empty() {
        return None;
    }

    Some(HookSetup {
        workdir: root.to_path_buf(),
        marker_path: root.join(PROMPT_SUBMIT_MARKER),
        supported_providers: providers,
        sources,
        install_scope,
    })
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

fn command_mentions_clawhip(command: &str) -> bool {
    let normalized = command.trim().to_ascii_lowercase();
    normalized.contains("clawhip native hook")
        || normalized.contains(".clawhip/hooks/native-hook.mjs")
        || normalized.contains("native-hook.mjs")
}

fn infer_git_workdir(cwd: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["-C", &cwd.to_string_lossy(), "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let trimmed = root.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = PathBuf::from(trimmed);
    Some(path.canonicalize().unwrap_or(path))
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

async fn ensure_provider_ready(
    pane: &mut PaneTarget,
    hook_setup: &HookSetup,
    config: &PromptDeliverConfig,
) -> Result<ProviderKind> {
    if let Ok(provider) = detect_active_provider(pane, hook_setup).await {
        return Ok(provider);
    }

    let provider = infer_provider_from_hook_setup(hook_setup)?;
    try_resume_provider(pane, provider, config).await?;
    *pane = resolve_target_pane(&pane.session).await?;
    detect_active_provider(pane, hook_setup).await
}

fn infer_provider_from_hook_setup(hook_setup: &HookSetup) -> Result<ProviderKind> {
    if hook_setup.supported_providers.len() == 1 {
        Ok(hook_setup.supported_providers[0])
    } else {
        Err("refusing delivery: multiple providers configured for this workdir; cannot infer which one to resume safely".into())
    }
}

async fn try_resume_provider(
    pane: &PaneTarget,
    provider: ProviderKind,
    config: &PromptDeliverConfig,
) -> Result<()> {
    let resume = build_resume_command(pane, provider).await?;
    send_literal_keys(&pane.pane_id, &resume).await?;
    send_key(&pane.pane_id, "Enter").await?;
    wait_for_provider_prompt(
        &pane.pane_id,
        provider,
        config.tui_timeout,
        config.poll_interval,
    )
    .await
}

async fn build_resume_command(pane: &PaneTarget, provider: ProviderKind) -> Result<String> {
    let process_tree = read_process_tree(pane.pane_pid).await.unwrap_or_default();
    if let Some(resume) = process_tree
        .iter()
        .find_map(|process| extract_resume_command(process, provider))
    {
        return Ok(resume);
    }

    let binary = match provider {
        ProviderKind::Omc => "omc",
        ProviderKind::Omx => "omx",
    };
    Ok(binary.into())
}

fn extract_resume_command(process: &ProcessInfo, provider: ProviderKind) -> Option<String> {
    let args = process.args.trim();
    if args.is_empty() || !provider_matches_command(provider, &process.args.to_ascii_lowercase()) {
        return None;
    }

    if let Some(idx) = args.find(" resume ") {
        return Some(args[idx + 1..].trim().to_string());
    }
    if args.starts_with("resume ") {
        return Some(args.to_string());
    }
    None
}

async fn wait_for_provider_prompt(
    target: &str,
    provider: ProviderKind,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Ok(());
        }

        if pane_looks_like_provider(target, provider).await {
            return Ok(());
        }

        sleep(poll_interval).await;
    }
}

async fn pane_looks_like_provider(target: &str, provider: ProviderKind) -> bool {
    let output = Command::new(tmux_bin())
        .arg("capture-pane")
        .arg("-p")
        .arg("-t")
        .arg(target)
        .arg("-S")
        .arg("-40")
        .output()
        .await;
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    match provider {
        ProviderKind::Omc => text.contains("claude") || text.contains("omc"),
        ProviderKind::Omx => {
            text.contains("openai codex") || text.contains("gpt-5.4") || text.contains("omx")
        }
    }
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

fn ensure_global_workdir_marker(hook_setup: &HookSetup) -> Result<()> {
    if hook_setup.install_scope != HookDetectionScope::Global {
        return Ok(());
    }
    if let Some(parent) = hook_setup.marker_path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn infer_repo_root(cwd: &Path) -> Option<PathBuf> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let cwd_str = cwd.to_string_lossy().into_owned();

    if let Some(common_dir) = std::process::Command::new("git")
        .args([
            "-C",
            &cwd_str,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.trim().to_string())
        .filter(|output| !output.is_empty())
        && let Some(repo_root) = Path::new(&common_dir).parent()
    {
        return Some(
            repo_root
                .canonicalize()
                .unwrap_or_else(|_| repo_root.to_path_buf()),
        );
    }

    let output = std::process::Command::new("git")
        .args(["-C", &cwd_str, "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let repo_root = String::from_utf8(output.stdout).ok()?;
    let repo_root = repo_root.trim();
    if repo_root.is_empty() {
        None
    } else {
        Some(
            PathBuf::from(repo_root)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(repo_root)),
        )
    }
}

fn marker_changed(path: &Path, baseline: Option<u64>) -> Result<bool> {
    let current = read_marker_hash(path)?;
    Ok(match (baseline, current) {
        (None, Some(_)) => true,
        (Some(before), Some(after)) => before != after,
        _ => false,
    })
}

async fn wait_for_progress_signal(
    target: &str,
    baseline_hash: Option<u64>,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        if let Some(before) = baseline_hash {
            if let Ok(current) = capture_pane_hash(target).await
                && current != before
            {
                return Ok(());
            }
        } else if capture_pane_hash(target).await.is_ok() {
            return Ok(());
        }

        if tokio::time::Instant::now() >= deadline {
            let last_line = capture_last_line(target).await.unwrap_or_default();
            return Err(format!(
                "prompt submit recorded, but no bounded progress signal appeared within {:?} (last line: {})",
                timeout,
                format_last_line(&last_line),
            )
            .into());
        }

        sleep(poll_interval).await;
    }
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

async fn capture_pane_hash(target: &str) -> Result<u64> {
    let output = Command::new(tmux_bin())
        .arg("capture-pane")
        .arg("-p")
        .arg("-t")
        .arg(target)
        .arg("-S")
        .arg("-200")
        .output()
        .await?;
    if !output.status.success() {
        return Err(tmux_stderr(&output.stderr).into());
    }
    Ok(content_hash(&String::from_utf8(output.stdout)?))
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
    use serial_test::serial;
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
        assert_eq!(config.progress_timeout, Duration::from_secs(4));
    }

    #[test]
    #[serial]
    fn detect_hook_setup_uses_global_hooks_for_repo_workdir() {
        let tempdir = tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        let home = tempdir.path().join("home");
        let nested = repo.join("src/bin");
        fs::create_dir_all(&nested).expect("create nested dir");
        let previous_home = std::env::var_os("HOME");
        git_init_repo(&repo);
        fs::create_dir_all(home.join(".codex")).expect("create codex dir");
        fs::create_dir_all(home.join(".clawhip/hooks")).expect("create hook dir");
        fs::write(
            home.join(".codex/hooks.json"),
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"node ~/.clawhip/hooks/native-hook.mjs --provider codex"}]}]}}"#,
        )
        .expect("write codex hooks");
        fs::write(
            home.join(".clawhip/hooks/native-hook.mjs"),
            "function maybeWritePromptSubmitState() { return '.clawhip/state/prompt-submit.json'; }\n",
        )
        .expect("write native hook");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let setup = detect_hook_setup(&nested).expect("hook setup");
        assert_eq!(setup.workdir, repo.canonicalize().expect("canonical repo"));
        assert_eq!(setup.supported_providers, vec![ProviderKind::Omx]);
        assert_eq!(setup.install_scope, HookDetectionScope::Global);

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
    #[serial]
    fn detect_hook_setup_recognizes_global_omc_user_prompt_submit_hooks() {
        let tempdir = tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        let home = tempdir.path().join("home");
        let previous_home = std::env::var_os("HOME");
        git_init_repo(&repo);
        fs::create_dir_all(home.join(".claude")).expect("create claude dir");
        fs::create_dir_all(home.join(".clawhip/hooks")).expect("create hook dir");
        fs::write(
            home.join(".claude/settings.json"),
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"node ~/.clawhip/hooks/native-hook.mjs --provider claude-code"}]}]}}"#,
        )
        .expect("write settings");
        fs::write(
            home.join(".clawhip/hooks/native-hook.mjs"),
            "function maybeWritePromptSubmitState() { return '.clawhip/state/prompt-submit.json'; }\n",
        )
        .expect("write native hook");
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let setup = detect_hook_setup(&repo).expect("hook setup");
        assert_eq!(setup.supported_providers, vec![ProviderKind::Omc]);
        assert_eq!(setup.install_scope, HookDetectionScope::Global);

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
    #[serial]
    fn detect_hook_setup_rejects_old_omx_bridge_without_prompt_submit_support() {
        let tempdir = tempdir().expect("tempdir");
        let repo = tempdir.path().join("repo");
        let fake_home = tempdir.path().join("home");
        fs::create_dir_all(&fake_home).expect("create fake home");
        git_init_repo(&repo);
        let previous_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", &fake_home);
        }

        let error = detect_hook_setup(&repo).expect_err("old bridge should be rejected");
        assert!(error.to_string().contains("global hook setup"));

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

    #[test]
    fn extract_resume_command_prefers_resume_tail_for_omx() {
        let process = ProcessInfo {
            pid: 42,
            ppid: 1,
            command: "codex".into(),
            args: "codex resume 019d7ac0-c822-7ab0-9edf-d76ec22288da".into(),
        };

        assert_eq!(
            extract_resume_command(&process, ProviderKind::Omx).as_deref(),
            Some("resume 019d7ac0-c822-7ab0-9edf-d76ec22288da")
        );
    }

    #[test]
    fn infer_provider_from_hook_setup_requires_single_provider() {
        let setup = HookSetup {
            workdir: PathBuf::from("/tmp/repo"),
            marker_path: PathBuf::from("/tmp/repo/.clawhip/state/prompt-submit.json"),
            supported_providers: vec![ProviderKind::Omx],
            sources: vec![".codex/hooks.json + .clawhip/hooks/native-hook.mjs"],
            install_scope: HookDetectionScope::Global,
        };
        assert_eq!(
            infer_provider_from_hook_setup(&setup).unwrap(),
            ProviderKind::Omx
        );

        let dual = HookSetup {
            supported_providers: vec![ProviderKind::Omc, ProviderKind::Omx],
            ..setup
        };
        assert!(infer_provider_from_hook_setup(&dual).is_err());
    }

    #[tokio::test]
    #[serial]
    async fn deliver_fails_when_prompt_submit_records_but_pane_shows_no_progress() {
        let tempdir = tempdir().expect("tempdir");
        let workdir = tempdir.path().join("repo");
        git_init_repo(&workdir);
        let fake_home = tempdir.path().join("home");
        fs::create_dir_all(fake_home.join(".codex")).expect("create codex dir");
        fs::create_dir_all(fake_home.join(".clawhip/hooks")).expect("create hook dir");
        fs::write(
            fake_home.join(".codex/hooks.json"),
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"node ~/.clawhip/hooks/native-hook.mjs --provider codex"}]}]}}"#,
        )
        .expect("write codex hooks");
        fs::write(
            fake_home.join(".clawhip/hooks/native-hook.mjs"),
            "function maybeWritePromptSubmitState() { return '.clawhip/state/prompt-submit.json'; }\n",
        )
        .expect("write native hook");

        let state_dir = tempdir.path().join("fake-tmux-idle");
        fs::create_dir_all(&state_dir).expect("create fake state dir");
        let marker_path = workdir.join(PROMPT_SUBMIT_MARKER);
        let marker_dir = marker_path.parent().expect("marker dir");
        let tmux_path = tempdir.path().join("fake-tmux-idle.sh");
        fs::write(
            &tmux_path,
            format!(
                "#!/usr/bin/env bash\nset -euo pipefail\nSTATE_DIR={state}\nMARKER={marker}\nMARKER_DIR={marker_dir}\nCMD=\"$1\"\nshift\ncase \"$CMD\" in\n  display-message)\n    while [ $# -gt 0 ]; do\n      case \"$1\" in\n        -p) shift ;;\n        -t) shift 2 ;;\n        *) FORMAT=\"$1\"; shift ;;\n      esac\n    done\n    printf 'issue-206\\t%%1\\t999999\\tcodex\\t%s\\n' {cwd}\n    ;;\n  capture-pane)\n    cat \"$STATE_DIR/capture.txt\" 2>/dev/null || true\n    ;;\n  send-keys)\n    LITERAL=0\n    while [ $# -gt 0 ]; do\n      case \"$1\" in\n        -t) shift 2 ;;\n        -l) LITERAL=1; shift; TEXT=\"$1\"; shift ;;\n        *) KEY=\"$1\"; shift ;;\n      esac\n    done\n    if [ \"$LITERAL\" -eq 1 ]; then\n      printf '%s\\n' \"$TEXT\" > \"$STATE_DIR/prompt.txt\"\n      printf '%s\\n' 'idle-prompt-surface' > \"$STATE_DIR/capture.txt\"\n    else\n      mkdir -p \"$MARKER_DIR\"\n      printf '{{\"attempt\":1}}\\n' > \"$MARKER\"\n      printf '%s\\n' 'idle-prompt-surface' > \"$STATE_DIR/capture.txt\"\n    fi\n    ;;\n  *)\n    echo \"unsupported fake tmux command: $CMD\" >&2\n    exit 1\n    ;;\nesac\n",
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

        let previous_home = std::env::var_os("HOME");
        let previous_tmux = std::env::var_os("CLAWHIP_TMUX_BIN");
        unsafe {
            std::env::set_var("HOME", &fake_home);
            std::env::set_var("CLAWHIP_TMUX_BIN", &tmux_path);
        }

        let config = PromptDeliverConfig {
            session: "issue-206".into(),
            prompt: "Ship the fix".into(),
            max_enters: 1,
            tui_timeout: Duration::from_millis(50),
            poll_interval: Duration::from_millis(10),
            verify_delay: Duration::from_millis(10),
            progress_timeout: Duration::from_millis(30),
        };

        let error = deliver(&config)
            .await
            .expect_err("idle pane should fail fast");
        assert!(error.to_string().contains("no bounded progress signal"));

        if let Some(previous) = previous_tmux {
            unsafe {
                std::env::set_var("CLAWHIP_TMUX_BIN", previous);
            }
        } else {
            unsafe {
                std::env::remove_var("CLAWHIP_TMUX_BIN");
            }
        }
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

    #[tokio::test]
    #[serial]
    async fn deliver_retries_enter_until_prompt_submit_marker_changes() {
        let tempdir = tempdir().expect("tempdir");
        let workdir = tempdir.path().join("repo");
        git_init_repo(&workdir);
        let fake_home = tempdir.path().join("home");
        fs::create_dir_all(fake_home.join(".codex")).expect("create codex dir");
        fs::create_dir_all(fake_home.join(".clawhip/hooks")).expect("create hook dir");
        fs::write(
            fake_home.join(".codex/hooks.json"),
            r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"node ~/.clawhip/hooks/native-hook.mjs --provider codex"}]}]}}"#,
        )
        .expect("write codex hooks");
        fs::write(
            fake_home.join(".clawhip/hooks/native-hook.mjs"),
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

        let previous_home = std::env::var_os("HOME");
        let previous_tmux = std::env::var_os("CLAWHIP_TMUX_BIN");
        unsafe {
            std::env::set_var("HOME", &fake_home);
            std::env::set_var("CLAWHIP_TMUX_BIN", &tmux_path);
        }

        let config = PromptDeliverConfig {
            session: "issue-184".into(),
            prompt: "Ship the fix".into(),
            max_enters: 3,
            tui_timeout: Duration::from_millis(50),
            poll_interval: Duration::from_millis(10),
            verify_delay: Duration::from_millis(10),
            progress_timeout: Duration::from_millis(40),
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

    fn shell_escape_path(path: &Path) -> String {
        let value = path.display().to_string();
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    fn git_init_repo(repo: &Path) {
        fs::create_dir_all(repo).expect("create repo");
        let output = std::process::Command::new("git")
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
