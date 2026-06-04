use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::events::IncomingEvent;

const GAJAE_ENV: &str = "GAJAE_BIN";
const GAJAE_PATH_NAME: &str = "gajae";
const PROFILE_INSTALL_ARGS: &[&str] = &["clawhip", "profile", "install"];
const SUMMARY_LIMIT: usize = 240;
const RECEIPT_STDIN_LIMIT: usize = 1_048_576;
const DEFAULT_ROUTES_PATH: &str = ".clawhip/gajae.routes.yml";
const DEFAULT_RUNTIME_DIR: &str = ".gajae/runtime";
const PROFILE_FILE_NAME: &str = "clawhip-profile.yml";
const MAX_PROFILE_BYTES: usize = 256 * 1024;
const SUPPORTED_EVENTS: &[&str] = &[
    "github.issue-opened",
    "github.issue-commented",
    "github.issue-closed",
    "github.pr-status-changed",
    "github.release-published",
    "github.release-prereleased",
    "github.release-edited",
    "github.ci-started",
    "github.ci-failed",
    "github.ci-passed",
    "github.ci-cancelled",
    "session.started",
    "session.blocked",
    "session.finished",
    "session.failed",
    "session.retry-needed",
    "session.pr-created",
    "session.test-started",
    "session.test-finished",
    "session.test-failed",
    "session.handoff-needed",
    "session.prompt-submitted",
    "session.prompt-delivered",
    "session.prompt-delivery-failed",
    "session.stopped",
    "tool.pre",
    "tool.post",
    "tmux.keyword",
    "tmux.stale",
];
const HANDLER_ARGS_PREFIX: &[&str] = &["clawhip", "handler"];
const ALLOWED_HANDLER_SUBCOMMANDS: &[&str] = &["handle-event", "route-action", "summarize-event"];
const DIAGNOSTIC_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerAction {
    pub subcommand: String,
    pub args: Vec<String>,
    pub requires_approval: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandlerLimits {
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerOutcome {
    Completed(Value),
    ApprovalRequired(Value),
    Failed {
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    TimedOut,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct BoundedOutput {
    bytes: Vec<u8>,
    capped: bool,
}

pub async fn run_handler(
    action: &HandlerAction,
    event: &Value,
    limits: HandlerLimits,
) -> Result<HandlerOutcome> {
    validate_handler_action(action)?;
    let bin = discover_gajae_with(|name| std::env::var_os(name));
    run_handler_with_bin(&bin, action, event, limits).await
}

#[allow(clippy::too_many_lines)]
async fn run_handler_with_bin(
    bin: &Path,
    action: &HandlerAction,
    event: &Value,
    limits: HandlerLimits,
) -> Result<HandlerOutcome> {
    if limits.timeout.is_zero() {
        bail!("GAJAE handler timeout must be nonzero");
    }
    if limits.max_output_bytes == 0 {
        bail!("GAJAE handler max output must be nonzero");
    }

    let mut command = tokio::process::Command::new(bin);
    command
        .args(handler_args(action))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = spawn_handler_command(&mut command, bin).await?;
    if let Some(mut stdin) = child.stdin.take() {
        let event_json = serde_json::to_vec(event)?;
        if let Err(error) = stdin.write_all(&event_json).await
            && error.kind() != io::ErrorKind::BrokenPipe
        {
            return Err(error.into());
        }
        if let Err(error) = stdin.shutdown().await
            && error.kind() != io::ErrorKind::BrokenPipe
        {
            return Err(error.into());
        }
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("GAJAE handler stdout pipe unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("GAJAE handler stderr pipe unavailable"))?;
    let mut stdout_task = tokio::spawn(read_bounded_output(stdout, limits.max_output_bytes));
    let mut stderr_task = tokio::spawn(read_bounded_output(stderr, limits.max_output_bytes));
    let mut stdout_output: Option<BoundedOutput> = None;
    let mut stderr_output: Option<BoundedOutput> = None;
    let timeout = tokio::time::sleep(limits.timeout);
    tokio::pin!(timeout);

    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            stdout_result = &mut stdout_task, if stdout_output.is_none() => {
                let output = stdout_result??;
                let capped = output.capped;
                stdout_output = Some(output);
                if capped {
                    let _ = child.start_kill();
                    break child.wait().await?;
                }
            }
            stderr_result = &mut stderr_task, if stderr_output.is_none() => {
                let output = stderr_result??;
                let capped = output.capped;
                stderr_output = Some(output);
                if capped {
                    let _ = child.start_kill();
                    break child.wait().await?;
                }
            }
            _ = &mut timeout => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                stdout_task.abort();
                stderr_task.abort();
                return Ok(HandlerOutcome::TimedOut);
            }
        }
    };

    let stdout = finish_bounded_task(stdout_output, stdout_task).await?;
    let stderr = finish_bounded_task(stderr_output, stderr_task).await?;

    let diagnostic_limit = limits.max_output_bytes.min(DIAGNOSTIC_BYTES);
    let stdout_diagnostic = bounded_bytes(&stdout.bytes, diagnostic_limit);
    let stderr_diagnostic = bounded_bytes(&stderr.bytes, diagnostic_limit);
    if !status.success() || stdout.capped || stderr.capped {
        return Ok(HandlerOutcome::Failed {
            code: status.code(),
            stdout: diagnostic_text(&stdout_diagnostic),
            stderr: diagnostic_text(&stderr_diagnostic),
        });
    }

    let parsed = if stdout.bytes.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        serde_json::from_slice(&stdout.bytes).unwrap_or_else(|_| {
            json!({
                "summary": diagnostic_text(&stdout_diagnostic),
            })
        })
    };

    if action.requires_approval || output_requests_mutation(&parsed) {
        Ok(HandlerOutcome::ApprovalRequired(parsed))
    } else {
        Ok(HandlerOutcome::Completed(parsed))
    }
}

async fn spawn_handler_command(
    command: &mut tokio::process::Command,
    bin: &Path,
) -> Result<tokio::process::Child> {
    let mut attempts = 0;
    loop {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) if error.raw_os_error() == Some(26) && attempts < 10 => {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to spawn GAJAE handler {}", bin.display()));
            }
        }
    }
}

