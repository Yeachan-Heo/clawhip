mod binding_verify;
mod cli;
mod client;
mod config;
mod core;
mod cron;
mod daemon;
mod discord;
mod dispatch;
mod dynamic_tokens;
mod event;
mod events;
mod hooks;
mod keyword_window;
mod lifecycle;
mod memory;
mod native_hooks;
mod plugins;
mod provenance;
mod release_preflight;
mod render;
mod router;
mod sink;
mod slack;
mod source;
mod telemetry;
mod tmux_wrapper;
mod update;

use std::sync::Arc;

use clap::Parser;
use tokio::runtime::Builder;

use crate::cli::{
    AgentCommands, Cli, Commands, ConfigCommand, CronCommands, ExplainArgs, GitCommands,
    GithubCommands, HooksCommands, MemoryCommands, NativeCommands, PluginCommands, ReleaseCommands,
    SetupArgs, TmuxCommands, UpdateCommands, VerifyBindingsArgs,
};
use crate::client::DaemonClient;
use crate::config::{AppConfig, SetupEdits};
use crate::discord::DiscordClient;
use crate::event::compat::from_incoming_event;
use crate::events::IncomingEvent;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub type DynError = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, DynError>;

fn main() {
    let cli = Cli::parse();
    let runtime = match build_runtime(&cli) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("clawhip error: {error}");
            std::process::exit(1);
        }
    };

    if let Err(error) = runtime.block_on(real_main(cli)) {
        eprintln!("clawhip error: {error}");
        std::process::exit(1);
    }
}

fn build_runtime(cli: &Cli) -> Result<tokio::runtime::Runtime> {
    let mut builder = Builder::new_multi_thread();
    builder.enable_all();
    if let Some(worker_threads) = cli.runtime_worker_threads() {
        builder.worker_threads(worker_threads);
    }
    Ok(builder.build()?)
}

fn prepare_event(event: IncomingEvent) -> Result<IncomingEvent> {
    let event = crate::events::normalize_event(event);
    let _typed = from_incoming_event(&event)?;
    Ok(event)
}

