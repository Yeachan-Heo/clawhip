mod binding_verify;
mod cli;
mod client;
mod config;
mod core;
mod cron;
mod daemon;
mod discord;
mod discord_watch;
mod dispatch;
mod dynamic_tokens;
mod event;
mod events;
mod gajae;
mod gateway_allowlist;
mod gjc;
mod gjc_lane;
mod gjc_sdk;
mod gjc_sdk_events;
mod hooks;
mod keyword_window;
mod lane;
mod ledger;
mod lifecycle;
mod memory;
mod native_hooks;
mod native_observability;
mod plugins;
mod provenance;
mod release_preflight;
mod render;
mod router;
mod sender_identity;
mod sink;
mod slack;
mod source;
mod telemetry;
mod tmux_wrapper;

mod update;

use std::io::Read;
use std::sync::Arc;

use clap::Parser;
use tokio::runtime::Builder;

use crate::cli::{
    AgentCommands, Cli, Commands, ConfigCommand, CronCommands, ExplainArgs,
    GajaeCheckpointCommands, GajaeCommands, GajaeMutationPlanCommands, GajaeProfileCommands,
    GajaeReceiptCommands, GitCommands, GithubCommands, HooksCommands, LaneCommands, LedgerCommands,
    MemoryCommands, NativeCommands, PluginCommands, ReleaseCommands, SetupArgs, SubscribeCommands,
    TmuxCommands, UpdateCommands, VerifyBindingsArgs, VerifyGatewayAllowlistArgs,
    VerifySenderIdentityArgs,
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

fn run_subscription_adapter(kind: &str) -> Result<()> {
    if kind != "question" {
        return Err(format!("unsupported subscription adapter '{kind}'").into());
    }
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let payload: serde_json::Value = serde_json::from_str(&input)
        .map_err(|_| "subscription adapter received malformed projection")?;
    if !payload.is_object() {
        return Err("subscription adapter projection must be an object".into());
    }
    println!(
        "{}",
        serde_json::json!({
            "type": "workflow.question",
            "payload": payload,
        })
    );
    Ok(())
}

fn load_config_for_cli(config_path: &std::path::Path) -> Result<Arc<AppConfig>> {
    AppConfig::load_or_default(config_path)
        .map(Arc::new)
        .map_err(|_| "config_invalid".into())
}

async fn real_main(cli: Cli) -> Result<()> {
    let config_path = cli.config_path();
    let config = load_config_for_cli(&config_path)?;
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
        Commands::Subscribe { command } => match command {
            SubscribeCommands::Adapter { kind } => run_subscription_adapter(&kind),
            command => {
                let client = DaemonClient::from_config(config.as_ref());
                match command {
                    SubscribeCommands::Validate => {
                        config.validate()?;
                        println!("Subscription configuration is valid.");
                    }
                    SubscribeCommands::List => println!(
                        "{}",
                        serde_json::to_string_pretty(&client.list_subscriptions().await?)?
                    ),
                    SubscribeCommands::Status { name } => println!(
                        "{}",
                        serde_json::to_string_pretty(&client.subscription_status(&name).await?)?
                    ),
                    SubscribeCommands::Start { name } => println!(
                        "{}",
                        serde_json::to_string_pretty(&client.start_subscription(&name).await?)?
                    ),
                    SubscribeCommands::Stop { name } => println!(
                        "{}",
                        serde_json::to_string_pretty(&client.stop_subscription(&name).await?)?
                    ),
                    SubscribeCommands::Adapter { .. } => unreachable!(),
                }
                Ok(())
            }
        },
        Commands::Ledger { command } => {
            match command {
                LedgerCommands::Status => {
                    let client = DaemonClient::from_config(config.as_ref());
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&client.ledger_status().await?)?
                    );
                }
                LedgerCommands::Query(args) => {
                    let mut params = Vec::<(&str, String)>::new();
                    if let Some(value) = args.repo {
                        params.push(("repo", value));
                    }
                    if let Some(value) = args.worktree {
                        params.push(("worktree", value));
                    }
                    if let Some(value) = args.session_id {
                        params.push(("session_id", value));
                    }
                    if let Some(value) = args.event_type {
                        params.push(("event_type", value));
                    }
                    if let Some(value) = args.since {
                        params.push(("since", value.to_string()));
                    }
                    if let Some(value) = args.until {
                        params.push(("until", value.to_string()));
                    }
                    if !args.keywords.is_empty() {
                        params.push(("keywords", args.keywords.join(",")));
                    }
                    if let Some(value) = args.limit {
                        params.push(("limit", value.to_string()));
                    }
                    let client = DaemonClient::from_config(config.as_ref());
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&client.ledger_query(&params).await?)?
                    );
                }
                LedgerCommands::Verify => {
                    let root = cron_state_path
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new("."));
                    let ledger = crate::ledger::EventLedger::open(config.ledger.clone(), root)?;
                    println!("{}", serde_json::to_string_pretty(&ledger.verify()?)?);
                }
            }
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
        Commands::Lane { command } => match command {
            LaneCommands::Status { session, json } => lane::status(config, session, json).await,
            LaneCommands::VerifyThread { session, json } => {
                lane::verify_thread(config, session, json).await
            }
            LaneCommands::Update {
                session,
                message,
                kind,
                workflow,
                json,
            } => lane::update(config, session, message, kind, workflow, json).await,
        },
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
                let health = if registrations.is_empty() {
                    client.health().await.ok()
                } else {
                    None
                };
                render_tmux_list(&registrations, health.as_ref());
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
                let mut editable = (*load_config_for_cli(&config_path)?).clone();
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
            ConfigCommand::VerifyGatewayAllowlist(args) => {
                run_verify_gateway_allowlist(config, args)
            }
            ConfigCommand::VerifySenderIdentity(args) => {
                run_verify_sender_identity(config, args).await
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
            MemoryCommands::ScaffoldChannels(args) => {
                memory::scaffold_channels(args, config.as_ref())
            }
        },
        Commands::Hooks { command } => match command {
            HooksCommands::Install(args) => hooks::install(args),
        },
        Commands::Gajae { command } => match command {
            GajaeCommands::Status => Ok(gajae::run(gajae::GajaeCommand::Status)?),
            GajaeCommands::Preflight => Ok(gajae::run_preflight()?),
            GajaeCommands::Doctor(args) => Ok(gajae::run_doctor(gajae::DoctorOptions {
                repo: args.repo,
                file: args.file,
            })?),
            GajaeCommands::Profile { command } => match command {
                GajaeProfileCommands::Install => {
                    let status = gajae::run_profile_install()?;
                    if status.success {
                        Ok(())
                    } else {
                        eprintln!(
                            "clawhip error: {}",
                            gajae::profile_install_failure_message(status)
                        );
                        std::process::exit(status.code.unwrap_or(1));
                    }
                }
                GajaeProfileCommands::Verify(args) => {
                    Ok(gajae::run_profile_verify(gajae::ProfileVerifyOptions {
                        file: args.file,
                    })?)
                }
                GajaeProfileCommands::Inspect(args) => {
                    Ok(gajae::run_profile_inspect(gajae::ProfileInspectOptions {
                        file: args.file,
                    })?)
                }
                GajaeProfileCommands::Explain(args) => {
                    Ok(gajae::run_profile_explain(gajae::ProfileExplainOptions {
                        file: args.file,
                        event: args.event,
                        repo: args.repo,
                    })?)
                }
                GajaeProfileCommands::Apply(args) => {
                    Ok(gajae::run_profile_apply(gajae::ProfileApplyOptions {
                        file: args.file,
                        dry_run: args.dry_run,
                        approve: args.approve,
                    })?)
                }
            },
            GajaeCommands::Receipt { command } => match command {
                GajaeReceiptCommands::Ingest(args) => {
                    let source = if let Some(file) = args.file {
                        gajae::ReceiptSource::File(file)
                    } else if args.stdin {
                        let input = gajae::read_receipt_stdin(&mut std::io::stdin())?;
                        gajae::ReceiptSource::Stdin(input)
                    } else {
                        return Err("receipt ingest requires --file or --stdin".into());
                    };
                    let send = args.send;
                    let result = gajae::ingest_receipt(gajae::ReceiptIngestRequest {
                        family: args.family,
                        source,
                        channel: args.channel,
                    })?;
                    if send {
                        let client = DaemonClient::from_config(config.as_ref());
                        send_incoming_event(&client, result.event).await?;
                        println!("{{\"status\":\"sent\"}}");
                    } else {
                        println!("{}", serde_json::to_string(&result.event)?);
                    }
                    Ok(())
                }
            },
            GajaeCommands::MutationPlan { command } => match command {
                GajaeMutationPlanCommands::Plan(args) => {
                    let plan = gajae::github_mutation_plan(gajae::GithubMutationPlanRequest {
                        repo: args.repo,
                        kind: args.kind,
                        target: args.target,
                        body: args.body,
                        label: args.label,
                        actor: args.actor,
                        existing_keys: args.existing_keys,
                    })?;
                    println!("{}", serde_json::to_string(&plan)?);
                    Ok(())
                }
            },
            GajaeCommands::Checkpoint { command } => match command {
                GajaeCheckpointCommands::ZeroBacklog(args) => {
                    let checkpoint = gajae::zero_backlog_followup_checkpoint(
                        gajae::ZeroBacklogCheckpointRequest {
                            repo: args.repo,
                            open_issues: args.open_issues,
                            open_prs: args.open_prs,
                            action_needed_sessions: args.action_needed_sessions,
                            observation_source: args.source,
                            approval_hold: args.approval_hold,
                            release_hold: args.release_hold,
                        },
                    )?;
                    println!("{}", serde_json::to_string(&checkpoint)?);
                    Ok(())
                }
            },
        },
        Commands::Release { command } => match command {
            ReleaseCommands::Preflight { version, repo } => release_preflight::run(repo, version),
        },
        Commands::Gjc { command } => crate::gjc::cli::run(config.clone(), command).await,
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

