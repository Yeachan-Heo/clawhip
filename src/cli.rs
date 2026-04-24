use std::io::Read;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use serde_json::Value;

use crate::events::MessageFormat;

pub const DEFAULT_RETRY_ENTER_COUNT: u32 = 4;
pub const DEFAULT_RETRY_ENTER_DELAY_MS: u64 = 250;
pub const DEFAULT_DELIVER_MAX_ENTERS: u32 = crate::hooks::prompt_deliver::DEFAULT_MAX_ENTERS;

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

    pub fn runtime_worker_threads(&self) -> Option<usize> {
        match self.command.as_ref() {
            Some(Commands::Start { worker_threads, .. }) => worker_threads.map(NonZeroUsize::get),
            _ => None,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Start the daemon (HTTP server + monitors + managed cron jobs).
    #[command(alias = "serve")]
    Start {
        #[arg(long)]
        port: Option<u16>,
        /// Override the Tokio worker thread count for the daemon runtime.
        #[arg(long)]
        worker_threads: Option<NonZeroUsize>,
    },
    /// Check daemon health/status.
    Status,
    #[command(
        about = "Scaffold common setup presets without editing advanced routes or monitors",
        long_about = "Scaffold the bounded quickstart preset catalog.\n\nAdvanced routes and monitors still require manual config editing or the bounded clawhip config editor."
    )]
    Setup(SetupArgs),
    /// Send a custom event to the local daemon.
    Send {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        message: String,
    },
    /// Deliver a prompt into an existing hooked tmux-backed Codex/Claude session (including OMC/OMX wrappers).
    Deliver(DeliverArgs),
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
    /// Send native provider hook events to the local daemon.
    Native {
        #[command(subcommand)]
        command: NativeCommands,
    },
    /// Run configured cron jobs via clawhip.
    Cron {
        #[command(subcommand)]
        command: CronCommands,
    },
    /// Install clawhip from the current git clone.
    Install {
        /// Install and start the bundled systemd service.
        #[arg(long, default_value_t = false)]
        systemd: bool,
        /// Disable the optional post-install GitHub star prompt.
        #[arg(long, default_value_t = false)]
        skip_star_prompt: bool,
    },
    /// Update clawhip from the current git clone.
    ///
    /// Without a subcommand, behaves like the legacy `clawhip update --restart`
    /// (pull + reinstall + optional restart). Use subcommands for daemon-aware
    /// operations: check, approve, dismiss, status.
    Update {
        #[command(subcommand)]
        command: Option<UpdateCommands>,
        /// Restart the systemd service after updating (legacy flag, used when no subcommand is given).
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
    /// Manage tool integration plugins.
    Plugin {
        #[command(subcommand)]
        command: PluginCommands,
    },
    /// Manage configuration.
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// Bootstrap and inspect filesystem-offloaded memory scaffolds.
    Memory {
        #[command(subcommand)]
        command: MemoryCommands,
    },
    /// Install and manage provider-native hook forwarding for Codex and Claude Code.
    Hooks {
        #[command(subcommand)]
        command: HooksCommands,
    },
    /// Explain how an event would be routed without actually dispatching it.
    ///
    /// Shows which routes match, which filters pass/fail, and where the
    /// event would be delivered — useful for debugging config.
    Explain(ExplainArgs),
    /// Release consistency checks.
    Release {
        #[command(subcommand)]
        command: ReleaseCommands,
    },
}

#[derive(Debug, Clone, Args)]
pub struct DeliverArgs {
    /// Existing tmux session name to target.
    #[arg(long)]
    pub session: String,
    /// Prompt text to submit into the active pane.
    #[arg(long)]
    pub prompt: String,
    /// Maximum Enter presses to attempt before failing.
    #[arg(long, default_value_t = DEFAULT_DELIVER_MAX_ENTERS)]
    pub max_enters: u32,
}

#[derive(Debug, Clone, Args)]
pub struct EmitArgs {
    pub event_type: String,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub fields: Vec<String>,
}

/// Arguments for `clawhip explain`.
///
/// Mirrors `EmitArgs` so operators can explain the exact same event shape
/// they would normally emit — with `--channel`, `--format`, `--payload` JSON,
/// and ad-hoc `--key value` fields.
#[derive(Debug, Clone, Args)]
pub struct ExplainArgs {
    /// Event type (canonical or alias, same as `clawhip emit`).
    pub event_type: String,
    /// Emit output as JSON instead of the human-readable text report.
    #[arg(long, default_value_t = false)]
    pub json: bool,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub fields: Vec<String>,
}