async fn real_main(cli: Cli) -> Result<()> {
    let config_path = cli.config_path();
    let config = Arc::new(AppConfig::load_or_default(&config_path)?);
    let cron_state_path = crate::cron::default_state_path(&config_path);

    match cli.command.unwrap_or(Commands::Start {
        port: None,
        worker_threads: None,
    }) {
        Commands::Start { port, .. } => daemon::run(config, port, cron_state_path).await,
        Commands::Status => {
            let client = DaemonClient::from_config(config.as_ref());
            let health = client.health().await?;
            println!("{}", serde_json::to_string_pretty(&health)?);
            Ok(())
        }
        Commands::Deliver(args) => crate::hooks::prompt_deliver::run(args).await,
        Commands::Emit(args) => {
            let client = DaemonClient::from_config(config.as_ref());
            send_incoming_event(&client, args.into_event()?).await
        }
        Commands::Explain(args) => run_explain(config.as_ref(), args),
        Commands::Setup(args) => run_setup(args, &config_path).await,
        Commands::Send { channel, message } => {
            let client = DaemonClient::from_config(config.as_ref());
            send_incoming_event(&client, IncomingEvent::custom(channel, message)).await
        }
        Commands::Git { command } => {
            let client = DaemonClient::from_config(config.as_ref());
            let event = match command {
                GitCommands::Commit {
                    repo,
                    branch,
                    commit,
                    summary,
                    channel,
                } => IncomingEvent::git_commit(repo, branch, commit, summary, channel),
                GitCommands::BranchChanged {
                    repo,
                    old_branch,
                    new_branch,
                    channel,
                } => IncomingEvent::git_branch_changed(repo, old_branch, new_branch, channel),
            };
            send_incoming_event(&client, event).await
        }
        Commands::Github { command } => {
            let client = DaemonClient::from_config(config.as_ref());
            let event = match command {
                GithubCommands::IssueOpened {
                    repo,
                    number,
                    title,
                    channel,
                } => IncomingEvent::github_issue_opened(repo, number, title, channel),
                GithubCommands::PrStatusChanged {
                    repo,
                    number,
                    title,
                    old_status,
                    new_status,
                    url,
                    channel,
                } => IncomingEvent::github_pr_status_changed(
                    repo, number, title, old_status, new_status, url, channel,
                ),
            };
            send_incoming_event(&client, event).await
        }
        Commands::Agent { command } => {
            let client = DaemonClient::from_config(config.as_ref());
            let event = match command {
                AgentCommands::Started(args) => IncomingEvent::agent_started(
                    args.agent_name,
                    args.session_id,
                    args.project,
                    args.elapsed_secs,
                    args.summary,
                    args.mention,
                    args.channel,
                ),
                AgentCommands::Blocked(args) => IncomingEvent::agent_blocked(
                    args.agent_name,
                    args.session_id,
                    args.project,
                    args.elapsed_secs,
                    args.summary,
                    args.mention,
                    args.channel,
                ),
                AgentCommands::Finished(args) => IncomingEvent::agent_finished(
                    args.agent_name,
                    args.session_id,
                    args.project,
                    args.elapsed_secs,
                    args.summary,
                    args.mention,
                    args.channel,
                ),
                AgentCommands::Failed(args) => IncomingEvent::agent_failed(
                    args.event.agent_name,
                    args.event.session_id,
                    args.event.project,
                    args.event.elapsed_secs,
                    args.event.summary,
                    args.error_message,
                    args.event.mention,
                    args.event.channel,
                ),
            };
            send_incoming_event(&client, event).await
        }
        Commands::Install {
            systemd,
            skip_star_prompt,
        } => lifecycle::install(systemd, skip_star_prompt),
        Commands::Update { command, restart } => match command {
            None => lifecycle::update(restart),
            Some(UpdateCommands::Check) => {
                let http = reqwest::Client::builder()
                    .user_agent(format!("clawhip/{VERSION}"))
                    .build()?;
                match update::check_latest_version(&http).await {
                    Ok(Some((version, url))) => {
                        if update::version_is_newer(&version) {
                            println!("Update available: v{VERSION} -> {version}\n{url}");
                        } else {
                            println!("Already up to date (v{VERSION})");
                        }
                    }
                    Ok(None) => println!("No releases found"),
                    Err(error) => eprintln!("Check failed: {error}"),
                }
                Ok(())
            }
            Some(UpdateCommands::Approve) => {
                let client = DaemonClient::from_config(config.as_ref());
                let result = client.post_update_action("approve").await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
                Ok(())
            }
            Some(UpdateCommands::Dismiss) => {
                let client = DaemonClient::from_config(config.as_ref());
                let result = client.post_update_action("dismiss").await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
                Ok(())
            }
            Some(UpdateCommands::Status) => {
                let client = DaemonClient::from_config(config.as_ref());
                let result = client.get_update_status().await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
                Ok(())
            }
        },
        Commands::Uninstall {
            remove_systemd,
            remove_config,
        } => lifecycle::uninstall(remove_systemd, remove_config),
        Commands::Tmux { command } => match command {
            TmuxCommands::Keyword {
                session,
                keyword,
                line,
                channel,
            } => {
                let client = DaemonClient::from_config(config.as_ref());
                send_incoming_event(
                    &client,
                    IncomingEvent::tmux_keyword(session, keyword, line, channel),
                )
                .await
            }
            TmuxCommands::Stale {
                session,
                pane,
                minutes,
                last_line,
                channel,
            } => {
                let client = DaemonClient::from_config(config.as_ref());
                send_incoming_event(
                    &client,
                    IncomingEvent::tmux_stale(session, pane, minutes, last_line, channel),
                )
                .await
            }
            TmuxCommands::New(args) => tmux_wrapper::run(args, config.as_ref()).await,
            TmuxCommands::Watch(args) => tmux_wrapper::watch(args, config.as_ref()).await,
            TmuxCommands::List => {
                let client = DaemonClient::from_config(config.as_ref());
                let registrations = client.list_tmux().await?;
                render_tmux_list(&registrations);
                Ok(())
            }
        },
        Commands::Native { command } => match command {
            NativeCommands::Hook(args) => {
                let client = DaemonClient::from_config(config.as_ref());
                let mut payload = args.read_payload(&mut std::io::stdin())?;
                if let Some(provider) = args.provider.as_deref()
                    && payload.get("provider").is_none()
                    && let Some(object) = payload.as_object_mut()
                {
                    object.insert("provider".into(), serde_json::json!(provider));
                }
                if let Some(source) = args.source.as_deref()
                    && payload.get("source").is_none()
                    && let Some(object) = payload.as_object_mut()
                {
                    object.insert("source".into(), serde_json::json!(source));
                }
                let response = client.send_native_hook(&payload).await?;
                println!("{}", serde_json::to_string(&response)?);
                Ok(())
            }
        },
        Commands::Cron { command } => match command {
            CronCommands::Run { id } => {
                crate::cron::run_configured_job(config.as_ref(), &id).await?;
                println!("Ran cron job {id}");
                Ok(())
            }
        },
        Commands::Config { command } => match command.unwrap_or(ConfigCommand::Interactive) {
            ConfigCommand::Interactive => {
                let mut editable = AppConfig::load_or_default(&config_path)?;
                editable.run_interactive_editor(&config_path)
            }
            ConfigCommand::Show => {
                println!("{}", config.to_pretty_toml()?);
                Ok(())
            }
            ConfigCommand::Path => {
                println!("{}", config_path.display());
                Ok(())
            }
            ConfigCommand::VerifyBindings(args) => run_verify_bindings(config, args).await,
        },
        Commands::Plugin { command } => match command {
            PluginCommands::List => {
                let plugins_dir = plugins::default_plugins_dir()?;
                let discovered = plugins::load_plugins(&plugins_dir)?;

                if discovered.is_empty() {
                    println!("No plugins found in {}", plugins_dir.display());
                    return Ok(());
                }

                println!("NAME\tBRIDGE\tDESCRIPTION");
                for plugin in discovered {
                    println!(
                        "{}\t{}\t{}",
                        plugin.name,
                        plugin.bridge_path.display(),
                        plugin.description.as_deref().unwrap_or("-"),
                    );
                }
                Ok(())
            }
        },
        Commands::Memory { command } => match command {
            MemoryCommands::Init(args) => memory::init(args),
            MemoryCommands::Status(args) => memory::status(args),
        },
        Commands::Hooks { command } => match command {
            HooksCommands::Install(args) => hooks::install(args),
        },
        Commands::Release { command } => match command {
            ReleaseCommands::Preflight { version, repo } => release_preflight::run(repo, version),
        },
    }
}