fn parse_bind_overrides(entries: &[String]) -> Result<Vec<(String, String)>> {
    let mut repos = std::collections::HashSet::new();
    let mut binds = Vec::new();
    for entry in entries {
        let (repo, channel_id) = entry
            .split_once('=')
            .ok_or_else(|| format!("--bind must be REPO=CHANNEL_ID, got '{entry}'"))?;
        let repo = repo.trim();
        let channel_id = channel_id.trim();
        if repo.is_empty() {
            return Err(format!("--bind '{entry}' has an empty repo name").into());
        }
        if channel_id.is_empty() {
            return Err(format!("--bind '{entry}' has an empty channel id").into());
        }
        if !repos.insert(repo.to_string()) {
            return Err(format!("--bind has duplicate entries for repo '{repo}'").into());
        }
        binds.push((repo.to_string(), channel_id.to_string()));
    }
    Ok(binds)
}

fn parse_bind_checkout_overrides(
    entries: &[String],
) -> Result<std::collections::HashMap<String, String>> {
    let mut map = std::collections::HashMap::new();
    for entry in entries {
        let (repo, path) = entry
            .split_once('=')
            .ok_or_else(|| format!("--bind-checkout must be REPO=PATH, got '{entry}'"))?;
        let repo = repo.trim();
        let path = path.trim();
        if repo.is_empty() {
            return Err(format!("--bind-checkout '{entry}' has an empty repo name").into());
        }
        if path.is_empty() {
            return Err(format!("--bind-checkout '{entry}' has an empty checkout path").into());
        }
        if map.insert(repo.to_string(), path.to_string()).is_some() {
            return Err(format!("--bind-checkout has duplicate entries for repo '{repo}'").into());
        }
    }
    Ok(map)
}

