use std::time::Duration;

use tokio::process::Command;
use tokio::time::sleep;

use crate::Result;
use crate::cli::{TmuxNewArgs, TmuxWatchArgs, TmuxWrapperFormat};
use crate::client::DaemonClient;
use crate::config::AppConfig;
use crate::source::tmux::{
    RegisteredTmuxSession, content_hash, monitor_registered_session, session_exists, tmux_bin,
};

pub async fn run(args: TmuxNewArgs, config: &AppConfig) -> Result<()> {
    launch_session(&args).await?;
    let monitor_args = TmuxMonitorArgs::from(&args);
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
    keywords: Vec<String>,
    keyword_window_secs: u64,
    stale_minutes: u64,
    format: Option<TmuxWrapperFormat>,
}

impl From<&TmuxNewArgs> for TmuxMonitorArgs {
    fn from(value: &TmuxNewArgs) -> Self {
        Self {
            session: value.session.clone(),
            channel: value.channel.clone(),
            mention: value.mention.clone(),
            keywords: value.keywords.clone(),
            keyword_window_secs: default_keyword_window_secs(),
            stale_minutes: value.stale_minutes,
            format: value.format,
        }
    }
}

impl From<&TmuxWatchArgs> for TmuxMonitorArgs {
    fn from(value: &TmuxWatchArgs) -> Self {
        Self {
            session: value.session.clone(),
            channel: value.channel.clone(),
            mention: value.mention.clone(),
            keywords: value.keywords.clone(),
            keyword_window_secs: default_keyword_window_secs(),
            stale_minutes: value.stale_minutes,
            format: value.format,
        }
    }
}

impl From<TmuxMonitorArgs> for RegisteredTmuxSession {
    fn from(value: TmuxMonitorArgs) -> Self {
        Self {
            session: value.session,
            channel: value.channel,
            mention: value.mention,
            keywords: value.keywords,
            keyword_window_secs: value.keyword_window_secs,
            stale_minutes: value.stale_minutes,
            format: value.format.map(Into::into),
            active_wrapper_monitor: true,
        }
    }
}

async fn register_and_start_monitor(
    args: TmuxMonitorArgs,
    config: &AppConfig,
) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let client = DaemonClient::from_config(config);
    let registration: RegisteredTmuxSession = args.into();
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

#[cfg(test)]
mod tests {
    use super::*;

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
            monitor_args.format,
            Some(TmuxWrapperFormat::Inline)
        ));
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