async fn send_incoming_event(client: &DaemonClient, event: IncomingEvent) -> Result<()> {
    let event = prepare_event(event)?;
    client.send_event(&event).await
}

/// Parse `--expect-name REPO=NAME` entries into a `repo -> name` map.
///
/// **Hard-fails** on any malformed entry instead of silently skipping it, so
/// a typo like `--expect-name clawhip` (missing `=`) cannot bypass the
/// name-match guard during `setup --bind`. This is a correctness guarantee:
/// when the operator asks us to enforce a name, we must either enforce it or
/// refuse the command — never quietly drop the assertion.
///
/// Rejects:
/// - entries without `=` (`"clawhip"`)
/// - empty repo (`"=dev"` or `"   =dev"`)
/// - empty name (`"clawhip="` or `"clawhip=   "`)
/// - duplicate repo keys (prevents ambiguous overrides)
fn parse_expect_name_overrides(
    entries: &[String],
) -> Result<std::collections::HashMap<String, String>> {
    let mut map = std::collections::HashMap::new();
    for entry in entries {
        let (repo, name) = entry
            .split_once('=')
            .ok_or_else(|| format!("--expect-name must be REPO=NAME, got '{entry}'"))?;
        let repo = repo.trim();
        let name = name.trim();
        if repo.is_empty() {
            return Err(format!("--expect-name '{entry}' has an empty repo name").into());
        }
        if name.is_empty() {
            return Err(format!("--expect-name '{entry}' has an empty channel name").into());
        }
        if map.insert(repo.to_string(), name.to_string()).is_some() {
            return Err(format!("--expect-name has duplicate entries for repo '{repo}'").into());
        }
    }
    Ok(map)
}