async fn read_bounded_output<R>(mut reader: R, max: usize) -> io::Result<BoundedOutput>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(max.min(8 * 1024));
    let mut buffer = [0_u8; 8192];

    loop {
        let remaining = max.saturating_sub(bytes.len());
        let read_limit = remaining.saturating_add(1).min(buffer.len());
        if read_limit == 0 {
            return Ok(BoundedOutput {
                bytes,
                capped: true,
            });
        }

        let read = reader.read(&mut buffer[..read_limit]).await?;
        if read == 0 {
            return Ok(BoundedOutput {
                bytes,
                capped: false,
            });
        }
        if read > remaining {
            bytes.extend_from_slice(&buffer[..remaining]);
            return Ok(BoundedOutput {
                bytes,
                capped: true,
            });
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
}

async fn finish_bounded_task(
    output: Option<BoundedOutput>,
    task: tokio::task::JoinHandle<io::Result<BoundedOutput>>,
) -> Result<BoundedOutput> {
    match output {
        Some(output) => Ok(output),
        None => Ok(task.await??),
    }
}

fn validate_handler_action(action: &HandlerAction) -> Result<()> {
    let subcommand = action.subcommand.trim();
    if !ALLOWED_HANDLER_SUBCOMMANDS.contains(&subcommand) {
        bail!("unsupported GAJAE handler subcommand '{subcommand}'");
    }
    if action.args.iter().any(|arg| arg.contains('\0')) {
        bail!("GAJAE handler arguments must not contain NUL bytes");
    }
    Ok(())
}

fn handler_args(action: &HandlerAction) -> Vec<String> {
    HANDLER_ARGS_PREFIX
        .iter()
        .map(|arg| (*arg).to_string())
        .chain(std::iter::once(action.subcommand.clone()))
        .chain(action.args.clone())
        .collect()
}

fn output_requests_mutation(value: &Value) -> bool {
    value
        .get("mutation_requested")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || value
            .get("requires_approval")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || value.get("mutation").is_some()
        || value.get("mutations").is_some()
}

fn bounded_bytes(bytes: &[u8], max: usize) -> Vec<u8> {
    bytes.iter().copied().take(max).collect()
}

fn diagnostic_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.chars()
        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
        .collect::<String>()
        .lines()
        .take(8)
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, Copy)]
pub enum GajaeCommand {
    Status,
}