impl ExplainArgs {
    pub fn into_event(self) -> crate::Result<crate::events::IncomingEvent> {
        EmitArgs {
            event_type: self.event_type,
            fields: self.fields,
        }
        .into_event()
    }
}

#[derive(Debug, Clone, Default, Args)]
#[command(arg_required_else_help = true)]
pub struct SetupArgs {
    /// Set or update the canonical Discord webhook quickstart route.
    #[arg(long)]
    pub webhook: Option<String>,
    /// Set the Discord bot token in [providers.discord].
    #[arg(long = "bot-token")]
    pub bot_token: Option<String>,
    /// Set the default Discord channel in [defaults].
    #[arg(long = "default-channel")]
    pub default_channel: Option<String>,
    /// Set the default message format in [defaults].
    #[arg(long = "default-format")]
    pub default_format: Option<MessageFormat>,
    /// Set the daemon base URL in [daemon].
    #[arg(long = "daemon-base-url")]
    pub daemon_base_url: Option<String>,
    /// Bind a repo to a Discord channel ID. Format: `repo=channel_id`.
    ///
    /// Resolves the channel ID against the live Discord API, surfaces the
    /// live channel name, writes the resulting route with a `channel_name`
    /// hint for drift detection, and refuses if the channel can't be
    /// resolved (missing, forbidden, or no bot token). Repeatable.
    #[arg(long = "bind", value_name = "REPO=CHANNEL_ID")]
    pub bind: Vec<String>,
    /// When combined with `--bind`, refuse unless the live channel name
    /// matches the expected name. Format: `repo=expected_name`. Repeatable.
    #[arg(long = "expect-name", value_name = "REPO=NAME")]
    pub expect_name: Vec<String>,
    /// Verify all resulting channel bindings against live Discord state
    /// before writing the config. Fails the command if any binding drifts.
    #[arg(long = "verify-bindings", default_value_t = false)]
    pub verify_bindings: bool,
}

#[derive(Debug, Clone, Default, Args)]
pub struct VerifyBindingsArgs {
    /// Emit machine-readable JSON instead of the human-readable text report.
    #[arg(long, default_value_t = false)]
    pub json: bool,
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

#[derive(Debug, Clone, Subcommand)]
pub enum PluginCommands {
    List,
}

#[derive(Debug, Clone, Subcommand)]
pub enum NativeCommands {
    /// Forward a provider-native hook payload to clawhip.
    Hook(NativeHookArgs),
}

#[derive(Debug, Clone, Args)]
pub struct NativeHookArgs {
    /// Provider name (for example: claude-code or codex).
    #[arg(long)]
    pub provider: Option<String>,
    /// Source/tool name override. Defaults to provider when omitted.
    #[arg(long)]
    pub source: Option<String>,
    /// Provide the native hook JSON inline.
    #[arg(long)]
    pub payload: Option<String>,
    /// Read native hook JSON from a file. Use "-" or omit to read stdin.
    #[arg(long)]
    pub file: Option<PathBuf>,
}

#[cfg_attr(test, allow(dead_code))]
impl NativeHookArgs {
    pub fn read_payload(&self, stdin: &mut dyn Read) -> crate::Result<serde_json::Value> {
        match (&self.payload, &self.file) {
            (Some(_), Some(_)) => {
                Err("provide either --payload or --file for clawhip native hook, not both".into())
            }
            (Some(payload), None) => Ok(serde_json::from_str(payload)?),
            (None, Some(path)) => {
                if path.as_os_str() == "-" {
                    return Self::read_payload_from_stdin(stdin);
                }
                Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
            }
            (None, None) => Self::read_payload_from_stdin(stdin),
        }
    }

