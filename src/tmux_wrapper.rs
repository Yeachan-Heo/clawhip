use std::fs;
use std::path::Path;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::sleep;

use crate::Result;
use crate::cli::{TmuxNewArgs, TmuxWatchArgs, TmuxWrapperFormat};
use crate::client::DaemonClient;
use crate::config::AppConfig;
use crate::events::RoutingMetadata;
use crate::router::resolve_tmux_session_channel_with_metadata;
use crate::source::tmux::{
    ParentProcessInfo, RegisteredTmuxSession, RegistrationSource, content_hash,
    current_timestamp_rfc3339, monitor_registered_session, session_exists, tmux_bin,
};

pub async fn run(args: TmuxNewArgs, config: &AppConfig) -> Result<()> {
    launch_session(&args).await?;
    let monitor_args = TmuxMonitorArgs::from_new_args(&args, config);
    let monitor = register_and_start_monitor(monitor_args, config).await?;

    if args.attach {
        attach_session(&args.session).await?;
    }

    monitor.await??;
    Ok(())
}

pub async fn watch(args: TmuxWatchArgs, config: &AppConfig) -> Result<()> {
    if !session_exists(&args.session).await? {
        return Err(format!("tmux session '{}' does not exist", args.session).into());
    }

    let monitor = register_and_start_monitor(TmuxMonitorArgs::from(&args), config).await?;
    monitor.await??;
    Ok(())
}

#[derive(Clone)]
struct TmuxMonitorArgs {
    session: String,
    channel: Option<String>,
    mention: Option<String>,
    routing: RoutingMetadata,
    keywords: Vec<String>,
    keyword_window_secs: u64,
    stale_minutes: u64,
    format: Option<TmuxWrapperFormat>,
    registered_at: String,
    registration_source: RegistrationSource,
    parent_process: Option<ParentProcessInfo>,
}

impl From<&TmuxNewArgs> for TmuxMonitorArgs {
    fn from(value: &TmuxNewArgs) -> Self {
        Self {
            session: value.session.clone(),
            channel: value.channel.clone(),
            mention: value.mention.clone(),
            routing: routing_metadata_for_cwd(value.cwd.as_deref()),
            keywords: value.keywords.clone(),
            keyword_window_secs: default_keyword_window_secs(),
            stale_minutes: value.stale_minutes,
            format: value.format,
            registered_at: current_timestamp_rfc3339(),
            registration_source: RegistrationSource::CliNew,
            parent_process: current_parent_process_info(),
        }
    }
}

impl TmuxMonitorArgs {
    fn from_new_args(value: &TmuxNewArgs, config: &AppConfig) -> Self {
        let mut monitor_args = Self::from(value);
        if monitor_args.channel.is_none() {
            monitor_args.channel = resolve_tmux_session_channel_with_metadata(
                config,
                &value.session,
                &monitor_args.routing,
            );
        }
        monitor_args
    }
}

impl From<&TmuxWatchArgs> for TmuxMonitorArgs {
    fn from(value: &TmuxWatchArgs) -> Self {
        Self {
            session: value.session.clone(),
            channel: value.channel.clone(),
            mention: value.mention.clone(),
            routing: routing_metadata_for_session(&value.session),
            keywords: value.keywords.clone(),
            keyword_window_secs: default_keyword_window_secs(),
            stale_minutes: value.stale_minutes,
            format: value.format,
            registered_at: current_timestamp_rfc3339(),
            registration_source: RegistrationSource::CliWatch,
            parent_process: current_parent_process_info(),
        }
    }
}

impl From<TmuxMonitorArgs> for RegisteredTmuxSession {
    fn from(value: TmuxMonitorArgs) -> Self {
        Self {
            session: value.session,
            channel: value.channel,
            mention: value.mention,
            routing: value.routing,
            keywords: value.keywords,
            keyword_window_secs: value.keyword_window_secs,
            stale_minutes: value.stale_minutes,
            format: value.format.map(Into::into),
            registered_at: value.registered_at,
            registration_source: value.registration_source,
            parent_process: value.parent_process,
            active_wrapper_monitor: true,
            detect_waiting: false,
            waiting_interval: 0,
            mention_on: Vec::new(),
        }
    }
}