fn validate_bind_checkout_repos(
    checkout_map: &std::collections::HashMap<String, String>,
    binds: &[(String, String)],
) -> Result<()> {
    let bind_repos: std::collections::HashSet<&str> =
        binds.iter().map(|(repo, _)| repo.as_str()).collect();
    for repo in checkout_map.keys() {
        if !bind_repos.contains(repo.as_str()) {
            return Err(
                format!("--bind-checkout repo '{repo}' must also be present in --bind").into(),
            );
        }
    }
    Ok(())
}

fn resolve_bind_checkout_path(
    repo: &str,
    explicit: &std::collections::HashMap<String, String>,
    bind_count: usize,
    config: &AppConfig,
) -> Result<Option<String>> {
    if let Some(path) = explicit.get(repo) {
        return Ok(Some(path.clone()));
    }
    if let Some(path) = existing_setup_monitor_checkout_path(config, repo)? {
        return Ok(Some(path));
    }
    if bind_count == 1
        && let Some(path) = infer_cwd_checkout_path(repo)?
    {
        return Ok(Some(path));
    }
    Ok(None)
}

fn existing_setup_monitor_checkout_path(config: &AppConfig, repo: &str) -> Result<Option<String>> {
    let matches = config
        .monitors
        .git
        .repos
        .iter()
        .filter(|monitor| {
            monitor.setup_owned
                && (monitor.github_repo.as_deref() == Some(repo)
                    || (monitor.github_repo.is_none() && monitor.name.as_deref() == Some(repo)))
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Ok(None),
        [monitor] => Ok(Some(monitor.path.clone())),
        _ => Err(format!(
            "bind {repo}: multiple setup-owned git monitors already exist; pass --bind-checkout {repo}=PATH after cleanup"
        )
        .into()),
    }
}

fn infer_cwd_checkout_path(repo: &str) -> Result<Option<String>> {
    let cwd = std::env::current_dir()?;
    let inside = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(&cwd)
        .output();
    let Ok(inside) = inside else {
        return Ok(None);
    };
    if !inside.status.success() || String::from_utf8_lossy(&inside.stdout).trim() != "true" {
        return Ok(None);
    }

    let top_level = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&cwd)
        .output()?;
    if !top_level.status.success() {
        return Ok(None);
    }

    let remote = std::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(&cwd)
        .output()?;
    if !remote.status.success() {
        return Ok(None);
    }
    let remote = String::from_utf8_lossy(&remote.stdout);
    if remote_repo_identity(remote.trim()).as_deref() != Some(repo) {
        return Ok(None);
    }

    Ok(Some(
        String::from_utf8_lossy(&top_level.stdout)
            .trim()
            .to_string(),
    ))
}

