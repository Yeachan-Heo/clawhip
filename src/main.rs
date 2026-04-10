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
mod tmux_wrapper;
mod update;

use std::sync::Arc;

use clap::Parser;

use crate::cli::{
    AgentCommands, Cli, Commands, ConfigCommand, CronCommands, ExplainArgs, GitCommands,
    GithubCommands, HooksCommands, MemoryCommands, NativeCommands, PluginCommands, ReleaseCommands,
    TmuxCommands, UpdateCommands,
};
use crate::client::DaemonClient;
use crate::config::{AppConfig, SetupEdits};
use crate::event::compat::from_incoming_event;
use crate::events::IncomingEvent;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub type DynError = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, DynError>;

#[tokio::main]
async fn main() {
    if let Err(error) = real_main().await {
        eprintln!("clawhip error: {error}");
        std::process::exit(1);
    }
}

fn prepare_event(event: IncomingEvent) -> Result<IncomingEvent> {
    let event = crate::events::normalize_event(event);
    let _typed = from_incoming_event(&event)?;
    Ok(event)
}

async fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config_path();
    let config = Arc::new(AppConfig::load_or_default(&config_path)?);
    let cron_state_path = crate::cron::default_state_path(&config_path);

    match cli.command.unwrap_or(Commands::Start { port: None }) {
        Commands::Start { port } => daemon::run(config, port, cron_state_path).await,
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
        Commands::Setup(args) => {
            let mut editable = AppConfig::load_or_default(&config_path)?;
            editable.apply_setup_edits(SetupEdits {
                webhook: args.webhook,
                bot_token: args.bot_token,
                default_channel: args.default_channel,
                default_format: args.default_format,
                daemon_base_url: args.daemon_base_url,
            })?;
            editable.validate()?;
            editable.save(&config_path)?;
            println!("Saved {}", config_path.display());
            Ok(())
        }
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
    use super::format_tmux_list;
    use crate::events::RoutingMetadata;
    use crate::source::tmux::{ParentProcessInfo, RegisteredTmuxSession, RegistrationSource};

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