async fn run_setup(args: SetupArgs, config_path: &std::path::Path) -> Result<()> {
    let mut editable = AppConfig::load_or_default(config_path)?;

    let standard_edits = SetupEdits {
        webhook: args.webhook,
        bot_token: args.bot_token,
        default_channel: args.default_channel,
        default_format: args.default_format,
        daemon_base_url: args.daemon_base_url,
    };

    // Must have at least one meaningful action.
    if standard_edits.is_empty() && args.bind.is_empty() && !args.verify_bindings {
        return Err("setup requires at least one non-empty setup flag".into());
    }

    // Apply standard setup edits first (only if any are set).
    if !standard_edits.is_empty() {
        editable.apply_setup_edits(standard_edits)?;
    }

    // Process --bind entries: resolve each channel against Discord and write a
    // repo binding route with a channel_name hint.
    if !args.bind.is_empty() {
        let client = DiscordClient::from_config(Arc::new(editable.clone()))?;

        // Collect expected-name overrides (repo -> name). Hard-fails on
        // malformed input so a typo like `--expect-name clawhip` cannot
        // silently bypass the name-match guard.
        let expect_map = parse_expect_name_overrides(&args.expect_name)?;

        for entry in &args.bind {
            let (repo, channel_id) = entry
                .split_once('=')
                .ok_or_else(|| format!("--bind must be REPO=CHANNEL_ID, got '{entry}'"))?;
            let repo = repo.trim();
            let channel_id = channel_id.trim();

            let lookup = client.lookup_channel(channel_id).await;
            match &lookup {
                binding_verify::ChannelLookup::Found { name, .. } => {
                    let live_name = name.as_deref().unwrap_or("<unnamed>");

                    // Check expected-name override.
                    if let Some(expected) = expect_map.get(repo) {
                        let expected_clean = expected.trim().trim_start_matches('#');
                        if !live_name.eq_ignore_ascii_case(expected_clean) {
                            return Err(format!(
                                "bind {repo}: channel {channel_id} live name is #{live_name} but --expect-name requires #{expected_clean}"
                            ).into());
                        }
                    }

                    println!("bind: {repo} -> {channel_id} (#{live_name})");
                    editable.apply_repo_binding(repo, channel_id, name.as_deref())?;
                }
                binding_verify::ChannelLookup::NotFound => {
                    return Err(
                        format!("bind {repo}: channel {channel_id} not found on Discord").into(),
                    );
                }
                binding_verify::ChannelLookup::Forbidden => {
                    return Err(format!(
                        "bind {repo}: bot cannot access channel {channel_id} (403 Forbidden)"
                    )
                    .into());
                }
                binding_verify::ChannelLookup::Unauthorized => {
                    return Err("bind: Discord bot token is invalid (401 Unauthorized)".into());
                }
                binding_verify::ChannelLookup::NoToken => {
                    return Err(
                        "bind: --bind requires a Discord bot token; configure [providers.discord].token first".into()
                    );
                }
                binding_verify::ChannelLookup::Transport(msg) => {
                    return Err(format!("bind {repo}: {msg}").into());
                }
            }
        }
    }

    editable.validate()?;

    // Optional full binding audit before saving.
    if args.verify_bindings {
        let client = DiscordClient::from_config(Arc::new(editable.clone()))?;
        let audit = binding_verify::verify(&client, &editable).await;
        print!("{audit}");
        if !audit.all_ok() {
            return Err("setup aborted: binding verification failed (see above)".into());
        }
    }

    editable.save(config_path)?;
    println!("Saved {}", config_path.display());
    Ok(())
}

async fn run_verify_bindings(config: Arc<AppConfig>, args: VerifyBindingsArgs) -> Result<()> {
    let client = DiscordClient::from_config(config.clone())?;
    let audit = binding_verify::verify(&client, &config).await;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&audit)?);
    } else {
        print!("{audit}");
    }

    if !audit.all_ok() {
        std::process::exit(1);
    }
    Ok(())
}

fn run_explain(config: &AppConfig, args: ExplainArgs) -> Result<()> {
    let json_output = args.json;
    // Only normalize the event (for canonical_kind / template_context), skip
    // the strict typed-envelope validation that prepare_event does — explain
    // must work even with partial payloads an operator types by hand.
    let event = crate::events::normalize_event(args.into_event()?);
    let router = router::Router::new(Arc::new(config.clone()));
    let provenance = router.explain(&event);

    if json_output {
        println!("{}", serde_json::to_string_pretty(&provenance)?);
    } else {
        print!("{provenance}");
    }

    Ok(())
}

fn render_tmux_list(registrations: &[crate::source::RegisteredTmuxSession]) {
    print!("{}", format_tmux_list(registrations));
}

fn format_tmux_list(registrations: &[crate::source::RegisteredTmuxSession]) -> String {
    if registrations.is_empty() {
        return "No active tmux watches found\n".to_string();
    }

    let mut output =
        "SESSION\tCHANNEL\tKEYWORDS\tMENTION\tSTALE_MINUTES\tSOURCE\tREGISTERED_AT\tPARENT\n"
            .to_string();
    for registration in registrations {
        let keywords = if registration.keywords.is_empty() {
            "-".to_string()
        } else {
            registration.keywords.join(",")
        };
        let parent = registration
            .parent_process
            .as_ref()
            .map(|parent| match parent.name.as_deref() {
                Some(name) => format!("{}:{name}", parent.pid),
                None => parent.pid.to_string(),
            })
            .unwrap_or_else(|| "-".to_string());

        output.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            registration.session,
            registration.channel.as_deref().unwrap_or("-"),
            keywords,
            registration.mention.as_deref().unwrap_or("-"),
            registration.stale_minutes,
            registration.registration_source.as_str(),
            registration.registered_at,
            parent,
        ));
    }

    output
}

#[cfg(test)]
mod tests {
    use super::{format_tmux_list, parse_expect_name_overrides};
    use crate::events::RoutingMetadata;
    use crate::source::tmux::{ParentProcessInfo, RegisteredTmuxSession, RegistrationSource};