fn remote_repo_identity(remote: &str) -> Option<String> {
    let mut value = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    if let Some((_, path)) = value.rsplit_once(':') {
        if !path.contains('/') && value.contains("://") {
            return None;
        }
        value = path;
    } else if let Some((_, path)) = value.split_once("://") {
        value = path.split_once('/').map(|(_, rest)| rest)?;
    }
    let mut parts = value.rsplit('/');
    let repo = parts.next()?.trim();
    let owner = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

fn current_setup_repo_identity() -> Result<String> {
    let cwd = std::env::current_dir()?;
    let output = std::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd)
        .output()?;
    if !output.status.success() {
        return Err("question setup could not resolve the current repository remote".into());
    }
    remote_repo_identity(String::from_utf8_lossy(&output.stdout).trim())
        .ok_or_else(|| "question setup could not resolve owner/repo from remote.origin.url".into())
}

async fn run_setup(args: SetupArgs, config_path: &std::path::Path) -> Result<()> {
    let mut editable = (*load_config_for_cli(config_path)?).clone();

    let standard_edits = SetupEdits {
        webhook: args.webhook,
        bot_token: args.bot_token,
        default_channel: args.default_channel,
        default_format: args.default_format,
        daemon_base_url: args.daemon_base_url,
    };

    let binds = parse_bind_overrides(&args.bind)?;
    let checkout_map = parse_bind_checkout_overrides(&args.bind_checkout)?;
    validate_bind_checkout_repos(&checkout_map, &binds)?;
    let expect_map = parse_expect_name_overrides(&args.expect_name)?;

    // Must have at least one meaningful action.
    let question_setup_requested = args.question_channel.is_some() || args.question_fallback;
    if standard_edits.is_empty()
        && binds.is_empty()
        && !args.verify_bindings
        && !question_setup_requested
    {
        return Err("setup requires at least one non-empty setup flag".into());
    }

    // Apply standard setup edits first (only if any are set).
    if !standard_edits.is_empty() {
        editable.apply_setup_edits(standard_edits)?;
    }

    if question_setup_requested || args.question_mention.is_some() {
        let repo = current_setup_repo_identity()?;
        let adapter_program = std::env::current_exe()?.to_string_lossy().into_owned();
        editable.apply_gjc_question_setup(
            args.question_channel,
            args.question_mention,
            args.question_fallback,
            repo,
            adapter_program,
        )?;
    }

    // Process --bind entries: resolve each channel against Discord and write a
    // route binding. Git monitor onboarding is conditional: it is added or
    // updated only when a checkout is explicit, already known, or safely inferred.
    let mut monitored_bind_repos = std::collections::HashSet::new();
    if !binds.is_empty() {
        let client = DiscordClient::from_config(Arc::new(editable.clone()))?;

        for (repo, channel_id) in &binds {
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

                    let checkout_path =
                        resolve_bind_checkout_path(repo, &checkout_map, binds.len(), &editable)?;
                    println!("bind: {repo} -> {channel_id} (#{live_name})");
                    if let Some(checkout_path) = checkout_path.as_deref() {
                        editable.apply_repo_channel_binding(
                            repo,
                            channel_id,
                            name.as_deref(),
                            checkout_path,
                        )?;
                        monitored_bind_repos.insert(repo.as_str());
                    } else {
                        editable.apply_repo_channel_route_binding(
                            repo,
                            channel_id,
                            name.as_deref(),
                        )?;
                    }
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

    let drift_audit = binding_verify::audit_route_monitor_drift(&editable);
    if !monitored_bind_repos.is_empty() {
        let bind_repos = monitored_bind_repos;
        let affected_errors = drift_audit
            .findings
            .iter()
            .filter(|finding| {
                finding.severity == "error" && bind_repos.contains(finding.repo.as_str())
            })
            .collect::<Vec<_>>();
        if !affected_errors.is_empty() {
            eprint!("{drift_audit}");
            return Err("setup aborted: route/monitor drift check failed for bound repo(s)".into());
        }
    }

    // Optional full binding audit before saving.
    if args.verify_bindings {
        let client = DiscordClient::from_config(Arc::new(editable.clone()))?;
        let audit = binding_verify::verify(&client, &editable).await;
        print!("{audit}");
        print!("{drift_audit}");
        if !audit.all_ok() || !drift_audit.ok {
            return Err("setup aborted: binding verification failed (see above)".into());
        }
    }

    let cleanup = editable.save_with_backup_reporting(config_path)?;
    println!("Saved {}", config_path.display());
    if cleanup.classified > 0 {
        println!(
            "Config backup cleanup: {} classified, {} deleted, {} preserved",
            cleanup.classified, cleanup.deleted, cleanup.preserved
        );
    }
    Ok(())
}

fn verify_bindings_json(
    audit: &binding_verify::BindingAudit,
    drift_audit: &binding_verify::BindingDriftAudit,
) -> serde_json::Value {
    serde_json::json!({
        "verdicts": &audit.verdicts,
        "drift_audit": drift_audit,
    })
}

async fn run_verify_bindings(config: Arc<AppConfig>, args: VerifyBindingsArgs) -> Result<()> {
    let client = DiscordClient::from_config(config.clone())?;
    let audit = binding_verify::verify(&client, &config).await;
    let drift_audit = binding_verify::audit_route_monitor_drift(&config);

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&verify_bindings_json(&audit, &drift_audit))?
        );
    } else {
        print!("{audit}");
        print!("{drift_audit}");
    }

    if !audit.all_ok() || !drift_audit.ok {
        std::process::exit(1);
    }
    Ok(())
}