    fn read_payload_from_stdin(stdin: &mut dyn Read) -> crate::Result<serde_json::Value> {
        let mut buffer = String::new();
        stdin.read_to_string(&mut buffer)?;
        let trimmed = buffer.trim();
        if trimmed.is_empty() {
            return Err(
                "clawhip native hook expects a JSON payload via stdin, --payload, or --file".into(),
            );
        }
        Ok(serde_json::from_str(trimmed)?)
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum ReleaseCommands {
    /// Verify version/Cargo.lock/CHANGELOG consistency before tagging a release.
    ///
    /// If <VERSION> is omitted the current Cargo.toml version is used.
    /// Exits non-zero when any check fails.
    Preflight {
        /// Expected release version (e.g. 0.6.5, v0.6.5, refs/tags/v0.6.5).
        version: Option<String>,
        /// Path to the repository root. Defaults to the current directory.
        #[arg(long)]
        repo: Option<std::path::PathBuf>,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum CronCommands {
    /// Run one configured cron job immediately, which is useful for native system-cron entrypoints.
    Run {
        /// Cron job id from [[cron.jobs]].id.
        id: String,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum UpdateCommands {
    /// Check whether a newer release is available on GitHub.
    Check,
    /// Approve a pending update detected by the daemon.
    Approve,
    /// Dismiss (skip) a pending update without applying it.
    Dismiss,
    /// Show the current pending-update status from the daemon.
    Status,
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
    /// List active tmux watch registrations known to the daemon.
    List,
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
    /// Keep the wrapper process alive to monitor the session in-process
    /// (tighter 1 s polling). Without this flag the wrapper exits after
    /// successful launch and the daemon takes over monitoring.
    #[arg(long, default_value_t = false)]
    pub follow: bool,
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    pub retry_enter: bool,
    #[arg(long, default_value_t = DEFAULT_RETRY_ENTER_COUNT)]
    pub retry_enter_count: u32,
    #[arg(long, default_value_t = DEFAULT_RETRY_ENTER_DELAY_MS)]
    pub retry_enter_delay_ms: u64,
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

#[derive(Debug, Clone, Subcommand)]
pub enum MemoryCommands {
    /// Create a filesystem-offloaded memory scaffold in a repo or workspace.
    Init(MemoryInitArgs),
    /// Inspect whether a filesystem-offloaded memory scaffold is present.
    Status(MemoryStatusArgs),
}

#[derive(Debug, Clone, Args)]
pub struct MemoryInitArgs {
    /// Root directory where MEMORY.md and memory/ should live.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Stable project slug for memory/projects/<project>.md.
    #[arg(long)]
    pub project: Option<String>,
    /// Optional channel slug for memory/channels/<channel>.md.
    #[arg(long)]
    pub channel: Option<String>,
    /// Optional agent slug for memory/agents/<agent>.md.
    #[arg(long)]
    pub agent: Option<String>,
    /// Daily shard name to create under memory/daily/ (YYYY-MM-DD).
    #[arg(long)]
    pub date: Option<String>,
    /// Overwrite generated scaffold files when they already exist.
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

#[derive(Debug, Clone, Args)]
pub struct MemoryStatusArgs {
    /// Root directory where MEMORY.md and memory/ should live.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Stable project slug to inspect under memory/projects/<project>.md.
    #[arg(long)]
    pub project: Option<String>,
    /// Optional channel slug to inspect under memory/channels/<channel>.md.
    #[arg(long)]
    pub channel: Option<String>,
    /// Optional agent slug to inspect under memory/agents/<agent>.md.
    #[arg(long)]
    pub agent: Option<String>,
    /// Daily shard name to inspect under memory/daily/ (YYYY-MM-DD).
    #[arg(long)]
    pub date: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HookProvider {
    Codex,
    #[value(name = "claude-code", alias = "claude")]
    ClaudeCode,
}

impl HookProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HookInstallScope {
    Project,
    Global,
}

#[derive(Debug, Clone, Subcommand)]
pub enum HooksCommands {
    /// Install provider-native hook forwarding for Codex and/or Claude Code.
    Install(HooksInstallArgs),
}

#[derive(Debug, Clone, Args)]
pub struct HooksInstallArgs {
    /// Install all supported providers.
    #[arg(long, default_value_t = false)]
    pub all: bool,
    /// Install only the selected provider(s). Repeat to install multiple.
    #[arg(long, value_enum, action = ArgAction::Append)]
    pub provider: Vec<HookProvider>,
    /// Install into the provider's supported hook config location(s): Codex supports project or global hooks.json; Claude Code is global-only.
    #[arg(long, value_enum, default_value_t = HookInstallScope::Global)]
    pub scope: HookInstallScope,
    /// Project root for project-scoped Codex install. Ignored for global installs.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Overwrite clawhip-managed generated files when they already exist.
    #[arg(long, default_value_t = false)]
    pub force: bool,
}

#[derive(Debug, Clone, Default, Subcommand)]
pub enum ConfigCommand {
    /// Edit the five common setup presets interactively; advanced routes and monitors remain manual-edit territory.
    #[default]
    Interactive,
    /// Print the current config file.
    Show,
    /// Print the active config file path.
    Path,
    /// Verify all channel bindings in the config against live Discord server state.
    ///
    /// Walks routes, defaults, and monitors to collect every channel ID reference,
    /// then queries the Discord API to confirm each channel exists and (optionally)
    /// matches the `channel_name` hint set alongside the ID.
    VerifyBindings(VerifyBindingsArgs),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::compat::from_incoming_event;
    use clap::CommandFactory;
    use clap::error::ErrorKind;

    #[test]
    fn parses_start_subcommand_with_worker_threads_override() {
        let cli = Cli::parse_from(["clawhip", "start", "--worker-threads", "2"]);

        let Commands::Start {
            port,
            worker_threads,
        } = cli.command.expect("start command")
        else {
            panic!("expected start command");
        };

        assert_eq!(port, None);
        assert_eq!(worker_threads, Some(NonZeroUsize::new(2).unwrap()));
    }

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
    fn parses_deliver_subcommand() {
        let cli = Cli::parse_from([
            "clawhip",
            "deliver",
            "--session",
            "issue-184",
            "--prompt",
            "Ship it",
            "--max-enters",
            "6",
        ]);

        let Commands::Deliver(args) = cli.command.expect("deliver command") else {
            panic!("expected deliver command");
        };

        assert_eq!(args.session, "issue-184");
        assert_eq!(args.prompt, "Ship it");
        assert_eq!(args.max_enters, 6);
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
    fn emit_agent_lifecycle_events_normalize_for_validation() {
        let args = EmitArgs {
            event_type: "agent.started".into(),
            fields: vec![
                "--agent".into(),
                "omx".into(),
                "--session".into(),
                "issue-65".into(),
                "--project".into(),
                "clawhip".into(),
            ],
        };

        let normalized = crate::events::normalize_event(args.into_event().expect("event"));
        let typed = from_incoming_event(&normalized).expect("typed envelope");

        assert_eq!(normalized.kind, "agent.started");
        assert_eq!(
            normalized.payload["status"],
            Value::String("started".into())
        );
        assert_eq!(normalized.payload["tool"], Value::String("omx".into()));
        assert_eq!(typed.metadata.priority, crate::event::EventPriority::Normal);
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
    fn parses_tmux_list_subcommand() {
        let cli = Cli::parse_from(["clawhip", "tmux", "list"]);

        let Commands::Tmux { command } = cli.command.expect("tmux command") else {
            panic!("expected tmux command");
        };

        assert!(matches!(command, TmuxCommands::List));
    }

    #[test]
    fn parses_setup_bind_subcommand() {
        let cli = Cli::parse_from([
            "clawhip",
            "setup",
            "--bind",
            "clawhip=1480171113253175356",
            "--bind",
            "oh-my-codex=1480171106324189335",
            "--expect-name",
            "clawhip=clawhip-dev",
        ]);
        let Commands::Setup(args) = cli.command.expect("setup command") else {
            panic!("expected Setup");
        };
        assert_eq!(args.bind.len(), 2);
        assert_eq!(args.bind[0], "clawhip=1480171113253175356");
        assert_eq!(args.bind[1], "oh-my-codex=1480171106324189335");
        assert_eq!(args.expect_name.len(), 1);
        assert_eq!(args.expect_name[0], "clawhip=clawhip-dev");
    }

    #[test]
    fn parses_config_verify_bindings_subcommand() {
        let cli = Cli::parse_from(["clawhip", "config", "verify-bindings", "--json"]);
        let Some(Commands::Config { command }) = cli.command else {
            panic!("expected Config");
        };
        let Some(ConfigCommand::VerifyBindings(args)) = command else {
            panic!("expected VerifyBindings");
        };
        assert!(args.json);
    }

    #[test]
    fn parses_config_verify_bindings_text_default() {
        let cli = Cli::parse_from(["clawhip", "config", "verify-bindings"]);
        let Some(Commands::Config {
            command: Some(ConfigCommand::VerifyBindings(args)),
        }) = cli.command
        else {
            panic!("expected verify-bindings");
        };
        assert!(!args.json);
    }

    #[test]
    fn parses_setup_webhook_subcommand() {
        let cli = Cli::parse_from([
            "clawhip",
            "setup",
            "--webhook",
            "https://discord.com/api/webhooks/123/abc",
        ]);

        let Commands::Setup(args) = cli.command.expect("setup command") else {
            panic!("expected setup command");
        };

        assert_eq!(
            args.webhook.as_deref(),
            Some("https://discord.com/api/webhooks/123/abc")
        );
        assert!(args.bot_token.is_none());
    }

    #[test]
    fn parses_setup_mixed_flag_subcommand() {
        let cli = Cli::parse_from([
            "clawhip",
            "setup",
            "--webhook",
            "https://discord.com/api/webhooks/123/abc",
            "--bot-token",
            "discord-token",
            "--default-channel",
            "alerts",
            "--default-format",
            "alert",
            "--daemon-base-url",
            "http://127.0.0.1:31337",
        ]);

        let Commands::Setup(args) = cli.command.expect("setup command") else {
            panic!("expected setup command");
        };

        assert_eq!(
            args.webhook.as_deref(),
            Some("https://discord.com/api/webhooks/123/abc")
        );
        assert_eq!(args.bot_token.as_deref(), Some("discord-token"));
        assert_eq!(args.default_channel.as_deref(), Some("alerts"));
        assert_eq!(args.default_format, Some(MessageFormat::Alert));
        assert_eq!(
            args.daemon_base_url.as_deref(),
            Some("http://127.0.0.1:31337")
        );
    }

    #[test]
    fn setup_without_flags_fails_with_help() {
        let error = Cli::try_parse_from(["clawhip", "setup"]).expect_err("setup should fail");
        assert_eq!(
            error.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );

        let rendered = error.to_string();
        assert!(rendered.contains("Usage: clawhip setup [OPTIONS]"));
        assert!(rendered.contains("--webhook"));
        assert!(rendered.contains("--bot-token"));
    }

    #[test]
    fn setup_help_mentions_manual_advanced_editing() {
        let mut command = Cli::command();
        let setup = command
            .find_subcommand_mut("setup")
            .expect("setup subcommand");
        let mut buffer = Vec::new();
        setup.write_long_help(&mut buffer).expect("write help");
        let help = String::from_utf8(buffer).expect("utf8");

        assert!(help.contains("Advanced routes and monitors still require manual config editing"));
        assert!(help.contains("--default-format <DEFAULT_FORMAT>"));
        assert!(help.contains("--daemon-base-url <DAEMON_BASE_URL>"));
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
        assert_eq!(args.retry_enter_count, DEFAULT_RETRY_ENTER_COUNT);
        assert_eq!(args.retry_enter_delay_ms, DEFAULT_RETRY_ENTER_DELAY_MS);
        assert_eq!(args.command, vec!["codex"]);
    }

    #[test]
    fn parses_tmux_new_with_retry_enter_backoff_overrides() {
        let cli = Cli::parse_from([
            "clawhip",
            "tmux",
            "new",
            "-s",
            "issue-22",
            "--retry-enter-count",
            "6",
            "--retry-enter-delay-ms",
            "400",
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
        assert!(args.retry_enter);
        assert_eq!(args.retry_enter_count, 6);
        assert_eq!(args.retry_enter_delay_ms, 400);
        assert_eq!(args.command, vec!["codex"]);
    }

    #[test]
    fn tmux_new_defaults_to_non_follow_mode_for_194() {
        // Regression for #194: the default launcher path MUST return control
        // to the caller after the session is created. If `follow` defaulted
        // back to true, `clawhip tmux new` would once again block for the
        // session lifetime and expose callers to false-negative SIGKILL.
        let cli = Cli::parse_from(["clawhip", "tmux", "new", "-s", "issue-194", "--", "codex"]);

        let Commands::Tmux { command } = cli.command.expect("tmux command") else {
            panic!("expected tmux command");
        };
        let TmuxCommands::New(args) = command else {
            panic!("expected tmux new command");
        };

        assert!(!args.follow, "follow must default to false after #194");
    }

    #[test]
    fn parses_tmux_new_with_explicit_follow_flag() {
        let cli = Cli::parse_from([
            "clawhip",
            "tmux",
            "new",
            "-s",
            "issue-194",
            "--follow",
            "--",
            "codex",
        ]);

        let Commands::Tmux { command } = cli.command.expect("tmux command") else {
            panic!("expected tmux command");
        };
        let TmuxCommands::New(args) = command else {
            panic!("expected tmux new command");
        };

        assert!(args.follow);
    }

    #[test]
    fn parses_plugin_list_subcommand() {
        let cli = Cli::parse_from(["clawhip", "plugin", "list"]);

        let Commands::Plugin { command } = cli.command.expect("plugin command") else {
            panic!("expected plugin command");
        };

        assert!(matches!(command, PluginCommands::List));
    }

    #[test]
    fn parses_native_hook_subcommand() {
        let cli = Cli::parse_from([
            "clawhip",
            "native",
            "hook",
            "--provider",
            "codex",
            "--file",
            "payload.json",
        ]);

        let Commands::Native { command } = cli.command.expect("native command") else {
            panic!("expected native command");
        };

        let NativeCommands::Hook(args) = command;

        assert_eq!(args.provider.as_deref(), Some("codex"));
        assert_eq!(
            args.file.as_deref(),
            Some(PathBuf::from("payload.json").as_path())
        );
    }

    #[test]
    fn parses_cron_run_subcommand() {
        let cli = Cli::parse_from(["clawhip", "cron", "run", "dev-followup"]);

        let Commands::Cron { command } = cli.command.expect("cron command") else {
            panic!("expected cron command");
        };
        let CronCommands::Run { id } = command;

        assert_eq!(id, "dev-followup");
    }

    #[test]
    fn native_hook_args_read_payload_from_inline_json() {
        let args = NativeHookArgs {
            provider: None,
            source: None,
            payload: Some(r#"{"event_name":"SessionStart"}"#.into()),
            file: None,
        };

        let payload = args
            .read_payload(&mut std::io::Cursor::new(Vec::<u8>::new()))
            .expect("inline json payload");

        assert_eq!(payload["event_name"], serde_json::json!("SessionStart"));
    }

    #[test]
    fn native_hook_args_reject_empty_input() {
        let args = NativeHookArgs {
            provider: None,
            source: None,
            payload: None,
            file: None,
        };

        let error = args
            .read_payload(&mut std::io::Cursor::new(Vec::<u8>::new()))
            .expect_err("empty stdin should fail");

        assert!(
            error
                .to_string()
                .contains("clawhip native hook expects a JSON payload")
        );
    }

    #[test]
    fn parses_memory_init_subcommand() {
        let cli = Cli::parse_from([
            "clawhip",
            "memory",
            "init",
            "--root",
            "/tmp/workspace",
            "--project",
            "clawhip",
            "--channel",
            "discord-alerts",
            "--agent",
            "codex",
            "--date",
            "2026-03-10",
            "--force",
        ]);

        let Commands::Memory { command } = cli.command.expect("memory command") else {
            panic!("expected memory command");
        };

        let MemoryCommands::Init(args) = command else {
            panic!("expected memory init command");
        };

        assert_eq!(args.root, Some(PathBuf::from("/tmp/workspace")));
        assert_eq!(args.project.as_deref(), Some("clawhip"));
        assert_eq!(args.channel.as_deref(), Some("discord-alerts"));
        assert_eq!(args.agent.as_deref(), Some("codex"));
        assert_eq!(args.date.as_deref(), Some("2026-03-10"));
        assert!(args.force);
    }

    #[test]
    fn parses_memory_status_subcommand() {
        let cli = Cli::parse_from([
            "clawhip",
            "memory",
            "status",
            "--root",
            "/tmp/workspace",
            "--project",
            "clawhip",
            "--agent",
            "codex",
        ]);

        let Commands::Memory { command } = cli.command.expect("memory command") else {
            panic!("expected memory command");
        };

        let MemoryCommands::Status(args) = command else {
            panic!("expected memory status command");
        };

        assert_eq!(args.root, Some(PathBuf::from("/tmp/workspace")));
        assert_eq!(args.project.as_deref(), Some("clawhip"));
        assert_eq!(args.channel, None);
        assert_eq!(args.agent.as_deref(), Some("codex"));
        assert_eq!(args.date, None);
    }

    #[test]
    fn parses_install_subcommand_with_skip_star_prompt() {
        let cli = Cli::parse_from(["clawhip", "install", "--systemd", "--skip-star-prompt"]);

        let Commands::Install {
            systemd,
            skip_star_prompt,
        } = cli.command.expect("install command")
        else {
            panic!("expected install command");
        };

        assert!(systemd);
        assert!(skip_star_prompt);
    }

    #[test]
    fn parses_hooks_install_subcommand() {
        let cli = Cli::parse_from([
            "clawhip",
            "hooks",
            "install",
            "--provider",
            "codex",
            "--provider",
            "claude-code",
            "--scope",
            "project",
            "--root",
            "/tmp/repo",
        ]);

        let Commands::Hooks { command } = cli.command.expect("hooks command") else {
            panic!("expected hooks command");
        };

        let HooksCommands::Install(args) = command;

        assert_eq!(
            args.provider,
            vec![HookProvider::Codex, HookProvider::ClaudeCode]
        );
        assert_eq!(args.scope, HookInstallScope::Project);
        assert_eq!(args.root, Some(PathBuf::from("/tmp/repo")));
        assert!(!args.all);
    }

    #[test]
    fn parses_hooks_install_all_flag() {
        let cli = Cli::parse_from(["clawhip", "hooks", "install", "--all"]);

        let Commands::Hooks { command } = cli.command.expect("hooks command") else {
            panic!("expected hooks command");
        };

        let HooksCommands::Install(args) = command;

        assert!(args.provider.is_empty());
        assert!(args.all);
    }

    #[test]
    fn parses_hooks_install_with_global_scope_and_force() {
        let cli = Cli::parse_from([
            "clawhip",
            "hooks",
            "install",
            "--provider",
            "claude",
            "--scope",
            "global",
            "--force",
        ]);

        let Commands::Hooks { command } = cli.command.expect("hooks command") else {
            panic!("expected hooks command");
        };

        let HooksCommands::Install(args) = command;

        assert_eq!(args.provider, vec![HookProvider::ClaudeCode]);
        assert_eq!(args.scope, HookInstallScope::Global);
        assert!(args.force);
    }

    #[test]
    fn bare_update_preserves_legacy_restart_flag() {
        let cli = Cli::parse_from(["clawhip", "update", "--restart"]);

        let Commands::Update { command, restart } = cli.command.expect("update command") else {
            panic!("expected update command");
        };

        assert!(command.is_none());
        assert!(restart);
    }

    #[test]
    fn bare_update_without_restart_defaults_to_false() {
        let cli = Cli::parse_from(["clawhip", "update"]);

        let Commands::Update { command, restart } = cli.command.expect("update command") else {
            panic!("expected update command");
        };

        assert!(command.is_none());
        assert!(!restart);
    }

    #[test]
    fn parses_update_check_subcommand() {
        let cli = Cli::parse_from(["clawhip", "update", "check"]);

        let Commands::Update { command, .. } = cli.command.expect("update command") else {
            panic!("expected update command");
        };

        assert!(matches!(command, Some(UpdateCommands::Check)));
    }

    #[test]
    fn parses_update_approve_subcommand() {
        let cli = Cli::parse_from(["clawhip", "update", "approve"]);

        let Commands::Update { command, .. } = cli.command.expect("update command") else {
            panic!("expected update command");
        };

        assert!(matches!(command, Some(UpdateCommands::Approve)));
    }

    #[test]
    fn parses_update_dismiss_subcommand() {
        let cli = Cli::parse_from(["clawhip", "update", "dismiss"]);

        let Commands::Update { command, .. } = cli.command.expect("update command") else {
            panic!("expected update command");
        };

        assert!(matches!(command, Some(UpdateCommands::Dismiss)));
    }

    #[test]
    fn parses_update_status_subcommand() {
        let cli = Cli::parse_from(["clawhip", "update", "status"]);

        let Commands::Update { command, .. } = cli.command.expect("update command") else {
            panic!("expected update command");
        };

        assert!(matches!(command, Some(UpdateCommands::Status)));
    }
}