    #[test]
    fn parse_expect_name_overrides_accepts_well_formed_entries() {
        let entries = vec![
            "clawhip=clawhip-dev".to_string(),
            "oh-my-codex=omx-dev".to_string(),
        ];
        let map = parse_expect_name_overrides(&entries).expect("valid entries");
        assert_eq!(map.get("clawhip").map(String::as_str), Some("clawhip-dev"));
        assert_eq!(map.get("oh-my-codex").map(String::as_str), Some("omx-dev"));
    }

    #[test]
    fn parse_expect_name_overrides_trims_whitespace() {
        let entries = vec!["  clawhip  =  clawhip-dev  ".to_string()];
        let map = parse_expect_name_overrides(&entries).expect("trimmed entries");
        assert_eq!(map.get("clawhip").map(String::as_str), Some("clawhip-dev"));
    }

    #[test]
    fn parse_expect_name_overrides_rejects_missing_equals() {
        // Regression for #198 review: previously filter_map silently dropped
        // malformed entries, so `--expect-name clawhip` bypassed the guard.
        let entries = vec!["clawhip".to_string()];
        let error = parse_expect_name_overrides(&entries).expect_err("missing = must hard-fail");
        let msg = error.to_string();
        assert!(
            msg.contains("--expect-name must be REPO=NAME"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("'clawhip'"), "error should quote entry: {msg}");
    }

    #[test]
    fn parse_expect_name_overrides_rejects_empty_repo() {
        let entries = vec!["=clawhip-dev".to_string()];
        let error = parse_expect_name_overrides(&entries).expect_err("empty repo must hard-fail");
        assert!(error.to_string().contains("empty repo name"));
    }

    #[test]
    fn parse_expect_name_overrides_rejects_whitespace_only_repo() {
        let entries = vec!["   =clawhip-dev".to_string()];
        let error =
            parse_expect_name_overrides(&entries).expect_err("whitespace repo must hard-fail");
        assert!(error.to_string().contains("empty repo name"));
    }

    #[test]
    fn parse_expect_name_overrides_rejects_empty_name() {
        let entries = vec!["clawhip=".to_string()];
        let error = parse_expect_name_overrides(&entries).expect_err("empty name must hard-fail");
        assert!(error.to_string().contains("empty channel name"));
    }

    #[test]
    fn parse_expect_name_overrides_rejects_whitespace_only_name() {
        let entries = vec!["clawhip=   ".to_string()];
        let error =
            parse_expect_name_overrides(&entries).expect_err("whitespace name must hard-fail");
        assert!(error.to_string().contains("empty channel name"));
    }

    #[test]
    fn parse_expect_name_overrides_rejects_duplicate_repo() {
        let entries = vec![
            "clawhip=clawhip-dev".to_string(),
            "clawhip=omc-dev".to_string(),
        ];
        let error =
            parse_expect_name_overrides(&entries).expect_err("duplicate repo must hard-fail");
        assert!(
            error
                .to_string()
                .contains("duplicate entries for repo 'clawhip'")
        );
    }

    #[test]
    fn parse_expect_name_overrides_accepts_empty_input() {
        let map = parse_expect_name_overrides(&[]).expect("empty input is fine");
        assert!(map.is_empty());
    }

    #[test]
    fn format_tmux_list_renders_metadata_columns() {
        let output = format_tmux_list(&[RegisteredTmuxSession {
            session: "issue-105".into(),
            channel: Some("alerts".into()),
            mention: Some("<@123>".into()),
            routing: RoutingMetadata::default(),
            keywords: vec!["error".into(), "complete".into()],
            keyword_window_secs: 30,
            stale_minutes: 10,
            format: None,
            registered_at: "2026-04-02T00:00:00Z".into(),
            registration_source: RegistrationSource::CliWatch,
            parent_process: Some(ParentProcessInfo {
                pid: 4242,
                name: Some("codex".into()),
            }),
            active_wrapper_monitor: true,
        }]);

        assert!(output.contains(
            "SESSION\tCHANNEL\tKEYWORDS\tMENTION\tSTALE_MINUTES\tSOURCE\tREGISTERED_AT\tPARENT"
        ));
        assert!(output.contains(
            "issue-105\talerts\terror,complete\t<@123>\t10\tcli-watch\t2026-04-02T00:00:00Z\t4242:codex"
        ));
    }

    #[test]
    fn format_tmux_list_handles_empty_registry() {
        assert_eq!(format_tmux_list(&[]), "No active tmux watches found\n");
    }
}