async fn run_verify_sender_identity(
    config: Arc<AppConfig>,
    args: VerifySenderIdentityArgs,
) -> Result<()> {
    use crate::sender_identity::{sender_identity_expectation, verify_sender_identity};
    config.validate()?;

    let token_source = config.discord_token_source();
    let expectation = sender_identity_expectation(config.expected_discord_bot_id().as_deref());
    let client = DiscordClient::from_config(config.clone())?;
    let verdict = verify_sender_identity(&client, &expectation).await;

    if args.json {
        let payload = sender_identity_json(&verdict, &expectation, token_source);
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        print_sender_identity_report(&verdict, token_source);
    }

    // Fail closed: every non-verified outcome — mismatch, absent expectation,
    // invalid credential, rate limit, malformed response, transport failure —
    // exits non-zero. Transport success alone never passes this preflight.
    if !verdict.is_verified() {
        std::process::exit(1);
    }
    Ok(())
}

fn sender_identity_json(
    verdict: &crate::sender_identity::SenderIdentityVerdict,
    expectation: &crate::sender_identity::SenderIdentityExpectation,
    token_source: &str,
) -> serde_json::Value {
    use crate::sender_identity::SenderIdentityExpectation;
    use serde_json::json;
    let expected_bot_id = match expectation {
        SenderIdentityExpectation::Expected { bot_id } => json!(bot_id),
        SenderIdentityExpectation::Absent => serde_json::Value::Null,
    };
    let observed_bot_id = verdict
        .observed_bot_id()
        .map(|id| json!(id))
        .unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "verified": verdict.is_verified(),
        "reason_code": verdict.reason_code(),
        "expected_bot_id": expected_bot_id,
        "observed_bot_id": observed_bot_id,
        "verdict": verdict.to_string(),
        "token_source": token_source,
    })
}