#[derive(Debug, Clone, Default)]
pub struct ProfileInspectOptions {
    pub file: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ProfileExplainOptions {
    pub file: Option<PathBuf>,
    pub event: String,
    pub repo: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ProfileApplyOptions {
    pub file: Option<PathBuf>,
    pub dry_run: bool,
    pub approve: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GajaeRouteProfile {
    source: PathBuf,
    name: Option<String>,
    routes_file: Option<PathBuf>,
    routes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteValidation {
    unknown_events: Vec<String>,
    unsupported_commands: Vec<(String, String)>,
}

impl RouteValidation {
    fn is_clean(&self) -> bool {
        self.unknown_events.is_empty() && self.unsupported_commands.is_empty()
    }
}

pub fn run_profile_inspect(options: ProfileInspectOptions) -> Result<()> {
    let profile = load_profile(options.file.as_deref())?;
    let validation = validate_profile(&profile);
    println!(
        "GAJAE clawhip profile: {}",
        profile.name.as_deref().unwrap_or("unknown")
    );
    println!("source: {}", profile.source.display());
    println!("routes: {}", profile.routes.len());
    for (event, command) in &profile.routes {
        println!("- {event}:");
        println!("  command: {}", summarize_route_command(command, event));
    }
    print_validation(&validation);
    if validation.is_clean() {
        Ok(())
    } else {
        bail!("GAJAE profile validation failed")
    }
}

pub fn run_profile_explain(options: ProfileExplainOptions) -> Result<()> {
    let profile = load_profile(options.file.as_deref())?;
    let validation = validate_profile(&profile);
    if !SUPPORTED_EVENTS.contains(&options.event.as_str()) {
        bail!("unknown GAJAE event name: {}", options.event);
    }
    if !validation.is_clean() {
        print_validation(&validation);
        bail!("GAJAE profile validation failed")
    }

    println!("event: {}", options.event);
    if let Some(repo) = options.repo.as_deref() {
        println!("repo: {repo}");
    }
    match profile.routes.get(&options.event) {
        Some(command) => {
            println!("match: yes");
            println!(
                "route command: {}",
                summarize_route_command(command, &options.event)
            );
            println!("action: explain-only; command not executed");
            Ok(())
        }
        None => {
            println!("match: no");
            println!("action: explain-only; no command executed");
            Ok(())
        }
    }
}

pub fn run_profile_apply(options: ProfileApplyOptions) -> Result<()> {
    if !options.dry_run {
        if options.approve {
            bail!(
                "live GAJAE profile apply is approval-gated and not implemented by this safe inspector"
            );
        }
        bail!("refusing live GAJAE profile apply; rerun with --dry-run to inspect safely");
    }

    let profile = load_profile(options.file.as_deref())?;
    let validation = validate_profile(&profile);
    println!("GAJAE profile apply dry-run");
    println!("source: {}", profile.source.display());
    println!("would inspect routes: {}", profile.routes.len());
    println!("would execute commands: 0");
    print_validation(&validation);
    if validation.is_clean() {
        println!("dry-run result: supported; live apply still requires a separate approval gate");
        Ok(())
    } else {
        bail!("GAJAE profile dry-run detected unsupported route entries")
    }
}

fn load_profile(explicit_file: Option<&Path>) -> Result<GajaeRouteProfile> {
    load_profile_from_cwd(
        explicit_file,
        &std::env::current_dir().context("failed to inspect current directory")?,
    )
}

fn load_profile_from_cwd(explicit_file: Option<&Path>, cwd: &Path) -> Result<GajaeRouteProfile> {
    let source = match explicit_file {
        Some(path) => path.to_path_buf(),
        None => discover_profile_path()?,
    };
    let contents = read_bounded_profile(&source, "GAJAE profile route file")?;
    let profile = parse_profile(&contents, source)?;
    if !profile.routes.is_empty() {
        return Ok(profile);
    }

    let Some(routes_file) = profile.routes_file.as_deref() else {
        return Ok(profile);
    };
    let routes_source = resolve_routes_file(routes_file, profile.source.as_path(), cwd)?;
    let routes_contents = read_bounded_profile(&routes_source, "referenced GAJAE routes file")?;
    let mut routes_profile = parse_profile(&routes_contents, routes_source)?;
    if routes_profile.name.is_none() {
        routes_profile.name = profile.name;
    }
    Ok(routes_profile)
}

fn read_bounded_profile(path: &Path, description: &str) -> Result<String> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to read {description} {}", path.display()))?;
    let mut bytes = Vec::new();
    file.take((MAX_PROFILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {description} {}", path.display()))?;
    if bytes.len() > MAX_PROFILE_BYTES {
        bail!("{description} exceeds maximum size of {MAX_PROFILE_BYTES} bytes");
    }
    String::from_utf8(bytes)
        .with_context(|| format!("{description} {} is not valid UTF-8", path.display()))
}

fn resolve_routes_file(routes_file: &Path, source: &Path, cwd: &Path) -> Result<PathBuf> {
    let cwd = cwd
        .canonicalize()
        .context("failed to inspect current directory")?;
    let mut candidates = Vec::new();
    if routes_file.is_absolute() {
        candidates.push(routes_file.to_path_buf());
    } else {
        candidates.push(cwd.join(routes_file));
        if let Some(parent) = source.parent() {
            let candidate = parent.join(routes_file);
            if !candidates.iter().any(|path| path == &candidate) {
                candidates.push(candidate);
            }
        }
    }

    let mut escaped = false;
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        let canonical = candidate.canonicalize().with_context(|| {
            format!(
                "failed to inspect referenced GAJAE routes file {}",
                candidate.display()
            )
        })?;
        if canonical.starts_with(&cwd) {
            return Ok(candidate);
        }
        escaped = true;
    }

    if escaped {
        bail!("referenced GAJAE routes file is outside the current workspace");
    }
    bail!("referenced GAJAE routes file not found")
}

fn discover_profile_path() -> Result<PathBuf> {
    let routes = PathBuf::from(DEFAULT_ROUTES_PATH);
    if routes.is_file() {
        return Ok(routes);
    }

    let runtime_dir = Path::new(DEFAULT_RUNTIME_DIR);
    if runtime_dir.is_dir() {
        let mut candidates = Vec::new();
        for entry in
            fs::read_dir(runtime_dir).context("failed to inspect GAJAE runtime directory")?
        {
            let entry = entry.context("failed to inspect GAJAE runtime entry")?;
            let candidate = entry.path().join(PROFILE_FILE_NAME);
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
        candidates.sort();
        if let Some(candidate) = candidates.into_iter().next() {
            return Ok(candidate);
        }
    }

    bail!(
        "GAJAE clawhip profile not found; expected {DEFAULT_ROUTES_PATH} or {DEFAULT_RUNTIME_DIR}/*/{PROFILE_FILE_NAME}"
    )
}

fn parse_profile(contents: &str, source: PathBuf) -> Result<GajaeRouteProfile> {
    let mut name = None;
    let mut routes = BTreeMap::new();
    let mut top_level = None::<String>;
    let mut in_routes = false;
    let mut route_event = None::<String>;
    let mut route_missing_command = None::<(String, usize)>;
    let mut routes_file = None::<PathBuf>;
    let mut parent_stack: Vec<(usize, String)> = Vec::new();

    for (index, raw_line) in contents.lines().enumerate() {
        let line_number = index + 1;
        let without_comment = raw_line
            .split_once('#')
            .map_or(raw_line, |(before, _)| before);
        if without_comment.trim().is_empty() {
            continue;
        }
        let indent = without_comment.chars().take_while(|ch| *ch == ' ').count();
        if indent != raw_line.len() - raw_line.trim_start_matches(' ').len() || indent % 2 != 0 {
            bail!("invalid GAJAE profile syntax at line {line_number}");
        }
        let trimmed = without_comment.trim();
        if trimmed.starts_with('-') {
            if in_routes && indent >= routes_indent(top_level.as_deref()) {
                bail!("unsupported GAJAE list-style route entry at line {line_number}");
            }
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            bail!("invalid GAJAE profile syntax at line {line_number}");
        };
        let key = key.trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            bail!("invalid GAJAE profile key at line {line_number}");
        }
        let value = clean_scalar(value.trim());

        while parent_stack
            .last()
            .is_some_and(|(level, _)| *level >= indent)
        {
            parent_stack.pop();
        }
        if value.is_none() {
            parent_stack.push((indent, key.to_string()));
        }
        let parent = parent_stack.last().map(|(_, key)| key.as_str());

        if indent == 0 {
            ensure_route_has_command(&route_missing_command)?;
            top_level = Some(key.to_string());
            in_routes = key == "routes";
            route_event = None;
            route_missing_command = None;
            validate_top_level_key(key, source.as_path(), line_number)?;
            if key == "profile" || key == "name" {
                name = value.clone();
            }
            if key == "routesFile" {
                let value = value.ok_or_else(|| {
                    anyhow::anyhow!("invalid GAJAE routesFile at line {line_number}")
                })?;
                routes_file = Some(PathBuf::from(value));
            }
            continue;
        }

        if matches!(top_level.as_deref(), Some("clawhipProfile")) && indent == 2 {
            ensure_route_has_command(&route_missing_command)?;
            validate_clawhip_profile_key(key, line_number)?;
            in_routes = key == "routes";
            route_event = None;
            route_missing_command = None;
            if key == "name" {
                name = value.clone();
            }
            if key == "routesFile" {
                let value = value.ok_or_else(|| {
                    anyhow::anyhow!("invalid GAJAE routesFile at line {line_number}")
                })?;
                routes_file = Some(PathBuf::from(value));
            }
            continue;
        }

        if in_routes {
            if indent == routes_indent(top_level.as_deref()) {
                ensure_route_has_command(&route_missing_command)?;
                route_event = Some(key.to_string());
                if let Some(command) = value {
                    routes.insert(key.to_string(), command);
                    route_missing_command = None;
                } else {
                    route_missing_command = Some((key.to_string(), line_number));
                }
                continue;
            }
            if key == "command" {
                let event = route_event.clone().ok_or_else(|| {
                    anyhow::anyhow!("invalid GAJAE profile route command at line {line_number}")
                })?;
                let command = value.ok_or_else(|| {
                    anyhow::anyhow!("invalid GAJAE profile route command at line {line_number}")
                })?;
                route_missing_command = None;
                routes.insert(event, command);
                continue;
            }
            bail!("unsupported GAJAE profile route key at line {line_number}");
        }

        if matches!(parent, Some("safety" | "followUp" | "gajae")) {
            continue;
        }
    }
    ensure_route_has_command(&route_missing_command)?;

    if routes.is_empty() && routes_file.is_none() {
        bail!("GAJAE profile contains no routes");
    }

    Ok(GajaeRouteProfile {
        source,
        name,
        routes_file,
        routes,
    })
}

fn ensure_route_has_command(route_missing_command: &Option<(String, usize)>) -> Result<()> {
    if let Some((event, line_number)) = route_missing_command {
        bail!("GAJAE profile route `{event}` missing command at line {line_number}");
    }
    Ok(())
}

fn clean_scalar(value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let cleaned = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value);
    Some(cleaned.to_string())
}

fn routes_indent(top_level: Option<&str>) -> usize {
    if matches!(top_level, Some("clawhipProfile")) {
        4
    } else {
        2
    }
}

fn validate_top_level_key(key: &str, source: &Path, line_number: usize) -> Result<()> {
    let profile_file = source.file_name().and_then(|name| name.to_str()) == Some(PROFILE_FILE_NAME);
    let allowed = if profile_file {
        matches!(
            key,
            "runtime"
                | "category"
                | "displayName"
                | "gajae"
                | "clawhipProfile"
                | "safety"
                | "operatorConnectionsRequiredLater"
                | "name"
                | "description"
                | "routesFile"
                | "followUp"
                | "routes"
                | "profile"
        )
    } else {
        matches!(key, "profile" | "routes")
    };
    if allowed {
        Ok(())
    } else {
        bail!("unsupported GAJAE profile key `{key}` at line {line_number}")
    }
}

fn validate_clawhip_profile_key(key: &str, line_number: usize) -> Result<()> {
    if matches!(
        key,
        "name" | "description" | "routesFile" | "safety" | "followUp" | "routes"
    ) {
        Ok(())
    } else {
        bail!("unsupported GAJAE clawhipProfile key `{key}` at line {line_number}")
    }
}

fn validate_profile(profile: &GajaeRouteProfile) -> RouteValidation {
    let mut unknown_events = Vec::new();
    let mut unsupported_commands = Vec::new();
    for (event, command) in &profile.routes {
        if !SUPPORTED_EVENTS.contains(&event.as_str()) {
            unknown_events.push(event.clone());
        }
        if !command_matches_event(command, event) {
            unsupported_commands.push((event.clone(), command.clone()));
        }
    }
    RouteValidation {
        unknown_events,
        unsupported_commands,
    }
}

fn command_matches_event(command: &str, event: &str) -> bool {
    command == format!("gajae handle {event}")
        || command == format!("gajae runtime handle --router clawhip --event {event}")
}

fn summarize_route_command(command: &str, event: &str) -> &'static str {
    if command_matches_event(command, event) {
        "supported GAJAE handler (command redacted)"
    } else {
        "unsupported command (redacted)"
    }
}

fn print_validation(validation: &RouteValidation) {
    if validation.is_clean() {
        println!("validation: ok");
        return;
    }
    println!("validation: failed");
    for event in &validation.unknown_events {
        println!("unknown event: {event}");
    }
    for (event, _) in &validation.unsupported_commands {
        println!("unsupported command for event: {event}");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandExit {
    pub success: bool,
    pub code: Option<i32>,
}

pub trait CommandRunner {
    fn output(&mut self, program: &Path, args: &[&str]) -> io::Result<CommandOutput>;
    fn status_inherited_output(&mut self, program: &Path, args: &[&str])
    -> io::Result<CommandExit>;
    fn output_with_stdin(
        &mut self,
        program: &Path,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> io::Result<CommandOutput>;
}

#[derive(Debug, Default)]
pub struct StdCommandRunner;

impl CommandRunner for StdCommandRunner {
    fn output(&mut self, program: &Path, args: &[&str]) -> io::Result<CommandOutput> {
        let output = Command::new(program).args(args).output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn status_inherited_output(
        &mut self,
        program: &Path,
        args: &[&str],
    ) -> io::Result<CommandExit> {
        let status = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;
        Ok(CommandExit {
            success: status.success(),
            code: status.code(),
        })
    }
    fn output_with_stdin(
        &mut self,
        program: &Path,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> io::Result<CommandOutput> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(input) = stdin
            && let Some(mut child_stdin) = child.stdin.take()
        {
            child_stdin.write_all(input)?;
        }
        let output = child.wait_with_output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

pub fn run(command: GajaeCommand) -> Result<()> {
    let mut runner = StdCommandRunner;
    match command {
        GajaeCommand::Status => run_status_with(&mut runner, |name| std::env::var_os(name)),
    }
}

fn discover_gajae_with(env_var: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    env_var(GAJAE_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(GAJAE_PATH_NAME))
}

fn run_status_with(
    runner: &mut impl CommandRunner,
    env_var: impl Fn(&str) -> Option<OsString>,
) -> Result<()> {
    let bin = discover_gajae_with(env_var);
    match runner.output(&bin, &["--help"]) {
        Ok(output) if output.success => {
            println!("gajae available: {}", bin.display());
            Ok(())
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "gajae found at {} but `--help` failed{}",
                bin.display(),
                concise_detail(&stderr)
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            bail!("gajae unavailable: set {GAJAE_ENV} or install `{GAJAE_PATH_NAME}` on PATH")
        }
        Err(error) => Err(error).with_context(|| format!("failed to run {} --help", bin.display())),
    }
}

pub fn run_profile_install() -> Result<CommandExit> {
    let mut runner = StdCommandRunner;
    run_profile_install_with(&mut runner, |name| std::env::var_os(name))
}

fn run_profile_install_with(
    runner: &mut impl CommandRunner,
    env_var: impl Fn(&str) -> Option<OsString>,
) -> Result<CommandExit> {
    let bin = discover_gajae_with(env_var);
    let status = runner
        .status_inherited_output(&bin, PROFILE_INSTALL_ARGS)
        .with_context(|| {
            format!(
                "failed to run {} {}",
                bin.display(),
                PROFILE_INSTALL_ARGS.join(" ")
            )
        })?;

    Ok(status)
}

pub fn profile_install_failure_message(status: CommandExit) -> String {
    format!(
        "gajae clawhip profile install failed{}",
        status
            .code
            .map(|code| format!(" with exit code {code}"))
            .unwrap_or_else(|| " without an exit code".to_string())
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptSource {
    File(PathBuf),
    Stdin(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptIngestRequest {
    pub family: String,
    pub source: ReceiptSource,
    pub channel: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReceiptIngestResult {
    pub event: IncomingEvent,
}

pub fn read_receipt_stdin(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut input = Vec::new();
    reader
        .take((RECEIPT_STDIN_LIMIT + 1) as u64)
        .read_to_end(&mut input)
        .context("failed to read receipt from stdin")?;
    if input.len() > RECEIPT_STDIN_LIMIT {
        bail!("receipt stdin exceeds {RECEIPT_STDIN_LIMIT} byte limit");
    }
    Ok(input)
}

pub fn ingest_receipt(request: ReceiptIngestRequest) -> Result<ReceiptIngestResult> {
    let mut runner = StdCommandRunner;
    ingest_receipt_with(&mut runner, |name| std::env::var_os(name), request)
}

fn ingest_receipt_with(
    runner: &mut impl CommandRunner,
    env_var: impl Fn(&str) -> Option<OsString>,
    request: ReceiptIngestRequest,
) -> Result<ReceiptIngestResult> {
    let family = sanitize_family(&request.family)?;
    let bin = discover_gajae_with(env_var);
    let temp;
    let file_path = match &request.source {
        ReceiptSource::File(path) => path.as_path(),
        ReceiptSource::Stdin(input) => {
            temp = write_receipt_tempfile(input)?;
            temp.path()
        }
    };
    let file_arg = file_path
        .to_str()
        .ok_or_else(|| anyhow!("receipt file path is not valid UTF-8"))?;
    let args = [family.as_str(), "validate", "--file", file_arg];
    let output = runner
        .output_with_stdin(&bin, &args, None)
        .with_context(|| format!("failed to run {} {} validate", bin.display(), family))?;
    if !output.success {
        bail!(
            "gajae receipt validation failed for family {}{}",
            family,
            validation_detail(&output)
        );
    }

    let validation = parse_validation_output(&output)?;
    Ok(ReceiptIngestResult {
        event: receipt_event(&family, validation, request.channel),
    })
}

fn write_receipt_tempfile(input: &[u8]) -> Result<TempReceiptFile> {
    let path = std::env::temp_dir().join(format!(
        "clawhip-gajae-receipt-{}.json",
        uuid::Uuid::new_v4()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .context("failed to create temporary receipt file")?;
    file.write_all(input)
        .context("failed to write temporary receipt file")?;
    file.sync_all()
        .context("failed to sync temporary receipt file")?;
    Ok(TempReceiptFile { path })
}

#[derive(Debug)]
struct TempReceiptFile {
    path: PathBuf,
}

impl TempReceiptFile {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempReceiptFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn parse_validation_output(output: &CommandOutput) -> Result<Value> {
    if let Ok(value) = serde_json::from_slice::<Value>(&output.stdout)
        && value.is_object()
    {
        return Ok(value);
    }
    if let Ok(value) = serde_json::from_slice::<Value>(&output.stderr)
        && value.is_object()
    {
        return Ok(value);
    }
    Ok(json!({}))
}

fn receipt_event(family: &str, validation: Value, channel: Option<String>) -> IncomingEvent {
    let mut payload = Map::new();
    payload.insert("family".into(), json!(family));
    payload.insert("status".into(), json!("validated"));
    insert_safe_string(
        &mut payload,
        "receipt_id",
        first_string(&validation, &["receipt_id", "id"]),
    );
    insert_safe_string(
        &mut payload,
        "subject",
        first_string(&validation, &["subject", "target"]),
    );
    insert_safe_string(
        &mut payload,
        "verdict",
        first_string(&validation, &["verdict", "decision", "outcome"]),
    );
    insert_safe_string(
        &mut payload,
        "summary",
        first_string(&validation, &["summary", "reason"]),
    );

    IncomingEvent::workspace(
        event_kind_for_family(family).to_string(),
        Value::Object(payload),
        channel,
    )
}

fn event_kind_for_family(family: &str) -> &'static str {
    match family {
        "review-verdict-evidence" => "gajae.review.verdict",
        "merge-hold-decision" => "gajae.merge.hold",
        "zero-backlog-checkpoint" => "gajae.backlog.zero",
        family if family.contains("release-hold") => "gajae.release.hold",
        _ => "gajae.receipt.validated",
    }
}

fn first_string(value: &Value, keys: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .map(bounded_public_string)
        .filter(|value| !value.is_empty())
}

fn insert_safe_string(object: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        object.insert(key.to_string(), json!(value));
    }
}

fn sanitize_family(family: &str) -> Result<String> {
    let family = family.trim();
    if family.is_empty() {
        bail!("receipt family is required");
    }
    if !family
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("receipt family must contain only lowercase letters, digits, and '-' characters");
    }
    Ok(family.to_string())
}

fn validation_detail(_output: &CommandOutput) -> String {
    ": validator rejected receipt".to_string()
}

fn bounded_public_string(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        let safe = match ch {
            '\n' | '\r' | '\t' => ' ',
            '/' | '\\' => ' ',
            ch if ch.is_control() => ' ',
            ch => ch,
        };
        if out.len() + safe.len_utf8() > SUMMARY_LIMIT {
            break;
        }
        out.push(safe);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn concise_detail(stderr: &str) -> String {
    stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| format!(": {}", line.trim()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Call {
        program: PathBuf,
        args: Vec<String>,
        inherits_stdout_stderr: bool,
        stdin_null: bool,
        stdin_piped: bool,
    }

    #[derive(Debug)]
    struct MockRunner {
        calls: Vec<Call>,
        output_result: io::Result<CommandOutput>,
        status_result: io::Result<CommandExit>,
        output_with_stdin_result: io::Result<CommandOutput>,
    }

    impl MockRunner {
        fn available() -> Self {
            Self {
                calls: Vec::new(),
                output_result: Ok(CommandOutput {
                    success: true,
                    stdout: b"help".to_vec(),
                    stderr: Vec::new(),
                }),
                status_result: Ok(CommandExit {
                    success: true,
                    code: Some(0),
                }),
                output_with_stdin_result: Ok(CommandOutput {
                    success: true,
                    stdout: br#"{"receipt_id":"r1","verdict":"approve","summary":"safe summary"}"#
                        .to_vec(),
                    stderr: Vec::new(),
                }),
            }
        }

        fn failing_status(code: i32) -> Self {
            Self {
                status_result: Ok(CommandExit {
                    success: false,
                    code: Some(code),
                }),
                ..Self::available()
            }
        }
    }

    impl CommandRunner for MockRunner {
        fn output(&mut self, program: &Path, args: &[&str]) -> io::Result<CommandOutput> {
            self.calls.push(Call {
                program: program.to_path_buf(),
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
                inherits_stdout_stderr: false,
                stdin_null: false,
                stdin_piped: false,
            });
            self.output_result
                .as_ref()
                .map(Clone::clone)
                .map_err(|error| io::Error::new(error.kind(), error.to_string()))
        }

        fn status_inherited_output(
            &mut self,
            program: &Path,
            args: &[&str],
        ) -> io::Result<CommandExit> {
            self.calls.push(Call {
                program: program.to_path_buf(),
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
                inherits_stdout_stderr: true,
                stdin_null: true,
                stdin_piped: false,
            });
            self.status_result
                .as_ref()
                .copied()
                .map_err(|error| io::Error::new(error.kind(), error.to_string()))
        }

        fn output_with_stdin(
            &mut self,
            program: &Path,
            args: &[&str],
            stdin: Option<&[u8]>,
        ) -> io::Result<CommandOutput> {
            self.calls.push(Call {
                program: program.to_path_buf(),
                args: args.iter().map(|arg| (*arg).to_string()).collect(),
                inherits_stdout_stderr: false,
                stdin_null: stdin.is_none(),
                stdin_piped: stdin.is_some(),
            });
            self.output_with_stdin_result
                .as_ref()
                .map(Clone::clone)
                .map_err(|error| io::Error::new(error.kind(), error.to_string()))
        }
    }

    fn write_fake_gajae(script: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("fake-gajae.sh");
        let tmp_path = dir.path().join("fake-gajae.sh.tmp");
        {
            let mut file = fs::File::create(&tmp_path).expect("create fake gajae");
            file.write_all(script.as_bytes()).expect("write fake gajae");
            file.sync_all().expect("sync fake gajae");
        }
        let mut permissions = fs::metadata(&tmp_path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tmp_path, permissions).expect("chmod fake gajae");
        fs::rename(&tmp_path, &path).expect("install fake gajae");
        (dir, path)
    }

    fn handler_action() -> HandlerAction {
        HandlerAction {
            subcommand: "handle-event".into(),
            args: vec!["--label".into(), "semi;colon $(touch nope)".into()],
            requires_approval: false,
        }
    }

    fn handler_limits(timeout_ms: u64, max_output_bytes: usize) -> HandlerLimits {
        HandlerLimits {
            timeout: Duration::from_millis(timeout_ms),
            max_output_bytes,
        }
    }

    #[tokio::test]
    async fn handler_passes_fixed_args_and_event_json_stdin_without_shell_interpolation() {
        let script = r#"#!/bin/sh
printf '%s\n' "$@" > "$FAKE_ARG_FILE"
python3 -c 'import json, os, sys; data=json.load(sys.stdin); open(os.environ["FAKE_STDIN_FILE"], "w").write(data["type"] + "\n")'
printf '{"summary":"ok"}'
"#;
        let (_dir, bin) = write_fake_gajae(script);
        let out_dir = tempdir().expect("out tempdir");
        let arg_file = out_dir.path().join("args.txt");
        let stdin_file = out_dir.path().join("stdin.txt");
        unsafe {
            std::env::set_var("FAKE_ARG_FILE", &arg_file);
            std::env::set_var("FAKE_STDIN_FILE", &stdin_file);
        }

        let outcome = run_handler_with_bin(
            &bin,
            &handler_action(),
            &json!({"type": "github.pr.opened"}),
            handler_limits(1_000, 1_024),
        )
        .await
        .expect("handler should run");

        assert_eq!(outcome, HandlerOutcome::Completed(json!({"summary": "ok"})));
        let args = fs::read_to_string(&arg_file).expect("args file");
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            vec![
                "clawhip",
                "handler",
                "handle-event",
                "--label",
                "semi;colon $(touch nope)"
            ]
        );
        assert_eq!(
            fs::read_to_string(stdin_file).expect("stdin file"),
            "github.pr.opened\n"
        );
        assert!(!out_dir.path().join("nope").exists());
    }

    #[tokio::test]
    async fn handler_timeout_reports_timeout() {
        let (_dir, bin) = write_fake_gajae("#!/bin/sh\nsleep 2\n");
        let outcome = run_handler_with_bin(
            &bin,
            &handler_action(),
            &json!({"type": "github.pr.opened"}),
            handler_limits(10, 1_024),
        )
        .await
        .expect("handler should time out cleanly");

        assert_eq!(outcome, HandlerOutcome::TimedOut);
    }

    #[tokio::test]
    async fn handler_nonzero_bounds_diagnostics() {
        let script = "#!/bin/sh\npython3 -c 'import sys; sys.stdout.write(\"o\" * 2000); sys.stderr.write(\"e\" * 2000)'\nexit 17\n";
        let (_dir, bin) = write_fake_gajae(script);
        let outcome = run_handler_with_bin(
            &bin,
            &handler_action(),
            &json!({"type": "github.pr.opened"}),
            handler_limits(1_000, 4_096),
        )
        .await
        .expect("handler should report failure");

        let HandlerOutcome::Failed {
            code,
            stdout,
            stderr,
        } = outcome
        else {
            panic!("unexpected outcome")
        };
        assert_eq!(code, Some(17));
        assert!(stdout.len() <= DIAGNOSTIC_BYTES);
        assert!(stderr.len() <= DIAGNOSTIC_BYTES);
    }

    #[tokio::test]
    async fn handler_output_cap_kills_process_and_retains_only_bounded_bytes() {
        let script = r#"#!/bin/sh
while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done
printf 'not killed' > "$FAKE_MARKER_FILE"
"#;
        let (_dir, bin) = write_fake_gajae(script);
        let out_dir = tempdir().expect("out tempdir");
        let marker_file = out_dir.path().join("marker.txt");
        unsafe {
            std::env::set_var("FAKE_MARKER_FILE", &marker_file);
        }

        let started = std::time::Instant::now();
        let outcome = run_handler_with_bin(
            &bin,
            &handler_action(),
            &json!({"type": "github.pr.opened"}),
            handler_limits(5_000, 64),
        )
        .await
        .expect("handler should be killed on output cap");

        let HandlerOutcome::Failed { stdout, stderr, .. } = outcome else {
            panic!("unexpected outcome")
        };
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "output cap should kill before timeout"
        );
        assert!(stdout.len() <= 64);
        assert!(stderr.len() <= 64);
        assert!(!marker_file.exists());
    }

    #[tokio::test]
    async fn handler_mutating_output_requires_approval() {
        let (_dir, bin) = write_fake_gajae(
            "#!/bin/sh\nprintf '{\"mutation_requested\":true,\"summary\":\"needs approval\"}'\n",
        );
        let outcome = run_handler_with_bin(
            &bin,
            &handler_action(),
            &json!({"type": "github.pr.opened"}),
            handler_limits(1_000, 1_024),
        )
        .await
        .expect("handler should run");

        assert_eq!(
            outcome,
            HandlerOutcome::ApprovalRequired(
                json!({"mutation_requested": true, "summary": "needs approval"})
            )
        );
    }

    #[test]
    fn handler_rejects_unapproved_subcommand() {
        let action = HandlerAction {
            subcommand: "sh -c touch nope".into(),
            args: Vec::new(),
            requires_approval: false,
        };

        assert!(validate_handler_action(&action).is_err());
    }

    #[test]
    fn profile_parser_loads_routes_file_and_validates_supported_commands() {
        let profile = parse_profile(
            r#"
profile: gajae
routes:
  github.pr-status-changed:
    command: gajae handle github.pr-status-changed
  session.started:
    command: gajae runtime handle --router clawhip --event session.started
"#,
            PathBuf::from(".clawhip/gajae.routes.yml"),
        )
        .expect("profile should parse");

        assert_eq!(profile.name.as_deref(), Some("gajae"));
        assert_eq!(profile.routes.len(), 2);
        assert!(validate_profile(&profile).is_clean());
    }

    #[test]
    fn profile_parser_loads_nested_clawhip_profile_routes() {
        let profile = parse_profile(
            r#"
runtime: hermes
clawhipProfile:
  name: gajae
  routes:
    github.issue-opened:
      command: gajae handle github.issue-opened
safety:
  liveClawhipEnablement: false
"#,
            PathBuf::from(".gajae/runtime/hermes/clawhip-profile.yml"),
        )
        .expect("nested profile should parse");

        assert_eq!(profile.name.as_deref(), Some("gajae"));
        assert_eq!(
            profile
                .routes
                .get("github.issue-opened")
                .map(String::as_str),
            Some("gajae handle github.issue-opened")
        );
        assert!(validate_profile(&profile).is_clean());
    }

    #[test]
    fn profile_loader_follows_clawhip_profile_routes_file_from_cwd() {
        let temp = tempdir().expect("tempdir");
        let profile_path = temp
            .path()
            .join(".gajae/runtime/hermes/clawhip-profile.yml");
        fs::create_dir_all(profile_path.parent().expect("profile parent")).expect("profile dir");
        let routes_path = temp.path().join(".clawhip/gajae.routes.yml");
        fs::create_dir_all(routes_path.parent().expect("routes parent")).expect("routes dir");
        fs::write(
            &profile_path,
            r#"
runtime: hermes
clawhipProfile:
  name: gajae
  routesFile: .clawhip/gajae.routes.yml
"#,
        )
        .expect("write profile");
        fs::write(
            &routes_path,
            r#"
routes:
  session.started:
    command: gajae handle session.started
"#,
        )
        .expect("write routes");

        let profile = load_profile_from_cwd(Some(profile_path.as_path()), temp.path())
            .expect("profile should load referenced routes");

        assert_eq!(profile.name.as_deref(), Some("gajae"));
        assert_eq!(
            profile.routes.get("session.started").map(String::as_str),
            Some("gajae handle session.started")
        );
        assert_eq!(profile.source, routes_path);
        assert!(validate_profile(&profile).is_clean());
    }

    #[test]
    fn profile_loader_rejects_oversized_primary_profile_without_raw_contents() {
        let temp = tempdir().expect("tempdir");
        let profile_path = temp.path().join(".clawhip/gajae.routes.yml");
        fs::create_dir_all(profile_path.parent().expect("profile parent")).expect("profile dir");
        fs::write(
            &profile_path,
            format!(
                "routes:\n  session.started:\n    command: gajae handle session.started\n# {}\n",
                "secret-token-123".repeat(MAX_PROFILE_BYTES / 16)
            ),
        )
        .expect("write oversized profile");

        let error = load_profile_from_cwd(Some(profile_path.as_path()), temp.path())
            .expect_err("oversized primary profile should fail");
        let message = error.to_string();

        assert!(message.contains("exceeds maximum size"));
        assert!(!message.contains("secret-token-123"));
    }

    #[test]
    fn profile_loader_rejects_oversized_referenced_routes_without_raw_contents() {
        let temp = tempdir().expect("tempdir");
        let profile_path = temp
            .path()
            .join(".gajae/runtime/hermes/clawhip-profile.yml");
        fs::create_dir_all(profile_path.parent().expect("profile parent")).expect("profile dir");
        let routes_path = temp.path().join(".clawhip/gajae.routes.yml");
        fs::create_dir_all(routes_path.parent().expect("routes parent")).expect("routes dir");
        fs::write(
            &profile_path,
            r#"
runtime: hermes
clawhipProfile:
  name: gajae
  routesFile: .clawhip/gajae.routes.yml
"#,
        )
        .expect("write profile");
        fs::write(
            &routes_path,
            format!(
                "routes:\n  session.started:\n    command: gajae handle session.started\n# {}\n",
                "secret-token-123".repeat(MAX_PROFILE_BYTES / 16)
            ),
        )
        .expect("write oversized routes");

        let error = load_profile_from_cwd(Some(profile_path.as_path()), temp.path())
            .expect_err("oversized referenced routes should fail");
        let message = error.to_string();

        assert!(message.contains("referenced GAJAE routes file"));
        assert!(message.contains("exceeds maximum size"));
        assert!(!message.contains("secret-token-123"));
    }

    #[test]
    fn profile_parser_rejects_list_style_route_entries() {
        let error = parse_profile(
            r#"
routes:
  - event: github.issue-opened
  session.started:
    command: gajae handle session.started
"#,
            PathBuf::from(".clawhip/gajae.routes.yml"),
        )
        .expect_err("list-style routes should fail");
        let message = error.to_string();

        assert!(message.contains("list-style route entry"));
        assert!(message.contains("line 3"));
        assert!(!message.contains("github.issue-opened"));
    }

    #[test]
    fn profile_parser_rejects_nested_list_style_route_entries() {
        let error = parse_profile(
            r#"
runtime: hermes
clawhipProfile:
  routes:
    - event: github.issue-opened
"#,
            PathBuf::from(".gajae/runtime/hermes/clawhip-profile.yml"),
        )
        .expect_err("nested list-style routes should fail");
        let message = error.to_string();

        assert!(message.contains("list-style route entry"));
        assert!(message.contains("line 5"));
        assert!(!message.contains("github.issue-opened"));
    }

    #[test]
    fn profile_parser_rejects_missing_command_route_even_with_valid_route() {
        let error = parse_profile(
            r#"
routes:
  github.issue-opened:
  session.started:
    command: gajae handle session.started
"#,
            PathBuf::from(".clawhip/gajae.routes.yml"),
        )
        .expect_err("missing command should fail");
        let message = error.to_string();

        assert!(message.contains("github.issue-opened"));
        assert!(message.contains("missing command"));
    }

    #[test]
    fn profile_validation_detects_unknown_events_and_unsupported_commands() {
        let profile = parse_profile(
            r#"
routes:
  github.pr.closed:
    command: gajae handle github.pr.closed
  github.pr-status-changed:
    command: rm -rf /tmp/example
"#,
            PathBuf::from(".clawhip/gajae.routes.yml"),
        )
        .expect("profile should parse before semantic validation");

        let validation = validate_profile(&profile);
        assert_eq!(validation.unknown_events, vec!["github.pr.closed"]);
        assert_eq!(validation.unsupported_commands.len(), 1);
        assert_eq!(
            validation.unsupported_commands[0].0,
            "github.pr-status-changed"
        );
    }

    #[test]
    fn profile_validation_accepts_current_events_and_rejects_stale_dotted_events() {
        let current = parse_profile(
            r#"
routes:
  github.issue-opened:
    command: gajae handle github.issue-opened
  github.issue-commented:
    command: gajae handle github.issue-commented
  github.issue-closed:
    command: gajae handle github.issue-closed
  github.pr-status-changed:
    command: gajae handle github.pr-status-changed
  session.started:
    command: gajae handle session.started
  session.blocked:
    command: gajae handle session.blocked
  session.finished:
    command: gajae handle session.finished
  tmux.stale:
    command: gajae handle tmux.stale
"#,
            PathBuf::from(".clawhip/gajae.routes.yml"),
        )
        .expect("current profile should parse");
        assert!(validate_profile(&current).is_clean());

        let stale = parse_profile(
            r#"
routes:
  github.issue.opened:
    command: gajae handle github.issue.opened
  github.pr.opened:
    command: gajae handle github.pr.opened
  session.completed:
    command: gajae handle session.completed
  session.stale:
    command: gajae handle session.stale
"#,
            PathBuf::from(".clawhip/gajae.routes.yml"),
        )
        .expect("stale profile should parse before semantic validation");
        assert_eq!(
            validate_profile(&stale).unknown_events,
            vec![
                "github.issue.opened",
                "github.pr.opened",
                "session.completed",
                "session.stale",
            ]
        );
    }

    #[test]
    fn profile_command_summary_redacts_raw_command_details() {
        let event = "github.issue-opened";
        let secret_command = "gajae handle github.issue-opened --token secret-token-123 --webhook https://hooks.example/secret --path /home/operator/private";

        assert_eq!(
            summarize_route_command("gajae handle github.issue-opened", event),
            "supported GAJAE handler (command redacted)"
        );
        let summary = summarize_route_command(secret_command, event);
        assert_eq!(summary, "unsupported command (redacted)");
        assert!(!summary.contains("secret-token-123"));
        assert!(!summary.contains("https://hooks.example/secret"));
        assert!(!summary.contains("/home/operator/private"));
    }

    #[test]
    fn malformed_profile_error_is_sanitized_without_raw_input() {
        let raw_secret = "routes:\n  github.pr-status-changed command: secret-token-123\n";
        let error = parse_profile(raw_secret, PathBuf::from(".clawhip/gajae.routes.yml"))
            .expect_err("malformed profile should fail");
        let message = error.to_string();

        assert!(message.contains("line 2"), "unexpected message: {message}");
        assert!(
            !message.contains("secret-token-123"),
            "leaked raw input: {message}"
        );
        assert!(
            !message.contains("github.pr-status-changed command"),
            "leaked raw input: {message}"
        );
    }

    #[test]
    fn route_file_rejects_unknown_top_level_key_without_raw_value() {
        let error = parse_profile(
            "routes:\n  session.started:\n    command: gajae handle session.started\nprivate: secret-token-123\n",
            PathBuf::from(".clawhip/gajae.routes.yml"),
        )
        .expect_err("unknown key should fail");
        let message = error.to_string();

        assert!(message.contains("unsupported GAJAE profile key `private`"));
        assert!(!message.contains("secret-token-123"));
    }

    #[test]
    fn gajae_status_prefers_gajae_bin_env_override() {
        let mut runner = MockRunner::available();
        run_status_with(&mut runner, |name| {
            (name == GAJAE_ENV).then(|| OsString::from("/custom/gajae"))
        })
        .expect("status should pass");

        assert_eq!(
            runner.calls,
            vec![Call {
                program: PathBuf::from("/custom/gajae"),
                args: vec!["--help".into()],
                inherits_stdout_stderr: false,
                stdin_null: false,
                stdin_piped: false,
            }]
        );
    }

    #[test]
    fn gajae_status_uses_path_name_when_env_is_absent() {
        let mut runner = MockRunner::available();
        run_status_with(&mut runner, |_| None).expect("status should pass");

        assert_eq!(runner.calls[0].program, PathBuf::from("gajae"));
    }

    #[test]
    fn gajae_status_fails_when_help_exits_nonzero() {
        let mut runner = MockRunner {
            output_result: Ok(CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"usage unavailable".to_vec(),
            }),
            ..MockRunner::available()
        };

        let error = run_status_with(&mut runner, |_| None).expect_err("nonzero help should fail");

        assert!(
            error.to_string().contains("usage unavailable"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn profile_install_constructs_expected_args_inherits_output_and_closes_stdin() {
        let mut runner = MockRunner::available();
        let status = run_profile_install_with(&mut runner, |_| None).expect("install should run");
        assert!(status.success);

        assert_eq!(
            runner.calls,
            vec![Call {
                program: PathBuf::from("gajae"),
                args: PROFILE_INSTALL_ARGS
                    .iter()
                    .map(|arg| (*arg).to_string())
                    .collect(),
                inherits_stdout_stderr: true,
                stdin_null: true,
                stdin_piped: false,
            }]
        );
    }

    #[test]
    fn profile_install_fails_on_nonzero_status() {
        let mut runner = MockRunner::failing_status(17);
        let status =
            run_profile_install_with(&mut runner, |_| None).expect("nonzero still reports status");
        assert_eq!(status.code, Some(17));

        let message = profile_install_failure_message(status);
        assert!(
            message.contains("exit code 17"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn receipt_ingest_invokes_family_validator_and_maps_safe_event() {
        let mut runner = MockRunner::available();
        let result = ingest_receipt_with(
            &mut runner,
            |_| Some(OsString::from("/custom/gajae")),
            ReceiptIngestRequest {
                family: "review-verdict-evidence".into(),
                source: ReceiptSource::File(PathBuf::from("receipt.json")),
                channel: Some("ops".into()),
            },
        )
        .expect("receipt should validate");

        assert_eq!(result.event.kind, "gajae.review.verdict");
        assert_eq!(result.event.channel.as_deref(), Some("ops"));
        assert_eq!(
            result.event.payload["family"],
            json!("review-verdict-evidence")
        );
        assert_eq!(result.event.payload["receipt_id"], json!("r1"));
        assert_eq!(result.event.payload["verdict"], json!("approve"));
        assert_eq!(result.event.payload["summary"], json!("safe summary"));
        assert_eq!(
            runner.calls,
            vec![Call {
                program: PathBuf::from("/custom/gajae"),
                args: vec![
                    "review-verdict-evidence".into(),
                    "validate".into(),
                    "--file".into(),
                    "receipt.json".into(),
                ],
                inherits_stdout_stderr: false,
                stdin_null: true,
                stdin_piped: false,
            }]
        );
    }

    #[test]
    fn receipt_ingest_rejects_invalid_receipt_with_bounded_public_diagnostic() {
        let mut runner = MockRunner {
            output_with_stdin_result: Ok(CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: format!("/secret/path/token {}", "x".repeat(400)).into_bytes(),
            }),
            ..MockRunner::available()
        };

        let error = ingest_receipt_with(
            &mut runner,
            |_| None,
            ReceiptIngestRequest {
                family: "runtime-followup-receipt".into(),
                source: ReceiptSource::File(PathBuf::from("receipt.json")),
                channel: None,
            },
        )
        .expect_err("invalid receipt should fail");
        let message = error.to_string();
        assert!(message.contains("gajae receipt validation failed"));
        assert!(!message.contains("/secret/path"), "message={message}");
        assert!(message.len() < 360, "message too long: {}", message.len());
    }
}
