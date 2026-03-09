use std::path::PathBuf;

use serde_json::Value;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

use crate::events::MessageFormat;

#[derive(Debug, Parser)]
#[command(
    name = "clawhip",
    version,
    about = "Daemon-first event gateway for Discord"
)]
pub struct Cli {
    /// Override the config file path.
    #[arg(long, global = true, env = "CLAWHIP_CONFIG")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

impl Cli {
    pub fn config_path(&self) -> PathBuf {
        self.config
            .clone()
            .unwrap_or_else(crate::config::default_config_path)
    }
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Start the daemon (HTTP server + git/tmux monitors).
    #[command(alias = "serve")]
    Start {
        #[arg(long)]
        port: Option<u16>,
    },
    /// Check daemon health/status.
    Status,
    /// Send a custom event to the local daemon.
    Send {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        message: String,
    },
    /// Emit an arbitrary event to the local daemon.
    Emit(EmitArgs),
    /// Send git-related events to the local daemon.
    Git {
        #[command(subcommand)]
        command: GitCommands,
    },
    /// Send GitHub-related events to the local daemon.
    Github {
        #[command(subcommand)]
        command: GithubCommands,
    },
    /// Send agent lifecycle events to the local daemon.
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },
    /// Send tmux-related events to the local daemon or launch/register tmux sessions.
    Tmux {
        #[command(subcommand)]
        command: TmuxCommands,
    },
    /// Install clawhip from the current git clone.
    Install {
        #[arg(long, default_value_t = false)]
        systemd: bool,
    },
    /// Update clawhip from the current git clone.
    Update {
        #[arg(long, default_value_t = false)]
        restart: bool,
    },
    /// Uninstall clawhip.
    Uninstall {
        #[arg(long, default_value_t = false)]
        remove_systemd: bool,
        #[arg(long, default_value_t = false)]
        remove_config: bool,
    },
    /// Manage configuration.
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
}

#[derive(Debug, Clone, Args)]
pub struct EmitArgs {
    pub event_type: String,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub fields: Vec<String>,
}

impl EmitArgs {
    pub fn into_event(self) -> crate::Result<crate::events::IncomingEvent> {
        let mut channel = None;
        let mut mention = None;
        let mut format = None;
        let mut template = None;
        let mut payload = None;
        let mut payload_map = serde_json::Map::new();

        if !self.fields.len().is_multiple_of(2) {
            return Err("emit fields must be provided as --key value pairs".into());
        }

        for pair in self.fields.chunks_exact(2) {
            let key = pair[0]
                .strip_prefix("--")
                .ok_or_else(|| format!("emit field names must start with --, got {}", pair[0]))?;
            let key = normalize_emit_key(key);
            let raw_value = pair[1].clone();
            match key {
                "channel" => channel = Some(raw_value),
                "mention" => mention = Some(raw_value),
                "format" => format = Some(MessageFormat::from_label(&raw_value)?),
                "template" => template = Some(raw_value),
                "payload" => payload = Some(serde_json::from_str::<Value>(&raw_value)?),
                _ => {
                    payload_map.insert(key.to_string(), parse_emit_value(&raw_value));
                }
            }
        }

        let payload = match payload {
            Some(Value::Object(mut object)) => {
                object.extend(payload_map);
                Value::Object(object)
            }
            Some(other) => other,
            None => Value::Object(payload_map),
        };

        Ok(crate::events::IncomingEvent {
            kind: self.event_type,
            channel,
            mention,
            format,
            template,
            payload,
        })
    }
}

fn normalize_emit_key(key: &str) -> &str {
    match key {
        "agent" => "agent_name",
        "session" => "session_id",
        "elapsed" => "elapsed_secs",
        "error" => "error_message",
        other => other,
    }
}