fn print_sender_identity_report(
    verdict: &crate::sender_identity::SenderIdentityVerdict,
    token_source: &str,
) {
    let status = if verdict.is_verified() { "ok" } else { "FAIL" };
    println!("[{status:>4}] {verdict}");
    println!("       token source: {token_source} (credential value never printed)");
}

fn run_verify_gateway_allowlist(
    config: Arc<AppConfig>,
    args: VerifyGatewayAllowlistArgs,
) -> Result<()> {
    let gateway_config_path = match args.gateway_config {
        Some(path) => path,
        None => gateway_allowlist::default_gateway_config_path().ok_or_else(|| {
            "could not resolve default gateway config path; pass --gateway-config <path>"
                .to_string()
        })?,
    };
    let report = gateway_allowlist::verify_from_path(&config, &gateway_config_path)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{report}");
    }

    if !report.all_ok() {
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

fn render_tmux_list(
    registrations: &[crate::source::RegisteredTmuxSession],
    health: Option<&serde_json::Value>,
) {
    print!("{}", format_tmux_list_with_health(registrations, health));
}

#[cfg(test)]
fn format_tmux_list(registrations: &[crate::source::RegisteredTmuxSession]) -> String {
    format_tmux_list_with_health(registrations, None)
}

fn format_tmux_list_with_health(
    registrations: &[crate::source::RegisteredTmuxSession],
    health: Option<&serde_json::Value>,
) -> String {
    if registrations.is_empty() {
        let detail = tmux_empty_list_detail(health);
        return format!("No active tmux watches found{detail}\n");
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

fn tmux_empty_list_detail(health: Option<&serde_json::Value>) -> String {
    let Some(tmux) = health.and_then(|health| health.get("tmux")) else {
        return String::new();
    };
    let registry_state = tmux.get("registry_state");
    if registry_state
        .and_then(|state| state.get("status"))
        .and_then(serde_json::Value::as_str)
        == Some("ignored-invalid")
    {
        let path = registry_state
            .and_then(|state| state.get("path"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("tmux-watch-registry.json");
        return format!("; ignored invalid registry state at {path}");
    }
    if let Some(error) = tmux
        .get("live_probe")
        .and_then(|probe| probe.get("error"))
        .and_then(serde_json::Value::as_str)
    {
        return format!("; live tmux probe failed: {error}");
    }
    let live_count = tmux
        .get("live_probe")
        .and_then(|probe| probe.get("count"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if live_count > 0 {
        return format!(
            "; {live_count} live tmux session(s) exist but no clawhip watch routes are registered"
        );
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::{
        format_tmux_list, load_config_for_cli, parse_bind_checkout_overrides, parse_bind_overrides,
        parse_expect_name_overrides, validate_bind_checkout_repos, verify_bindings_json,
    };
    use crate::binding_verify::{BindingAudit, BindingDriftAudit};
    use crate::events::RoutingMetadata;
    use crate::source::tmux::{ParentProcessInfo, RegisteredTmuxSession, RegistrationSource};
    use std::fs;

    #[test]
    fn cli_config_errors_are_bounded_without_source_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"[[subscriptions]]
name = "safe-subscription"
enabled = false
kind = "websocket"
endpoint_env = "SAFE_ENDPOINT"
endpoint = "wss://private.invalid?token=secret-token"

[subscriptions.filter]
discriminator_pointer = "/type"
discriminator_equals = "workflow_gate"

[subscriptions.projection]
workflow_id = "/workflow/id"

[subscriptions.adapter]
program = "/bin/true"
"#,
        )
        .unwrap();

        let error = load_config_for_cli(&path).unwrap_err().to_string();
        assert_eq!(error, "config_invalid");
        assert!(!error.contains("secret-token"));
        assert!(!error.contains("private.invalid"));
    }
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
    fn parse_bind_checkout_overrides_accepts_well_formed_entries() {
        let entries = vec![
            "clawhip=/work/clawhip".to_string(),
            "oh-my-codex=../omx".to_string(),
        ];
        let map = parse_bind_checkout_overrides(&entries).expect("valid entries");
        assert_eq!(
            map.get("clawhip").map(String::as_str),
            Some("/work/clawhip")
        );
        assert_eq!(map.get("oh-my-codex").map(String::as_str), Some("../omx"));
    }

    #[test]
    fn parse_bind_checkout_overrides_rejects_duplicate_repo() {
        let entries = vec![
            "clawhip=/work/clawhip".to_string(),
            "clawhip=/tmp/clawhip".to_string(),
        ];
        let error = parse_bind_checkout_overrides(&entries).expect_err("duplicate repo must fail");
        assert!(
            error
                .to_string()
                .contains("duplicate entries for repo 'clawhip'")
        );
    }

    #[test]
    fn parse_bind_checkout_overrides_rejects_malformed_or_empty_parts() {
        let missing_equals = parse_bind_checkout_overrides(&["clawhip".to_string()])
            .expect_err("missing equals must fail");
        assert!(missing_equals.to_string().contains("REPO=PATH"));

        let empty_repo = parse_bind_checkout_overrides(&["=/work/clawhip".to_string()])
            .expect_err("empty repo must fail");
        assert!(empty_repo.to_string().contains("empty repo name"));

        let empty_path = parse_bind_checkout_overrides(&["clawhip=   ".to_string()])
            .expect_err("empty path must fail");
        assert!(empty_path.to_string().contains("empty checkout path"));
    }

    #[test]
    fn bind_checkout_repos_must_also_be_bound() {
        let binds = parse_bind_overrides(&["clawhip=123".to_string()]).expect("valid bind");
        let checkout_map = parse_bind_checkout_overrides(&["oh-my-codex=/work/omx".to_string()])
            .expect("valid checkout");
        let error = validate_bind_checkout_repos(&checkout_map, &binds)
            .expect_err("unmatched checkout repo must fail");
        assert!(
            error
                .to_string()
                .contains("repo 'oh-my-codex' must also be present in --bind")
        );
    }

    #[test]
    fn bind_checkout_repos_accept_matching_bind() {
        let binds = parse_bind_overrides(&["clawhip=123".to_string()]).expect("valid bind");
        let checkout_map = parse_bind_checkout_overrides(&["clawhip=/work/clawhip".to_string()])
            .expect("valid checkout");
        validate_bind_checkout_repos(&checkout_map, &binds).expect("matching repo is valid");
    }

    #[test]
    fn verify_bindings_json_preserves_top_level_verdicts() {
        let audit = BindingAudit { verdicts: vec![] };
        let drift = BindingDriftAudit {
            ok: true,
            findings: vec![],
        };

        let json = verify_bindings_json(&audit, &drift);

        assert!(json.get("verdicts").is_some());
        assert!(json.get("drift_audit").is_some());
        assert!(json.get("channel_audit").is_none());
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
            registration_generation: 0,
            active_wrapper_monitor: true,
            lane: None,
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