fn routing_metadata_for_cwd(cwd: Option<&str>) -> RoutingMetadata {
    let Some(cwd) = cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) else {
        return RoutingMetadata::default();
    };
    let workdir = Path::new(cwd);
    let worktree_path = fs::canonicalize(workdir)
        .unwrap_or_else(|_| workdir.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let repo_path = git_output(workdir, &["rev-parse", "--show-toplevel"]);
    let project = detect_project(workdir).or_else(|| {
        repo_path
            .as_deref()
            .map(|path| dir_basename(Path::new(path)))
    });
    let branch = git_output(workdir, &["branch", "--show-current"]);

    RoutingMetadata {
        project: project.clone(),
        repo_name: project,
        repo_path,
        worktree_path: Some(worktree_path),
        branch,
        ..RoutingMetadata::default()
    }
}

fn routing_metadata_for_session(session: &str) -> RoutingMetadata {
    let cwd = std::process::Command::new(tmux_bin())
        .args([
            "display-message",
            "-p",
            "-t",
            session,
            "#{pane_current_path}",
        ])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty());

    routing_metadata_for_cwd(cwd.as_deref())
}

fn detect_project(workdir: &Path) -> Option<String> {
    let common_dir = git_output(
        workdir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;

    Path::new(&common_dir)
        .parent()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
}

fn git_output(workdir: &Path, args: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(workdir)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn dir_basename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".into())
}

async fn register_and_start_monitor(
    args: TmuxMonitorArgs,
    config: &AppConfig,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let client = DaemonClient::from_config(config);
    let registration: RegisteredTmuxSession = args.into();
    eprintln!("{}", format_watch_audit_log(&registration));
    client.register_tmux(&registration).await?;

    let monitor_client = client.clone();
    Ok(tokio::spawn(async move {
        monitor_registered_session(registration, monitor_client).await
    }))
}

async fn launch_session(args: &TmuxNewArgs) -> Result<()> {
    let mut command = Command::new(tmux_bin());
    command
        .arg("new-session")
        .arg("-d")
        .arg("-s")
        .arg(&args.session);
    if let Some(window_name) = &args.window_name {
        command.arg("-n").arg(window_name);
    }
    if let Some(cwd) = &args.cwd {
        command.arg("-c").arg(cwd);
    }
    let output = command.output().await?;
    if !output.status.success() {
        return Err(tmux_stderr(&output.stderr).into());
    }

    if let Some(command) = build_command_to_send(args) {
        if args.retry_enter {
            send_keys_reliable(
                &args.session,
                &command,
                args.retry_enter_count,
                args.retry_enter_delay_ms,
            )
            .await?;
        } else {
            send_command_to_session(&args.session, &command).await?;
        }
    }

    Ok(())
}

async fn send_command_to_session(session: &str, command: &str) -> Result<()> {
    send_literal_keys(session, command).await?;
    send_enter_key(session, "Enter").await
}

async fn send_keys_reliable(
    session: &str,
    text: &str,
    retry_count: u32,
    retry_delay_ms: u64,
) -> Result<()> {
    send_literal_keys(session, text).await?;
    let mut baseline_hash = capture_target_hash(session).await?;

    for delay in retry_enter_delays(retry_count, retry_delay_ms) {
        send_enter_key(session, "Enter").await?;
        sleep(delay).await;
        let current_hash = capture_target_hash(session).await?;
        if current_hash != baseline_hash {
            return Ok(());
        }

        baseline_hash = current_hash;
    }

    Ok(())
}

fn retry_enter_delays(retry_count: u32, retry_delay_ms: u64) -> Vec<Duration> {
    let base_delay = retry_delay_ms.max(1);
    let mut next_delay_ms = base_delay;

    (0..=retry_count)
        .map(|_| {
            let delay = Duration::from_millis(next_delay_ms);
            next_delay_ms = next_delay_ms.saturating_mul(2);
            delay
        })
        .collect()
}

async fn send_literal_keys(session: &str, text: &str) -> Result<()> {
    let literal_output = Command::new(tmux_bin())
        .arg("send-keys")
        .arg("-t")
        .arg(session)
        .arg("-l")
        .arg(text)
        .output()
        .await?;
    if !literal_output.status.success() {
        return Err(tmux_stderr(&literal_output.stderr).into());
    }

    Ok(())
}

async fn send_enter_key(session: &str, key: &str) -> Result<()> {
    let enter_output = Command::new(tmux_bin())
        .arg("send-keys")
        .arg("-t")
        .arg(session)
        .arg(key)
        .output()
        .await?;
    if !enter_output.status.success() {
        return Err(tmux_stderr(&enter_output.stderr).into());
    }

    Ok(())
}

async fn capture_target_hash(target: &str) -> Result<u64> {
    let capture = Command::new(tmux_bin())
        .arg("capture-pane")
        .arg("-p")
        .arg("-t")
        .arg(target)
        .arg("-S")
        .arg("-200")
        .output()
        .await?;
    if !capture.status.success() {
        return Err(tmux_stderr(&capture.stderr).into());
    }

    Ok(content_hash(&String::from_utf8(capture.stdout)?))
}

fn build_command_to_send(args: &TmuxNewArgs) -> Option<String> {
    if args.command.is_empty() {
        return None;
    }

    let joined = if args.command.len() == 1 {
        args.command[0].clone()
    } else {
        shell_join(&args.command)
    };
    Some(match &args.shell {
        Some(shell) => format!("{} -c {}", shell_escape(shell), shell_escape(&joined)),
        None => joined,
    })
}

fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| shell_escape(part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_escape(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_@%+=:,./-".contains(ch))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn tmux_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).trim().to_string()
}

async fn attach_session(session: &str) -> Result<()> {
    let output = Command::new(tmux_bin())
        .arg("attach-session")
        .arg("-t")
        .arg(session)
        .output()
        .await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(tmux_stderr(&output.stderr).into())
    }
}

fn default_keyword_window_secs() -> u64 {
    30
}

fn current_parent_process_info() -> Option<ParentProcessInfo> {
    let pid = std::os::unix::process::parent_id();
    if pid == 0 {
        return None;
    }

    let name = fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    Some(ParentProcessInfo { pid, name })
}

fn format_watch_audit_log(registration: &RegisteredTmuxSession) -> String {
    let channel = registration.channel.as_deref().unwrap_or("-");
    let mention = registration.mention.as_deref().unwrap_or("-");
    let keywords = if registration.keywords.is_empty() {
        "-".to_string()
    } else {
        registration.keywords.join(",")
    };
    let format = registration
        .format
        .as_ref()
        .map(|format| format.as_str())
        .unwrap_or("-");
    let (parent_pid, parent_name) = registration
        .parent_process
        .as_ref()
        .map(|parent| {
            (
                parent.pid.to_string(),
                parent.name.as_deref().unwrap_or("-").to_string(),
            )
        })
        .unwrap_or_else(|| ("-".to_string(), "-".to_string()));

    format!(
        "clawhip tmux {} start session={} channel={} keywords={} mention={} stale_minutes={} format={} registered_at={} parent_pid={} parent_name={}",
        registration.registration_source.as_str(),
        registration.session,
        channel,
        keywords,
        mention,
        registration.stale_minutes,
        format,
        registration.registered_at,
        parent_pid,
        parent_name
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, DefaultsConfig, RouteRule};
    use std::collections::BTreeMap;
    use std::process::Command as StdCommand;
    use tempfile::tempdir;

    fn init_git_repo() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        let status = StdCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .expect("git init");
        assert!(status.success(), "git init should succeed");
        dir
    }

    #[test]
    fn build_command_to_send_preserves_shell_arguments_when_joining() {
        let args = TmuxNewArgs {
            session: "dev".into(),
            window_name: None,
            cwd: None,
            channel: None,
            mention: None,
            keywords: Vec::new(),
            stale_minutes: 10,
            format: None,
            attach: false,
            retry_enter: true,
            retry_enter_count: crate::cli::DEFAULT_RETRY_ENTER_COUNT,
            retry_enter_delay_ms: crate::cli::DEFAULT_RETRY_ENTER_DELAY_MS,
            shell: None,
            command: vec![
                "zsh".into(),
                "-c".into(),
                "source ~/.zshrc && omx --madmax".into(),
            ],
        };

        assert_eq!(
            build_command_to_send(&args).as_deref(),
            Some("zsh -c 'source ~/.zshrc && omx --madmax'")
        );
    }

    #[test]
    fn build_command_to_send_wraps_joined_command_with_override_shell() {
        let args = TmuxNewArgs {
            session: "dev".into(),
            window_name: None,
            cwd: None,
            channel: None,
            mention: None,
            keywords: Vec::new(),
            stale_minutes: 10,
            format: None,
            attach: false,
            retry_enter: true,
            retry_enter_count: crate::cli::DEFAULT_RETRY_ENTER_COUNT,
            retry_enter_delay_ms: crate::cli::DEFAULT_RETRY_ENTER_DELAY_MS,
            shell: Some("/bin/zsh".into()),
            command: vec!["source ~/.zshrc && omx --madmax".into()],
        };

        assert_eq!(
            build_command_to_send(&args).as_deref(),
            Some("/bin/zsh -c 'source ~/.zshrc && omx --madmax'")
        );
    }

    #[test]
    fn build_command_to_send_leaves_single_shell_snippet_unquoted_without_override() {
        let args = TmuxNewArgs {
            session: "dev".into(),
            window_name: None,
            cwd: None,
            channel: None,
            mention: None,
            keywords: Vec::new(),
            stale_minutes: 10,
            format: None,
            attach: false,
            retry_enter: true,
            retry_enter_count: crate::cli::DEFAULT_RETRY_ENTER_COUNT,
            retry_enter_delay_ms: crate::cli::DEFAULT_RETRY_ENTER_DELAY_MS,
            shell: None,
            command: vec!["source ~/.zshrc && omx --madmax".into()],
        };

        assert_eq!(
            build_command_to_send(&args).as_deref(),
            Some("source ~/.zshrc && omx --madmax")
        );
    }

    #[test]
    fn watch_args_convert_to_monitor_args() {
        let args = TmuxWatchArgs {
            session: "existing".into(),
            channel: Some("alerts".into()),
            mention: Some("<@123>".into()),
            keywords: vec!["error".into(), "complete".into()],
            stale_minutes: 15,
            format: Some(TmuxWrapperFormat::Inline),
            retry_enter: true,
        };

        let monitor_args = TmuxMonitorArgs::from(&args);

        assert_eq!(monitor_args.session, "existing");
        assert_eq!(monitor_args.channel.as_deref(), Some("alerts"));
        assert_eq!(monitor_args.mention.as_deref(), Some("<@123>"));
        assert_eq!(monitor_args.keywords, vec!["error", "complete"]);
        assert_eq!(monitor_args.keyword_window_secs, 30);
        assert_eq!(monitor_args.stale_minutes, 15);
        assert!(matches!(
            monitor_args.registration_source,
            RegistrationSource::CliWatch
        ));
        assert!(!monitor_args.registered_at.is_empty());
        assert!(matches!(
            monitor_args.format,
            Some(TmuxWrapperFormat::Inline)
        ));
    }

    #[test]
    fn registered_tmux_session_from_monitor_args_keeps_audit_metadata() {
        let registration: RegisteredTmuxSession = TmuxMonitorArgs {
            session: "issue-105".into(),
            channel: Some("alerts".into()),
            mention: None,
            routing: RoutingMetadata::default(),
            keywords: vec!["error".into()],
            keyword_window_secs: 30,
            stale_minutes: 10,
            format: Some(TmuxWrapperFormat::Alert),
            registered_at: "2026-04-02T00:00:00Z".into(),
            registration_source: RegistrationSource::CliNew,
            parent_process: Some(ParentProcessInfo {
                pid: 99,
                name: Some("bash".into()),
            }),
        }
        .into();

        assert_eq!(registration.registered_at, "2026-04-02T00:00:00Z");
        assert!(matches!(
            registration.registration_source,
            RegistrationSource::CliNew
        ));
        assert_eq!(registration.parent_process.unwrap().pid, 99);
    }

    #[test]
    fn format_watch_audit_log_contains_required_fields() {
        let log = format_watch_audit_log(&RegisteredTmuxSession {
            session: "issue-105".into(),
            channel: Some("alerts".into()),
            mention: Some("<@123>".into()),
            routing: RoutingMetadata::default(),
            keywords: vec!["error".into(), "complete".into()],
            keyword_window_secs: 30,
            stale_minutes: 12,
            format: Some(crate::events::MessageFormat::Alert),
            registered_at: "2026-04-02T00:00:00Z".into(),
            registration_source: RegistrationSource::CliWatch,
            parent_process: Some(ParentProcessInfo {
                pid: 42,
                name: Some("codex".into()),
            }),
            active_wrapper_monitor: true,
            ..Default::default()
        });

        assert!(log.contains("session=issue-105"));
        assert!(log.contains("channel=alerts"));
        assert!(log.contains("keywords=error,complete"));
        assert!(log.contains("mention=<@123>"));
        assert!(log.contains("stale_minutes=12"));
        assert!(log.contains("format=alert"));
        assert!(log.contains("registered_at=2026-04-02T00:00:00Z"));
        assert!(log.contains("parent_pid=42"));
        assert!(log.contains("parent_name=codex"));
    }

    #[test]
    fn new_args_auto_resolve_channel_from_routes() {
        let args = TmuxNewArgs {
            session: "xeroclaw-22".into(),
            window_name: None,
            cwd: None,
            channel: None,
            mention: None,
            keywords: Vec::new(),
            stale_minutes: 10,
            format: None,
            attach: false,
            retry_enter: true,
            retry_enter_count: crate::cli::DEFAULT_RETRY_ENTER_COUNT,
            retry_enter_delay_ms: crate::cli::DEFAULT_RETRY_ENTER_DELAY_MS,
            shell: None,
            command: vec!["codex".into()],
        };
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: crate::events::MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.*".into(),
                filter: BTreeMap::from([("session".into(), "xeroclaw-*".into())]),
                sink: "discord".into(),
                channel: Some("xeroclaw-dev".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: None,
                template: None,
            }],
            ..AppConfig::default()
        };

        let monitor_args = TmuxMonitorArgs::from_new_args(&args, &config);

        assert_eq!(monitor_args.channel.as_deref(), Some("xeroclaw-dev"));
    }

    #[test]
    fn new_args_auto_resolve_channel_prefers_repo_metadata_over_session_prefix_heuristics() {
        let repo = init_git_repo();
        let args = TmuxNewArgs {
            session: "clawhip-issue-152".into(),
            window_name: None,
            cwd: Some(repo.path().to_string_lossy().into_owned()),
            channel: None,
            mention: None,
            keywords: Vec::new(),
            stale_minutes: 10,
            format: None,
            attach: false,
            retry_enter: true,
            retry_enter_count: crate::cli::DEFAULT_RETRY_ENTER_COUNT,
            retry_enter_delay_ms: crate::cli::DEFAULT_RETRY_ENTER_DELAY_MS,
            shell: None,
            command: vec!["codex".into()],
        };
        let repo_name = repo
            .path()
            .file_name()
            .and_then(|value| value.to_str())
            .expect("repo name")
            .to_string();
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: crate::events::MessageFormat::Compact,
            },
            routes: vec![
                RouteRule {
                    event: "tmux.*".into(),
                    filter: BTreeMap::from([("session".into(), "clawhip-*".into())]),
                    sink: "discord".into(),
                    channel: Some("heuristic-route".into()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "session.*".into(),
                    filter: BTreeMap::from([("repo_name".into(), repo_name)]),
                    sink: "discord".into(),
                    channel: Some("metadata-route".into()),
                    ..RouteRule::default()
                },
            ],
            ..AppConfig::default()
        };

        let monitor_args = TmuxMonitorArgs::from_new_args(&args, &config);

        assert_eq!(monitor_args.channel.as_deref(), Some("metadata-route"));
        assert_eq!(
            monitor_args.routing.worktree_path.as_deref(),
            Some(repo.path().to_string_lossy().as_ref())
        );
        assert!(monitor_args.routing.repo_name.is_some());
    }

    #[test]
    fn new_args_keep_explicit_channel_over_route_resolution() {
        let args = TmuxNewArgs {
            session: "xeroclaw-22".into(),
            window_name: None,
            cwd: None,
            channel: Some("manual".into()),
            mention: None,
            keywords: Vec::new(),
            stale_minutes: 10,
            format: None,
            attach: false,
            retry_enter: true,
            retry_enter_count: crate::cli::DEFAULT_RETRY_ENTER_COUNT,
            retry_enter_delay_ms: crate::cli::DEFAULT_RETRY_ENTER_DELAY_MS,
            shell: None,
            command: vec!["codex".into()],
        };
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: crate::events::MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.*".into(),
                filter: BTreeMap::from([("session".into(), "xeroclaw-*".into())]),
                sink: "discord".into(),
                channel: Some("xeroclaw-dev".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: None,
                template: None,
            }],
            ..AppConfig::default()
        };

        let monitor_args = TmuxMonitorArgs::from_new_args(&args, &config);

        assert_eq!(monitor_args.channel.as_deref(), Some("manual"));
    }

    #[test]
    fn retry_enter_delays_respect_requested_backoff_limit() {
        assert_eq!(retry_enter_delays(0, 250), vec![Duration::from_millis(250)]);
        assert_eq!(
            retry_enter_delays(2, 250),
            vec![
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_millis(1_000)
            ]
        );
        assert_eq!(
            retry_enter_delays(4, 250),
            vec![
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_millis(1_000),
                Duration::from_millis(2_000),
                Duration::from_millis(4_000)
            ]
        );
    }

    #[test]
    fn retry_enter_delays_clamp_zero_delay_to_one_millisecond() {
        assert_eq!(
            retry_enter_delays(2, 0),
            vec![
                Duration::from_millis(1),
                Duration::from_millis(2),
                Duration::from_millis(4)
            ]
        );
    }
}