fn parse_emit_value(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

#[derive(Debug, Subcommand)]
pub enum GitCommands {
    Commit {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        branch: String,
        #[arg(long)]
        commit: String,
        #[arg(long)]
        summary: String,
        #[arg(long)]
        channel: Option<String>,
    },
    BranchChanged {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        old_branch: String,
        #[arg(long)]
        new_branch: String,
        #[arg(long)]
        channel: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum GithubCommands {
    IssueOpened {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        number: u64,
        #[arg(long)]
        title: String,
        #[arg(long)]
        channel: Option<String>,
    },
    PrStatusChanged {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        number: u64,
        #[arg(long)]
        title: String,
        #[arg(long)]
        old_status: String,
        #[arg(long)]
        new_status: String,
        #[arg(long, default_value = "")]
        url: String,
        #[arg(long)]
        channel: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum AgentCommands {
    Started(AgentEventArgs),
    Blocked(AgentEventArgs),
    Finished(AgentEventArgs),
    Failed(AgentFailedArgs),
}

#[derive(Debug, Clone, Args)]
pub struct AgentEventArgs {
    #[arg(long = "name")]
    pub agent_name: String,
    #[arg(long = "session")]
    pub session_id: Option<String>,
    #[arg(long)]
    pub project: Option<String>,
    #[arg(long = "elapsed")]
    pub elapsed_secs: Option<u64>,
    #[arg(long)]
    pub summary: Option<String>,
    #[arg(long)]
    pub mention: Option<String>,
    #[arg(long)]
    pub channel: Option<String>,
}

#[derive(Debug, Clone, Args)]
pub struct AgentFailedArgs {
    #[command(flatten)]
    pub event: AgentEventArgs,
    #[arg(long = "error")]
    pub error_message: String,
}

#[derive(Debug, Subcommand)]
pub enum TmuxCommands {
    Keyword {
        #[arg(long)]
        session: String,
        #[arg(long)]
        keyword: String,
        #[arg(long)]
        line: String,
        #[arg(long)]
        channel: Option<String>,
    },
    Stale {
        #[arg(long)]
        session: String,
        #[arg(long)]
        pane: String,
        #[arg(long)]
        minutes: u64,
        #[arg(long)]
        last_line: String,
        #[arg(long)]
        channel: Option<String>,
    },
    New(TmuxNewArgs),
    Watch(TmuxWatchArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum TmuxWrapperFormat {
    Compact,
    Alert,
    Inline,
}

impl From<TmuxWrapperFormat> for MessageFormat {
    fn from(value: TmuxWrapperFormat) -> Self {
        match value {
            TmuxWrapperFormat::Compact => MessageFormat::Compact,
            TmuxWrapperFormat::Alert => MessageFormat::Alert,
            TmuxWrapperFormat::Inline => MessageFormat::Inline,
        }
    }
}

#[derive(Debug, Clone, Args)]
pub struct TmuxNewArgs {
    #[arg(short = 's', long = "session")]
    pub session: String,
    #[arg(short = 'n', long = "window-name")]
    pub window_name: Option<String>,
    #[arg(short = 'c', long = "cwd")]
    pub cwd: Option<String>,
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub mention: Option<String>,
    #[arg(long, value_delimiter = ',')]
    pub keywords: Vec<String>,
    #[arg(long, default_value_t = 10)]
    pub stale_minutes: u64,
    #[arg(long)]
    pub format: Option<TmuxWrapperFormat>,
    #[arg(long, default_value_t = false)]
    pub attach: bool,
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    pub retry_enter: bool,
    #[arg(long)]
    pub shell: Option<String>,
    #[arg(last = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct TmuxWatchArgs {
    #[arg(short = 's', long = "session")]
    pub session: String,
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub mention: Option<String>,
    #[arg(long, value_delimiter = ',')]
    pub keywords: Vec<String>,
    #[arg(long, default_value_t = 10)]
    pub stale_minutes: u64,
    #[arg(long)]
    pub format: Option<TmuxWrapperFormat>,
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    pub retry_enter: bool,
}

#[derive(Debug, Clone, Default, Subcommand)]
pub enum ConfigCommand {
    #[default]
    Interactive,
    Show,
    Path,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_emit_subcommand_with_top_level_fields() {
        let cli = Cli::parse_from([
            "clawhip",
            "emit",
            "agent.started",
            "--channel",
            "alerts",
            "--mention",
            "<@123>",
            "--format",
            "alert",
            "--template",
            "agent {agent_name}",
            "--agent",
            "omc",
            "--elapsed",
            "17",
        ]);

        let Commands::Emit(args) = cli.command.expect("emit command") else {
            panic!("expected emit command");
        };

        let event = args.into_event().expect("event");
        assert_eq!(event.kind, "agent.started");
        assert_eq!(event.channel.as_deref(), Some("alerts"));
        assert_eq!(event.mention.as_deref(), Some("<@123>"));
        assert!(matches!(event.format, Some(MessageFormat::Alert)));
        assert_eq!(event.template.as_deref(), Some("agent {agent_name}"));
        assert_eq!(event.payload["agent_name"], Value::String("omc".into()));
        assert_eq!(event.payload["elapsed_secs"], Value::from(17));
    }

    #[test]
    fn emit_args_merge_payload_json_with_extra_fields() {
        let args = EmitArgs {
            event_type: "agent.failed".into(),
            fields: vec![
                "--payload".into(),
                r#"{"session":"sess-1","ok":true}"#.into(),
                "--error".into(),
                "boom".into(),
            ],
        };

        let event = args.into_event().expect("event");
        assert_eq!(event.payload["session"], Value::String("sess-1".into()));
        assert_eq!(event.payload["ok"], Value::Bool(true));
        assert_eq!(event.payload["error_message"], Value::String("boom".into()));
    }

    #[test]
    fn emit_args_reject_invalid_format() {
        let args = EmitArgs {
            event_type: "agent.started".into(),
            fields: vec!["--format".into(), "loud".into()],
        };

        let error = args.into_event().expect_err("invalid format should fail");
        assert!(error.to_string().contains("unsupported message format"));
    }

    #[test]
    fn emit_args_reject_invalid_field_shape() {
        let args = EmitArgs {
            event_type: "agent.started".into(),
            fields: vec!["agent".into(), "omc".into(), "--session".into()],
        };

        let error = args.into_event().expect_err("invalid fields should fail");
        assert!(
            error
                .to_string()
                .contains("emit fields must be provided as --key value pairs")
        );
    }

    #[test]
    fn parses_agent_finished_subcommand() {
        let cli = Cli::parse_from([
            "clawhip",
            "agent",
            "finished",
            "--name",
            "worker-1",
            "--session",
            "sess-123",
            "--project",
            "my-repo",
            "--elapsed",
            "300",
            "--summary",
            "PR created",
        ]);

        let Commands::Agent { command } = cli.command.expect("agent command") else {
            panic!("expected agent command");
        };

        let AgentCommands::Finished(args) = command else {
            panic!("expected agent finished command");
        };

        assert_eq!(args.agent_name, "worker-1");
        assert_eq!(args.session_id.as_deref(), Some("sess-123"));
        assert_eq!(args.project.as_deref(), Some("my-repo"));
        assert_eq!(args.elapsed_secs, Some(300));
        assert_eq!(args.summary.as_deref(), Some("PR created"));
    }

    #[test]
    fn parses_agent_failed_subcommand() {
        let cli = Cli::parse_from([
            "clawhip",
            "agent",
            "failed",
            "--name",
            "worker-1",
            "--session",
            "sess-123",
            "--project",
            "my-repo",
            "--elapsed",
            "17",
            "--summary",
            "after test run",
            "--error",
            "build failed",
            "--mention",
            "<@123>",
            "--channel",
            "alerts",
        ]);

        let Commands::Agent { command } = cli.command.expect("agent command") else {
            panic!("expected agent command");
        };

        let AgentCommands::Failed(args) = command else {
            panic!("expected agent failed command");
        };

        assert_eq!(args.event.agent_name, "worker-1");
        assert_eq!(args.event.session_id.as_deref(), Some("sess-123"));
        assert_eq!(args.event.project.as_deref(), Some("my-repo"));
        assert_eq!(args.event.elapsed_secs, Some(17));
        assert_eq!(args.event.summary.as_deref(), Some("after test run"));
        assert_eq!(args.event.mention.as_deref(), Some("<@123>"));
        assert_eq!(args.event.channel.as_deref(), Some("alerts"));
        assert_eq!(args.error_message, "build failed");
    }

    #[test]
    fn parses_tmux_watch_subcommand() {
        let cli = Cli::parse_from([
            "clawhip",
            "tmux",
            "watch",
            "-s",
            "issue-13",
            "--channel",
            "alerts",
            "--mention",
            "<@123>",
            "--keywords",
            "error,complete",
            "--stale-minutes",
            "15",
            "--format",
            "alert",
        ]);

        let Commands::Tmux { command } = cli.command.expect("tmux command") else {
            panic!("expected tmux command");
        };

        let TmuxCommands::Watch(args) = command else {
            panic!("expected tmux watch command");
        };

        assert_eq!(args.session, "issue-13");
        assert_eq!(args.channel.as_deref(), Some("alerts"));
        assert_eq!(args.mention.as_deref(), Some("<@123>"));
        assert_eq!(args.keywords, vec!["error", "complete"]);
        assert_eq!(args.stale_minutes, 15);
        assert!(args.retry_enter);
        assert!(matches!(args.format, Some(TmuxWrapperFormat::Alert)));
    }

    #[test]
    fn parses_tmux_new_with_retry_enter_disabled() {
        let cli = Cli::parse_from([
            "clawhip",
            "tmux",
            "new",
            "-s",
            "issue-22",
            "--retry-enter=false",
            "--",
            "codex",
        ]);

        let Commands::Tmux { command } = cli.command.expect("tmux command") else {
            panic!("expected tmux command");
        };

        let TmuxCommands::New(args) = command else {
            panic!("expected tmux new command");
        };

        assert_eq!(args.session, "issue-22");
        assert!(!args.retry_enter);
        assert_eq!(args.command, vec!["codex"]);
    }
}
