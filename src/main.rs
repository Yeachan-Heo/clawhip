mod cli;
mod client;
mod config;
mod core;
mod daemon;
mod discord;
mod dispatch;
mod dynamic_tokens;
mod event;
mod events;
mod keyword_window;
mod lifecycle;
mod memory;
mod plugins;
mod render;
mod router;
mod sink;
mod slack;
mod source;
mod tmux_wrapper;

use std::sync::Arc;

use clap::Parser;

use crate::cli::{
    AgentCommands, Cli, Commands, ConfigCommand, GitCommands, GithubCommands, MemoryCommands,
    OmxCommands, PluginCommands, TmuxCommands,
};
use crate::client::DaemonClient;
use crate::config::AppConfig;
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

    match cli.command.unwrap_or(Commands::Start { port: None }) {
        Commands::Start { port } => daemon::run(config, port).await,
        Commands::Status => {
            let client = DaemonClient::from_config(config.as_ref());
            let health = client.health().await?;
            println!("{}", serde_json::to_string_pretty(&health)?);
            Ok(())
        }
        Commands::Emit(args) => {
            let client = DaemonClient::from_config(config.as_ref());
            send_incoming_event(&client, args.into_event()?).await
        }
        Commands::Setup { webhook } => {
            let mut editable = AppConfig::load_or_default(&config_path)?;
            editable.scaffold_webhook_quickstart(webhook);
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
        Commands::Update { restart } => lifecycle::update(restart),
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
        Commands::Omx { command } => match command {
            OmxCommands::Hook(args) => {
                let client = DaemonClient::from_config(config.as_ref());
                let payload = args.read_payload(&mut std::io::stdin())?;
                let response = client.send_omx_hook(&payload).await?;
                println!("{}", serde_json::to_string(&response)?);
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
    }
}

async fn send_incoming_event(client: &DaemonClient, event: IncomingEvent) -> Result<()> {
    let event = prepare_event(event)?;
    client.send_event(&event).await
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
    use crate::source::tmux::{ParentProcessInfo, RegisteredTmuxSession, RegistrationSource};

    #[test]
    fn format_tmux_list_renders_metadata_columns() {
        let output = format_tmux_list(&[RegisteredTmuxSession {
            session: "issue-105".into(),
            channel: Some("alerts".into()),
            mention: Some("<@123>".into()),
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
