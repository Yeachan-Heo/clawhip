use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router as AxumRouter};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::{Mutex, RwLock, mpsc};

use crate::Result;
use crate::VERSION;
use crate::config::{AppConfig, GajaeRouteAction, RouteRule};
use crate::cron::CronSource;
use crate::dispatch::Dispatcher;
use crate::event::compat::from_incoming_event;
use crate::events::{IncomingEvent, MessageFormat, normalize_event};
use crate::gajae::{HandlerAction, HandlerLimits, HandlerOutcome};
use crate::native_hooks::{
    NATIVE_NON_GIT_OUTCOME, NATIVE_NORMALIZATION_OUTCOME_FIELD,
    incoming_event_from_native_hook_json,
};
use crate::native_observability::{
    SharedNativeHookObservability, is_native_hook_event, native_event_telemetry_fields,
    new_shared_native_hook_observability, snapshot_shared, with_native_observability,
};
use crate::render::{DefaultRenderer, Renderer};
use crate::router::Router;
use crate::sink::{DiscordSink, LocalFileSink, Sink, SlackSink};
use crate::source::{
    GitHubSource, GitSource, RegisteredTmuxSession, SharedTmuxRegistry, Source, TmuxSource,
    WorkspaceSource, list_active_tmux_registrations,
};
use crate::telemetry;
use crate::update::{self, SharedPendingUpdate};

const EVENT_QUEUE_CAPACITY: usize = 256;
const STALE_NATIVE_REPLAY_GRACE: Duration = Duration::from_secs(5 * 60);
const STALE_NATIVE_REPLAY_REASON: &str = "stale_replay";
const NATIVE_REPLAY_TIMESTAMP_POINTERS: &[&str] = &[
    "/event_timestamp",
    "/timestamp",
    "/observed_at",
    "/created_at",
    "/event_payload/event_timestamp",
    "/event_payload/timestamp",
];
const EVENT_REPLAY_TIMESTAMP_POINTERS: &[&str] = &[
    "/first_seen_at",
    "/event_timestamp",
    "/timestamp",
    "/observed_at",
    "/created_at",
];

#[derive(Clone)]
struct AppState {
    config: Arc<AppConfig>,
    port: u16,
    tx: mpsc::Sender<IncomingEvent>,
    tmux_registry: SharedTmuxRegistry,
    pending_update: SharedPendingUpdate,
    native_observability: SharedNativeHookObservability,
    cron_state_path: PathBuf,
    discord_watch_lock: Arc<Mutex<()>>,
}

pub async fn run(
    config: Arc<AppConfig>,
    port_override: Option<u16>,
    cron_state_path: PathBuf,
) -> Result<()> {
    config.validate()?;
    let token_source = config.discord_token_source();
    println!("clawhip v{VERSION} starting (token_source: {token_source})");
    telemetry::emit(daemon_record(
        telemetry::reason::DAEMON_STARTUP,
        json!({"version": VERSION, "token_source": token_source}),
    ));

    let mut sinks: HashMap<String, Box<dyn Sink>> = HashMap::new();
    sinks.insert(
        "discord".into(),
        Box::new(DiscordSink::from_config(config.clone())?),
    );
    sinks.insert("slack".into(), Box::new(SlackSink::default()));
    sinks.insert("localfile".into(), Box::new(LocalFileSink));
    let renderer: Box<dyn Renderer> = Box::new(DefaultRenderer);
    let router = Router::new(config.clone());
    let tmux_registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
    let (tx, rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
    let native_observability = new_shared_native_hook_observability();

    let ci_batch_window = config.dispatch.ci_batch_window();
    let routine_batch_window = config.dispatch.routine_batch_window();
    let dispatcher_native_observability = native_observability.clone();
    tokio::spawn(async move {
        let mut dispatcher = Dispatcher::new(
            rx,
            router,
            renderer,
            sinks,
            ci_batch_window,
            routine_batch_window,
            dispatcher_native_observability,
        );
        if let Err(error) = dispatcher.run().await {
            eprintln!("clawhip dispatcher stopped: {error}");
        }
    });
    spawn_source(GitSource::new(config.clone()), tx.clone());
    spawn_source(GitHubSource::new(config.clone()), tx.clone());
    spawn_source(
        TmuxSource::new(config.clone(), tmux_registry.clone()),
        tx.clone(),
    );
    spawn_source(WorkspaceSource::new(config.clone()), tx.clone());
    spawn_source(
        CronSource::new(config.clone(), cron_state_path.clone()),
        tx.clone(),
    );

    let pending_update = update::new_shared_pending_update();
    {
        let config = config.clone();
        let tx = tx.clone();
        let pending = pending_update.clone();
        tokio::spawn(async move {
            update::run_checker(config, tx, pending).await;
        });
    }

    let app = AxumRouter::new()
        .route("/health", get(health))
        .route("/api/status", get(status))
        .route("/event", post(post_event))
        .route("/api/event", post(post_event))
        .route("/events", post(post_event))
        .route("/native/hook", post(post_native_hook))
        .route("/api/native/hook", post(post_native_hook))
        .route("/api/tmux/register", post(register_tmux))
        .route("/api/tmux", get(list_tmux))
        .route("/github", post(post_github))
        .route("/api/update/status", get(update_status))
        .route("/api/update/approve", post(approve_update))
        .route("/api/update/dismiss", post(dismiss_update));
    let port = port_override.unwrap_or(config.daemon.port);

    let app = app.with_state(AppState {
        config: config.clone(),
        port,
        tx,
        tmux_registry,
        pending_update,
        native_observability,
        cron_state_path,
        discord_watch_lock: Arc::new(Mutex::new(())),
    });
    let addr: SocketAddr = format!("{}:{}", config.daemon.bind_host, port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    println!(
        "clawhip daemon v{VERSION} listening on http://{} (token_source: {token_source})",
        local_addr
    );
    telemetry::emit(daemon_record(
        telemetry::reason::DAEMON_LISTENING,
        json!({"version": VERSION, "addr": local_addr.to_string(), "token_source": token_source}),
    ));
    axum::serve(listener, app).await?;
    Ok(())
}

fn spawn_source<S>(source: S, tx: mpsc::Sender<IncomingEvent>)
where
    S: Source + Send + Sync + 'static,
{
    let source_name = source.name().to_string();
    tokio::spawn(async move {
        println!("clawhip source '{}' starting", source_name);
        telemetry::emit(source_lifecycle_record(
            telemetry::reason::SOURCE_START,
            &source_name,
            None,
        ));
        if let Err(error) = source.run(tx.clone()).await {
            telemetry::emit(source_lifecycle_record(
                telemetry::reason::SOURCE_STOPPED,
                &source_name,
                Some(error.to_string()),
            ));
            eprintln!("clawhip source '{}' stopped: {error}", source_name);
            if let Err(alert_error) = tx
                .send(source_failure_alert_event(&source_name, &error.to_string()))
                .await
            {
                eprintln!(
                    "clawhip source '{}' could not enqueue degraded alert: {alert_error}",
                    source_name
                );
            }
        }
    });
}

fn source_failure_alert_event(source_name: &str, error_message: &str) -> IncomingEvent {
    let mut event = IncomingEvent::custom(
        None,
        format!("clawhip degraded: source '{source_name}' stopped: {error_message}"),
    )
    .with_format(Some(MessageFormat::Alert));

    if let Some(payload) = event.payload.as_object_mut() {
        payload.insert("source_name".to_string(), json!(source_name));
        payload.insert("health_status".to_string(), json!("degraded"));
        payload.insert("error_message".to_string(), json!(error_message));
    }

    event
}

fn daemon_record(reason_code: &str, details: Value) -> serde_json::Map<String, Value> {
    let mut record = telemetry::record(
        telemetry::event_name::DAEMON_PHASE,
        reason_code,
        format!("daemon:{reason_code}"),
    );
    record.insert("details".to_string(), details);
    record
}

fn source_lifecycle_record(
    reason_code: &str,
    source_name: &str,
    error: Option<String>,
) -> serde_json::Map<String, Value> {
    let event_name = if reason_code == telemetry::reason::SOURCE_STOPPED {
        telemetry::event_name::SOURCE_DEGRADED
    } else {
        telemetry::event_name::SOURCE_INVENTORY
    };
    let mut record = telemetry::record(event_name, reason_code, format!("source:{source_name}"));
    record.insert("source".to_string(), json!(source_name));
    if let Some(error) = error {
        record.insert("error".to_string(), json!(error));
    }
    record
}

fn event_record(
    event_name: &str,
    reason_code: &str,
    event: &IncomingEvent,
    details: Value,
) -> serde_json::Map<String, Value> {
    let mut record = telemetry::record(
        event_name,
        reason_code,
        telemetry::correlation_id_for_event(event),
    );
    record.insert("event_kind".to_string(), json!(event.canonical_kind()));
    record.insert("details".to_string(), details);
    record
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let registered = state.tmux_registry.read().await.len();
    let native_hooks = snapshot_shared(&state.native_observability);
    Json(health_payload(
        state.config.as_ref(),
        state.port,
        registered,
        native_hooks,
    ))
}

fn health_payload(
    config: &AppConfig,
    port: u16,
    registered_tmux_sessions: usize,
    native_hooks: Value,
) -> Value {
    json!({
        "ok": true,
        "version": VERSION,
        "token_source": config.discord_token_source(),
        "webhook_routes_configured": config.has_webhook_routes(),
        "port": port,
        "daemon_base_url": config.daemon.base_url,
        "configured_git_monitors": config.monitors.git.repos.len(),
        "configured_tmux_monitors": config.monitors.tmux.sessions.len(),
        "configured_workspace_monitors": config.monitors.workspace.len(),
        "configured_cron_jobs": config.cron.jobs.len(),
        "registered_tmux_sessions": registered_tmux_sessions,
        "native_hooks": native_hooks,
    })
}

async fn status(State(state): State<AppState>) -> impl IntoResponse {
    health(State(state)).await
}

async fn post_event(
    State(state): State<AppState>,
    Json(event): Json<IncomingEvent>,
) -> impl IntoResponse {
    let canonical_kind = event.canonical_kind();
    if let Some(defer) = stale_replay_defer(
        canonical_kind,
        &event.payload,
        EVENT_REPLAY_TIMESTAMP_POINTERS,
    ) {
        return stale_replay_defer_response(canonical_kind, &defer);
    }

    accept_event(&state, normalize_event(event)).await
}

async fn post_native_hook(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let raw_non_git = native_payload_is_non_git(&payload);
    with_native_observability(&state.native_observability, |observability| {
        observability.observe_received_raw(&payload);
    });
    eprintln!(
        "clawhip native hook received: provider={} event={} repo={} session={}",
        raw_native_field(
            &payload,
            &["/provider", "/source/provider", "/context/provider"],
        ),
        raw_native_field(
            &payload,
            &[
                "/event_name",
                "/event",
                "/hook_event_name",
                "/hookEventName",
            ],
        ),
        raw_native_field(
            &payload,
            &[
                "/repo_name",
                "/context/repo_name",
                "/project",
                "/project_name",
            ],
        ),
        raw_native_field(
            &payload,
            &[
                "/session_id",
                "/sessionId",
                "/context/session_id",
                "/event_payload/session_id",
            ],
        ),
    );

    let event = match incoming_event_from_native_hook_json(&payload) {
        Ok(event) => normalize_event(event),
        Err(error) => {
            with_native_observability(&state.native_observability, |observability| {
                observability.observe_dropped_raw(&payload, "normalization_failed");
            });
            eprintln!(
                "clawhip native hook dropped: provider={} event={} reason=normalization_failed error={}",
                raw_native_field(
                    &payload,
                    &["/provider", "/source/provider", "/context/provider"],
                ),
                raw_native_field(
                    &payload,
                    &[
                        "/event_name",
                        "/event",
                        "/hook_event_name",
                        "/hookEventName",
                    ],
                ),
                error
            );
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": error.to_string()})),
            )
                .into_response();
        }
    };

    with_native_observability(&state.native_observability, |observability| {
        observability.observe_normalized(&event);
    });

    if raw_non_git || native_hook_should_drop(&event) {
        telemetry::emit(event_record(
            telemetry::event_name::EVENT_DROPPED,
            telemetry::reason::DROP_NON_GIT_NATIVE_HOOK,
            &event,
            json!({"dropped": true, "source": "native_hook"}),
        ));
        with_native_observability(&state.native_observability, |observability| {
            observability.observe_dropped(&event, NATIVE_NON_GIT_OUTCOME);
        });
        eprintln!(
            "clawhip native hook dropped: {} reason={}",
            native_event_telemetry_fields(&event),
            NATIVE_NON_GIT_OUTCOME
        );
        return (
            StatusCode::ACCEPTED,
            Json(json!({
                "ok": true,
                "type": event.kind,
                "dropped": true,
                "reason": "non_git",
            })),
        )
            .into_response();
    }

    if let Some(defer) = stale_native_replay_defer(&event, &payload) {
        with_native_observability(&state.native_observability, |observability| {
            observability.observe_deferred(&event, defer.reason);
        });
        eprintln!(
            "clawhip native hook deferred: {} reason={} age_secs={}",
            native_event_telemetry_fields(&event),
            defer.reason,
            defer.age.as_secs()
        );
        return stale_replay_defer_response(&event.kind, &defer);
    }

    accept_event(&state, event).await
}

fn native_payload_is_non_git(payload: &Value) -> bool {
    payload
        .get(NATIVE_NORMALIZATION_OUTCOME_FIELD)
        .and_then(Value::as_str)
        == Some(NATIVE_NON_GIT_OUTCOME)
        || payload
            .get("event_payload")
            .and_then(|payload| payload.get(NATIVE_NORMALIZATION_OUTCOME_FIELD))
            .and_then(Value::as_str)
            == Some(NATIVE_NON_GIT_OUTCOME)
        || payload
            .get("payload")
            .and_then(|payload| payload.get(NATIVE_NORMALIZATION_OUTCOME_FIELD))
            .and_then(Value::as_str)
            == Some(NATIVE_NON_GIT_OUTCOME)
}

fn raw_native_field(payload: &Value, pointers: &[&str]) -> String {
    pointers
        .iter()
        .find_map(|pointer| payload.pointer(pointer).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn native_hook_should_drop(event: &IncomingEvent) -> bool {
    if event
        .payload
        .get(NATIVE_NORMALIZATION_OUTCOME_FIELD)
        .and_then(Value::as_str)
        == Some(NATIVE_NON_GIT_OUTCOME)
    {
        return true;
    }

    event
        .payload
        .get("payload")
        .and_then(|payload| payload.get(NATIVE_NORMALIZATION_OUTCOME_FIELD))
        .and_then(Value::as_str)
        == Some(NATIVE_NON_GIT_OUTCOME)
}

#[derive(Debug, Clone)]
struct NativeReplayDefer {
    reason: &'static str,
    timestamp: String,
    age: Duration,
}

fn stale_native_replay_defer(
    event: &IncomingEvent,
    raw_payload: &Value,
) -> Option<NativeReplayDefer> {
    stale_replay_defer(
        event.canonical_kind(),
        raw_payload,
        NATIVE_REPLAY_TIMESTAMP_POINTERS,
    )
}

fn stale_replay_defer(
    kind: &str,
    raw_payload: &Value,
    timestamp_pointers: &[&str],
) -> Option<NativeReplayDefer> {
    if !is_replay_sensitive_native_kind(kind) {
        return None;
    }

    let timestamp = replay_timestamp(raw_payload, timestamp_pointers)?;
    let observed_at = parse_native_replay_timestamp(&timestamp)?;
    let now = OffsetDateTime::now_utc();
    let age = now - observed_at;
    let age = age.try_into().ok()?;

    (age > STALE_NATIVE_REPLAY_GRACE).then_some(NativeReplayDefer {
        reason: STALE_NATIVE_REPLAY_REASON,
        timestamp,
        age,
    })
}

fn is_replay_sensitive_native_kind(kind: &str) -> bool {
    matches!(
        kind,
        "tool.pre" | "tool.post" | "session.prompt-submitted" | "session.stopped"
    )
}

fn replay_timestamp(raw_payload: &Value, pointers: &[&str]) -> Option<String> {
    pointers
        .iter()
        .find_map(|pointer| timestamp_string(raw_payload.pointer(pointer)))
}

fn stale_replay_defer_response(kind: &str, defer: &NativeReplayDefer) -> axum::response::Response {
    eprintln!(
        "clawhip deferred stale replay: type={} reason={} timestamp={} age_secs={}",
        kind,
        defer.reason,
        defer.timestamp,
        defer.age.as_secs()
    );
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "ok": true,
            "type": kind,
            "deferred": true,
            "quarantined": true,
            "reason": defer.reason,
            "timestamp": defer.timestamp,
            "age_secs": defer.age.as_secs(),
        })),
    )
        .into_response()
}

fn timestamp_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn parse_native_replay_timestamp(value: &str) -> Option<OffsetDateTime> {
    if let Ok(parsed) = OffsetDateTime::parse(value, &Rfc3339) {
        return Some(parsed);
    }

    let integer = value.trim().parse::<i64>().ok()?;
    let unix_seconds = if integer.unsigned_abs() >= 10_000_000_000 {
        integer / 1000
    } else {
        integer
    };
    OffsetDateTime::from_unix_timestamp(unix_seconds).ok()
}

async fn accept_event(state: &AppState, event: IncomingEvent) -> axum::response::Response {
    let envelope = match from_incoming_event(&event) {
        Ok(envelope) => envelope,
        Err(error) => {
            if is_native_hook_event(&event) {
                with_native_observability(&state.native_observability, |observability| {
                    observability.observe_dropped(&event, "validation_error");
                });
            }
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": error.to_string()})),
            )
                .into_response();
        }
    };

    if event.canonical_kind() == "discord.message-create" {
        if let Err(error) = handle_discord_watch(state, &event).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": error.to_string()})),
            )
                .into_response();
        }
        return local_only_event_response(&event, &envelope);
    }

    if event.canonical_kind() == "discord-watch.nudge-intent" {
        return local_only_event_response(&event, &envelope);
    }

    if let Some(handler_event) = run_matching_gajae_handler(state, &event).await {
        return enqueue_accepted_event(state, handler_event).await;
    }

    enqueue_accepted_event(state, event).await
}

async fn enqueue_accepted_event(
    state: &AppState,
    event: IncomingEvent,
) -> axum::response::Response {
    let envelope = match from_incoming_event(&event) {
        Ok(envelope) => envelope,
        Err(error) => {
            if is_native_hook_event(&event) {
                with_native_observability(&state.native_observability, |observability| {
                    observability.observe_dropped(&event, "validation_error");
                });
            }
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": error.to_string()})),
            )
                .into_response();
        }
    };

    match enqueue_event(&state.tx, event.clone()).await {
        Ok(()) => {
            expire_terminal_tmux_registration(state, &event).await;
            telemetry::emit(event_record(
                telemetry::event_name::EVENT_ACCEPTED,
                telemetry::reason::ACCEPT_ENQUEUED,
                &event,
                json!({"event_id": envelope.id.to_string()}),
            ));
            (
                StatusCode::ACCEPTED,
                Json(json!({
                    "ok": true,
                    "type": event.kind,
                    "event_id": envelope.id.to_string(),
                })),
            )
                .into_response()
        }
        Err(error) => {
            if is_native_hook_event(&event) {
                with_native_observability(&state.native_observability, |observability| {
                    observability.observe_dropped(&event, "queue_unavailable");
                });
            }
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"ok": false, "error": error.to_string()})),
            )
                .into_response()
        }
    }
}

fn local_only_event_response(
    event: &IncomingEvent,
    envelope: &crate::event::EventEnvelope,
) -> axum::response::Response {
    (
        StatusCode::ACCEPTED,
        Json(json!({
            "ok": true,
            "type": event.kind,
            "event_id": envelope.id.to_string(),
            "local_only": true,
        })),
    )
        .into_response()
}

async fn run_matching_gajae_handler(
    state: &AppState,
    event: &IncomingEvent,
) -> Option<IncomingEvent> {
    if !state.config.gajae.handlers_enabled {
        return None;
    }
    if event.canonical_kind().starts_with("gajae.handler.") {
        return None;
    }

    let route = matching_gajae_route(&state.config, event)?;
    let action = handler_action(route.gajae.as_ref()?);
    let event_json = handler_event_json(event);
    let limits = HandlerLimits {
        timeout: Duration::from_millis(state.config.gajae.handler_timeout_ms),
        max_output_bytes: state.config.gajae.handler_max_output_bytes,
    };

    let outcome = match crate::gajae::run_handler(&action, &event_json, limits).await {
        Ok(outcome) => outcome,
        Err(error) => HandlerOutcome::Failed {
            code: None,
            stdout: String::new(),
            stderr: bounded_handler_text(&error.to_string()),
        },
    };

    Some(handler_outcome_event(event, &action, outcome))
}

fn matching_gajae_route<'a>(config: &'a AppConfig, event: &IncomingEvent) -> Option<&'a RouteRule> {
    let context = event.template_context();
    config
        .routes
        .iter()
        .filter(|route| route.gajae.is_some())
        .filter(|route| route_matches_event(route, event.canonical_kind(), &context))
        .max_by_key(|route| route_specificity(route, &context))
}

fn route_matches_event(
    route: &RouteRule,
    canonical_kind: &str,
    context: &std::collections::BTreeMap<String, String>,
) -> bool {
    route_event_candidates(canonical_kind)
        .iter()
        .any(|candidate| crate::router::glob_match(&route.event, candidate))
        && route.filter.iter().all(|(key, expected)| {
            context
                .get(key)
                .map(|actual| crate::router::glob_match(expected, actual))
                .unwrap_or(false)
        })
}

fn route_event_candidates(canonical_kind: &str) -> [&str; 2] {
    let suffix = canonical_kind
        .split_once('.')
        .map(|(_, suffix)| suffix)
        .unwrap_or(canonical_kind);
    [canonical_kind, suffix]
}

fn route_specificity(
    route: &RouteRule,
    context: &std::collections::BTreeMap<String, String>,
) -> usize {
    let path_rank = if route.filter.contains_key("worktree_path")
        && context
            .get("worktree_path")
            .is_some_and(|value| !value.trim().is_empty())
    {
        3
    } else if route.filter.contains_key("repo_path")
        && context
            .get("repo_path")
            .is_some_and(|value| !value.trim().is_empty())
    {
        2
    } else if route.filter.contains_key("repo_name")
        && context
            .get("repo_name")
            .is_some_and(|value| !value.trim().is_empty())
    {
        1
    } else {
        0
    };

    (path_rank * 100) + route.filter.len()
}

fn handler_action(config: &GajaeRouteAction) -> HandlerAction {
    HandlerAction {
        subcommand: config.subcommand.clone(),
        args: config.args.clone(),
        requires_approval: config.requires_approval,
    }
}

fn handler_event_json(event: &IncomingEvent) -> Value {
    json!({
        "type": event.canonical_kind(),
        "payload": event.payload,
        "channel": event.channel,
        "mention": event.mention,
        "format": event.format.as_ref().map(|format| format.as_str()),
        "template": event.template,
    })
}

fn handler_outcome_event(
    source: &IncomingEvent,
    action: &HandlerAction,
    outcome: HandlerOutcome,
) -> IncomingEvent {
    let (kind, payload) = match outcome {
        HandlerOutcome::Completed(output) => (
            "gajae.handler.completed",
            json!({
                "source_event": source.canonical_kind(),
                "subcommand": action.subcommand,
                "output": output,
            }),
        ),
        HandlerOutcome::ApprovalRequired(output) => (
            "gajae.handler.approval-required",
            json!({
                "source_event": source.canonical_kind(),
                "subcommand": action.subcommand,
                "output": output,
                "approval_required": true,
            }),
        ),
        HandlerOutcome::Failed {
            code,
            stdout,
            stderr,
        } => (
            "gajae.handler.failed",
            json!({
                "source_event": source.canonical_kind(),
                "subcommand": action.subcommand,
                "exit_code": code,
                "stdout": bounded_handler_text(&stdout),
                "stderr": bounded_handler_text(&stderr),
            }),
        ),
        HandlerOutcome::TimedOut => (
            "gajae.handler.timeout",
            json!({
                "source_event": source.canonical_kind(),
                "subcommand": action.subcommand,
                "timeout": true,
            }),
        ),
    };

    IncomingEvent {
        kind: kind.to_string(),
        channel: source.channel.clone(),
        mention: None,
        format: Some(MessageFormat::Compact),
        template: None,
        payload,
    }
}

fn bounded_handler_text(value: &str) -> String {
    value.chars().take(512).collect()
}

async fn handle_discord_watch(state: &AppState, event: &IncomingEvent) -> Result<()> {
    let now_ms = OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
    let _guard = state.discord_watch_lock.lock().await;
    crate::discord_watch::handle_local_intent_event(
        &state.config.discord_watch,
        &state.cron_state_path,
        event,
        now_ms as i64,
    )?;
    Ok(())
}

async fn expire_terminal_tmux_registration(state: &AppState, event: &IncomingEvent) {
    if !is_terminal_session_event(event.canonical_kind()) {
        return;
    }

    let candidates = terminal_session_candidates(&event.payload);
    if candidates.is_empty() {
        return;
    }

    let mut registry = state.tmux_registry.write().await;
    for session in candidates {
        if registry.remove(&session).is_some() {
            telemetry::emit(tmux_terminal_expiry_record(&session));
        }
    }
}

fn is_terminal_session_event(kind: &str) -> bool {
    matches!(
        kind,
        "session.finished" | "session.stopped" | "session.pr-created"
    )
}

fn tmux_terminal_expiry_record(session: &str) -> serde_json::Map<String, Value> {
    let mut record = telemetry::record(
        telemetry::event_name::SOURCE_INVENTORY,
        "terminal_session_expired",
        format!("source:tmux:{session}"),
    );
    record.insert("source".to_string(), json!("tmux"));
    record.insert("session".to_string(), json!(session));
    record
}

fn terminal_session_candidates(payload: &Value) -> Vec<String> {
    let mut candidates = Vec::new();
    for key in ["session", "session_name", "session_id", "agent_name"] {
        if let Some(value) = payload.get(key).and_then(Value::as_str) {
            let value = value.trim();
            if !value.is_empty() && !candidates.iter().any(|candidate| candidate == value) {
                candidates.push(value.to_string());
            }
        }
    }
    candidates
}

async fn register_tmux(
    State(state): State<AppState>,
    Json(registration): Json<RegisteredTmuxSession>,
) -> impl IntoResponse {
    state
        .tmux_registry
        .write()
        .await
        .insert(registration.session.clone(), registration.clone());
    (
        StatusCode::ACCEPTED,
        Json(json!({"ok": true, "session": registration.session})),
    )
        .into_response()
}

async fn list_tmux(State(state): State<AppState>) -> impl IntoResponse {
    match list_active_tmux_registrations(state.config.as_ref(), &state.tmux_registry).await {
        Ok(registrations) => (StatusCode::OK, Json(json!(registrations))).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn post_github(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let event_name = headers
        .get("x-github-event")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let event = match event_name {
        "issues" if action == "opened" => {
            Some(normalize_event(IncomingEvent::github_issue_opened(
                payload
                    .pointer("/repository/full_name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown/unknown")
                    .to_string(),
                payload
                    .pointer("/issue/number")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
                payload
                    .pointer("/issue/title")
                    .and_then(Value::as_str)
                    .unwrap_or("Untitled issue")
                    .to_string(),
                None,
            )))
        }
        "release" if matches!(action, "published" | "released" | "prereleased" | "edited") => {
            let repo = payload
                .pointer("/repository/full_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown/unknown")
                .to_string();
            let tag = payload
                .pointer("/release/tag_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = payload
                .pointer("/release/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let is_prerelease = payload
                .pointer("/release/prerelease")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let url = payload
                .pointer("/release/html_url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let actor = payload
                .pointer("/sender/login")
                .and_then(Value::as_str)
                .map(ToString::to_string);

            Some(normalize_event(IncomingEvent::github_release(
                action,
                repo,
                tag,
                name,
                is_prerelease,
                url,
                actor,
                None,
            )))
        }
        "pull_request" => {
            let repo = payload
                .pointer("/repository/full_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown/unknown")
                .to_string();
            let number = payload
                .pointer("/pull_request/number")
                .or_else(|| payload.pointer("/number"))
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let title = payload
                .pointer("/pull_request/title")
                .and_then(Value::as_str)
                .unwrap_or("Untitled pull request")
                .to_string();
            let url = payload
                .pointer("/pull_request/html_url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match action {
                "opened" => Some(normalize_event(IncomingEvent::github_pr_status_changed(
                    repo,
                    number,
                    title,
                    "unknown".to_string(),
                    "opened".to_string(),
                    url,
                    None,
                ))),
                "closed" => Some(normalize_event(IncomingEvent::github_pr_status_changed(
                    repo,
                    number,
                    title,
                    "open".to_string(),
                    "closed".to_string(),
                    url,
                    None,
                ))),
                _ => None,
            }
        }
        _ => None,
    };

    let Some(event) = event else {
        let reason = if event_name == "pull_request" {
            "unsupported pull_request action"
        } else {
            "unsupported event"
        };
        return (
            StatusCode::ACCEPTED,
            Json(json!({"ok": true, "ignored": true, "reason": reason})),
        )
            .into_response();
    };

    if let Err(error) = from_incoming_event(&event) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": error.to_string()})),
        )
            .into_response();
    }

    match enqueue_event(&state.tx, event).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"ok": true}))).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn update_status(State(state): State<AppState>) -> impl IntoResponse {
    let pending = state.pending_update.read().await;
    match pending.as_ref() {
        Some(update) => (
            StatusCode::OK,
            Json(json!({
                "pending": true,
                "current_version": update.current_version,
                "latest_version": update.latest_version,
                "release_url": update.release_url,
                "detected_at": update.detected_at,
            })),
        )
            .into_response(),
        None => (
            StatusCode::OK,
            Json(json!({
                "pending": false,
                "current_version": VERSION,
            })),
        )
            .into_response(),
    }
}

async fn approve_update(State(state): State<AppState>) -> impl IntoResponse {
    match update::approve_update(&state.pending_update, &state.config, &state.tx).await {
        Ok(update) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "updated_to": update.latest_version,
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn dismiss_update(State(state): State<AppState>) -> impl IntoResponse {
    match update::dismiss_update(&state.pending_update).await {
        Ok(update) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "dismissed_version": update.latest_version,
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn enqueue_event(tx: &mpsc::Sender<IncomingEvent>, event: IncomingEvent) -> Result<()> {
    tx.send(event)
        .await
        .map_err(|error| format!("event queue unavailable: {error}").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::config::{CronJob, CronJobKind};
    use crate::events::{MessageFormat, RoutingMetadata};
    use crate::router::Router;
    use crate::sink::SinkTarget;
    use crate::source::tmux::{ParentProcessInfo, RegistrationSource};
    use axum::body::to_bytes;
    use std::fs;
    use tempfile::tempdir;
    use tokio::time::{Duration, timeout};

    fn native_hook_test_state() -> (AppState, mpsc::Receiver<IncomingEvent>) {
        let (tx, rx) = mpsc::channel(8);
        (
            AppState {
                config: Arc::new(AppConfig::default()),
                port: 25294,
                tx,
                tmux_registry: Arc::new(RwLock::new(HashMap::new())),
                pending_update: update::new_shared_pending_update(),
                native_observability: new_shared_native_hook_observability(),
                cron_state_path: PathBuf::from("cron-state.json"),
                discord_watch_lock: Arc::new(Mutex::new(())),
            },
            rx,
        )
    }

    fn tmux_registration(session: &str) -> RegisteredTmuxSession {
        RegisteredTmuxSession {
            session: session.into(),
            channel: Some("alerts".into()),
            mention: Some("<@123>".into()),
            routing: RoutingMetadata::default(),
            keywords: vec!["error".into()],
            keyword_window_secs: 30,
            stale_minutes: 15,
            format: None,
            registered_at: "2026-04-02T00:00:00Z".into(),
            registration_source: RegistrationSource::CliWatch,
            parent_process: Some(ParentProcessInfo {
                pid: 4242,
                name: Some("codex".into()),
            }),
            active_wrapper_monitor: true,
        }
    }

    fn gajae_test_event() -> IncomingEvent {
        IncomingEvent {
            kind: "github.pr.opened".into(),
            channel: Some("ops".into()),
            mention: None,
            format: None,
            template: None,
            payload: json!({"repo": "clawhip", "number": 250}),
        }
    }

    #[test]
    fn gajae_handler_completed_event_is_typed_and_bounded_to_data_output() {
        let action = HandlerAction {
            subcommand: "handle-event".into(),
            args: Vec::new(),
            requires_approval: false,
        };
        let event = handler_outcome_event(
            &gajae_test_event(),
            &action,
            HandlerOutcome::Completed(json!({"summary": "ok"})),
        );

        assert_eq!(event.kind, "gajae.handler.completed");
        assert_eq!(event.payload["source_event"], json!("github.pr.opened"));
        assert_eq!(event.payload["output"]["summary"], json!("ok"));
    }

    #[test]
    fn gajae_handler_timeout_event_is_bounded() {
        let action = HandlerAction {
            subcommand: "handle-event".into(),
            args: vec!["--profile".into(), "safe".into()],
            requires_approval: false,
        };
        let event = handler_outcome_event(&gajae_test_event(), &action, HandlerOutcome::TimedOut);

        assert_eq!(event.kind, "gajae.handler.timeout");
        assert_eq!(event.payload["timeout"], json!(true));
        assert!(event.payload.get("stdout").is_none());
        assert!(event.payload.get("stderr").is_none());
    }

    #[test]
    fn gajae_handler_failed_event_bounds_diagnostics_without_raw_dump() {
        let action = HandlerAction {
            subcommand: "handle-event".into(),
            args: Vec::new(),
            requires_approval: false,
        };
        let raw = "x".repeat(2_000);
        let event = handler_outcome_event(
            &gajae_test_event(),
            &action,
            HandlerOutcome::Failed {
                code: Some(17),
                stdout: raw.clone(),
                stderr: raw,
            },
        );

        assert_eq!(event.kind, "gajae.handler.failed");
        assert_eq!(event.payload["exit_code"], json!(17));
        assert!(event.payload["stdout"].as_str().unwrap().len() <= 512);
        assert!(event.payload["stderr"].as_str().unwrap().len() <= 512);
    }

    #[test]
    fn gajae_handler_mutating_output_requires_approval_event() {
        let action = HandlerAction {
            subcommand: "handle-event".into(),
            args: Vec::new(),
            requires_approval: false,
        };
        let event = handler_outcome_event(
            &gajae_test_event(),
            &action,
            HandlerOutcome::ApprovalRequired(json!({"mutation_requested": true})),
        );

        assert_eq!(event.kind, "gajae.handler.approval-required");
        assert_eq!(event.payload["approval_required"], json!(true));
    }

    fn git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .expect("git init");
        assert!(
            git.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&git.stderr)
        );
        dir
    }

    fn stale_rfc3339() -> String {
        (OffsetDateTime::now_utc() - time::Duration::hours(1))
            .format(&Rfc3339)
            .expect("format stale timestamp")
    }

    fn fresh_rfc3339() -> String {
        OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .expect("format fresh timestamp")
    }

    fn native_payload(repo: &std::path::Path, event_name: &str) -> Value {
        json!({
            "provider": "codex",
            "event_name": event_name,
            "directory": repo,
            "cwd": repo,
            "event_payload": {
                "session_id": "sess-213",
                "tool_name": "Bash",
                "cwd": repo
            }
        })
    }

    async fn post_native_payload(payload: Value) -> (Value, mpsc::Receiver<IncomingEvent>) {
        let (state, rx) = native_hook_test_state();
        let response = post_native_hook(State(state), Json(payload))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let response_json: Value = serde_json::from_slice(&body).unwrap();
        (response_json, rx)
    }

    fn insert_timestamp_at_path(payload: &mut Value, path: &[&str], value: String) {
        let mut current = payload;
        for key in &path[..path.len() - 1] {
            current = current
                .as_object_mut()
                .expect("object")
                .entry((*key).to_string())
                .or_insert_with(|| json!({}));
        }
        current
            .as_object_mut()
            .expect("object")
            .insert(path[path.len() - 1].to_string(), Value::String(value));
    }

    #[tokio::test]
    async fn accepted_terminal_session_event_expires_matching_tmux_registration() {
        let (tx, mut rx) = mpsc::channel(1);
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        registry
            .write()
            .await
            .insert("issue-221".into(), tmux_registration("issue-221"));
        registry
            .write()
            .await
            .insert("still-active".into(), tmux_registration("still-active"));
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: registry.clone(),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let response = accept_event(
            &state,
            IncomingEvent {
                kind: "session.finished".into(),
                channel: None,
                mention: None,
                format: None,
                template: None,
                payload: json!({
                    "agent_name": "issue-221",
                    "session": "issue-221",
                    "session_id": "issue-221",
                    "status": "finished"
                }),
            },
        )
        .await;

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            rx.recv().await.expect("queued event").kind,
            "session.finished"
        );
        let registry = registry.read().await;
        assert!(!registry.contains_key("issue-221"));
        assert!(registry.contains_key("still-active"));
    }

    #[tokio::test]
    async fn accepted_non_terminal_session_event_preserves_tmux_registration() {
        let (tx, mut rx) = mpsc::channel(1);
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        registry
            .write()
            .await
            .insert("issue-221".into(), tmux_registration("issue-221"));
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: registry.clone(),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let response = accept_event(
            &state,
            IncomingEvent {
                kind: "session.blocked".into(),
                channel: None,
                mention: None,
                format: None,
                template: None,
                payload: json!({
                    "agent_name": "issue-221",
                    "session": "issue-221",
                    "status": "blocked"
                }),
            },
        )
        .await;

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            rx.recv().await.expect("queued event").kind,
            "session.blocked"
        );
        assert!(registry.read().await.contains_key("issue-221"));
    }

    #[test]
    fn health_payload_includes_version_and_token_source() {
        let mut config = AppConfig::default();
        config.providers.discord.bot_token = Some("config-token".into());
        config.monitors.git.repos.push(Default::default());
        config.monitors.tmux.sessions.push(Default::default());
        config.monitors.workspace.push(Default::default());

        let payload = health_payload(
            &config,
            25294,
            3,
            snapshot_shared(&new_shared_native_hook_observability()),
        );

        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(payload["version"], Value::String(VERSION.to_string()));
        assert_eq!(payload["token_source"], Value::String("config".to_string()));
        assert_eq!(payload["port"], Value::from(25294));
        assert_eq!(payload["configured_git_monitors"], Value::from(1));
        assert_eq!(payload["configured_tmux_monitors"], Value::from(1));
        assert_eq!(payload["configured_workspace_monitors"], Value::from(1));
        assert_eq!(payload["registered_tmux_sessions"], Value::from(3));
        assert!(payload["native_hooks"]["totals"]["received"].is_number());
    }

    #[tokio::test]
    async fn source_failure_alert_defaults_to_alert_format_and_default_channel_routing() {
        let event =
            source_failure_alert_event("cron", "EOF while parsing a value at line 1 column 0");

        assert_eq!(event.kind, "custom");
        assert_eq!(event.channel, None);
        assert_eq!(event.format, Some(MessageFormat::Alert));
        assert_eq!(event.payload["source_name"], Value::from("cron"));
        assert_eq!(event.payload["health_status"], Value::from("degraded"));
        assert!(
            event.payload["message"]
                .as_str()
                .is_some_and(|message| message.contains("source 'cron' stopped"))
        );

        let mut config = AppConfig::default();
        config.defaults.channel = Some("default-alerts".into());
        let router = Router::new(Arc::new(config));
        let delivery = router.preview_delivery(&event).await.expect("delivery");

        assert_eq!(
            delivery.target,
            SinkTarget::DiscordChannel("default-alerts".into())
        );
    }

    #[tokio::test]
    async fn spawn_source_allows_cron_source_to_start_with_empty_state_and_emit_job_event() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        fs::write(&state_path, "").expect("write invalid cron state");

        let mut config = AppConfig::default();
        config.defaults.channel = Some("default-alerts".into());
        config.cron.jobs.push(CronJob {
            id: "dev-followup".into(),
            schedule: "* * * * *".into(),
            timezone: "UTC".into(),
            enabled: true,
            channel: Some("ops".into()),
            mention: None,
            format: Some(MessageFormat::Alert),
            state_file: None,
            zero_backlog_suppression_ttl_secs: 60 * 60,
            kind: CronJobKind::CustomMessage {
                message: "check open PRs".into(),
            },
        });

        let (tx, mut rx) = mpsc::channel(4);
        spawn_source(CronSource::new(Arc::new(config.clone()), state_path), tx);

        let event = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out waiting for cron job event")
            .expect("cron job event");

        assert_eq!(event.kind, "custom");
        assert_eq!(event.channel, Some("ops".into()));
        assert_eq!(event.format, Some(MessageFormat::Alert));
        assert_eq!(event.payload["cron_job_id"], Value::from("dev-followup"));
        assert_eq!(event.payload["cron_timezone"], Value::from("UTC"));

        let router = Router::new(Arc::new(config));
        let delivery = router.preview_delivery(&event).await.expect("delivery");
        assert_eq!(delivery.target, SinkTarget::DiscordChannel("ops".into()));

        let rendered = router
            .render_delivery(&event, &delivery, &crate::render::DefaultRenderer)
            .await
            .expect("rendered event");
        assert!(rendered.contains("check open PRs"));
    }

    #[tokio::test]
    async fn post_event_defers_stale_tool_replay_before_normalization_and_enqueue() {
        let (tx, mut rx) = mpsc::channel(1);
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };
        let event = IncomingEvent {
            kind: "tool.post".into(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: json!({
                "first_seen_at": stale_rfc3339(),
                "tool": "codex",
                "summary": "old replay"
            }),
        };

        let response = post_event(State(state), Json(event)).await.into_response();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let response_json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response_json["ok"], json!(true));
        assert_eq!(response_json["type"], json!("tool.post"));
        assert_eq!(response_json["deferred"], json!(true));
        assert_eq!(response_json["quarantined"], json!(true));
        assert_eq!(response_json["reason"], json!(STALE_NATIVE_REPLAY_REASON));
        assert!(rx.try_recv().is_err(), "stale replay should not enqueue");
    }

    #[tokio::test]
    async fn post_event_preserves_fresh_tool_payload_with_first_seen_at() {
        let (tx, mut rx) = mpsc::channel(1);
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };
        let event = IncomingEvent {
            kind: "tool.post".into(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: json!({
                "first_seen_at": fresh_rfc3339(),
                "tool": "codex",
                "summary": "fresh"
            }),
        };

        let response = post_event(State(state), Json(event)).await.into_response();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let queued = rx.recv().await.expect("queued event");
        assert_eq!(queued.kind, "tool.post");
    }

    #[tokio::test]
    async fn post_event_returns_event_id_and_preserves_normalized_metadata() {
        let (tx, mut rx) = mpsc::channel(1);
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };
        let event = IncomingEvent::agent_started(
            "worker-1".into(),
            Some("sess-123".into()),
            Some("my-repo".into()),
            None,
            Some("booted".into()),
            None,
            None,
        );

        let response = post_event(State(state), Json(event)).await.into_response();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let response_json: Value = serde_json::from_slice(&body).unwrap();
        let event_id = response_json["event_id"].as_str().unwrap();
        assert!(!event_id.is_empty());
        assert_eq!(response_json["type"], Value::from("agent.started"));

        let queued = rx.recv().await.unwrap();
        assert_eq!(queued.payload["event_id"], Value::from(event_id));
        assert_eq!(queued.payload["correlation_id"], Value::from("sess-123"));
        assert!(
            queued
                .payload
                .get("first_seen_at")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
        );
    }

    #[tokio::test]
    async fn discord_watch_nudge_intent_ingress_is_local_only_without_enqueueing() {
        let (tx, mut rx) = mpsc::channel(1);
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let response = accept_event(
            &state,
            IncomingEvent {
                kind: "discord-watch.nudge-intent".into(),
                channel: Some("must-not-route".into()),
                mention: None,
                format: None,
                template: None,
                payload: json!({
                    "id": "intent-1",
                    "created_at_ms": 1000,
                    "reasons": ["t3-channel-backlog"],
                    "source_channel_id": "fixture-general",
                    "source_channel_name": "general",
                    "nudge_target_channel_id": "fixture-nudge-target",
                    "content": "UltraWorkers: <#fixture-general> / general 스윕하라.",
                    "local_only": true
                }),
            },
        )
        .await;

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(
            timeout(Duration::from_millis(25), rx.recv()).await.is_err(),
            "local nudge intents must not enter generic Discord dispatch routing"
        );
    }

    #[tokio::test]
    async fn discord_watch_message_create_writes_local_intent_without_enqueueing() {
        let (tx, mut rx) = mpsc::channel(1);
        let dir = tempdir().expect("tempdir");
        let intents = dir.path().join("discord-watch-intents.jsonl");
        let mut config = AppConfig::default();
        config.discord_watch.enabled = true;
        config.discord_watch.gaebal_gajae_user_id = "fixture-gaebal".into();
        config.discord_watch.watched_channels = vec![crate::config::DiscordWatchChannel {
            id: "fixture-general".into(),
            name: "general".into(),
        }];
        config.discord_watch.owner_user_ids = vec!["owner".into()];
        config.discord_watch.state_file = Some(dir.path().join("discord-watch-state.json"));
        config.discord_watch.intent_file = Some(intents.clone());
        let state = AppState {
            config: Arc::new(config),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: dir.path().join("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let response = accept_event(
            &state,
            IncomingEvent {
                kind: "discord.message-create".into(),
                channel: Some("dm".into()),
                mention: None,
                format: None,
                template: None,
                payload: json!({
                    "message_id": "dm1",
                    "channel_id": "dm",
                    "channel_name": "owner-dm",
                    "author_id": "owner",
                    "content": "please sweep",
                    "mentions": [],
                    "direct_message": true,
                    "author_is_owner": false,
                    "timestamp_ms": 1000
                }),
            },
        )
        .await;

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(
            timeout(Duration::from_millis(25), rx.recv()).await.is_err(),
            "discord watch ingress must not enqueue for live dispatch"
        );
        let jsonl = fs::read_to_string(intents).expect("intent jsonl");
        assert!(
            jsonl.contains("\"local_only\":true"),
            "intent must be persisted as local-only JSONL"
        );
    }

    #[tokio::test]
    async fn discord_watch_local_intent_write_failure_rejects_without_enqueueing() {
        let (tx, mut rx) = mpsc::channel(1);
        let dir = tempdir().expect("tempdir");
        let mut config = AppConfig::default();
        config.discord_watch.enabled = true;
        config.discord_watch.gaebal_gajae_user_id = "fixture-gaebal".into();
        config.discord_watch.watched_channels = vec![crate::config::DiscordWatchChannel {
            id: "fixture-general".into(),
            name: "general".into(),
        }];
        config.discord_watch.owner_user_ids = vec!["owner".into()];
        config.discord_watch.state_file = Some(dir.path().join("discord-watch-state.json"));
        config.discord_watch.intent_file = Some(dir.path().to_path_buf());
        let state = AppState {
            config: Arc::new(config),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: dir.path().join("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let response = accept_event(
            &state,
            IncomingEvent {
                kind: "discord.message-create".into(),
                channel: Some("dm".into()),
                mention: None,
                format: None,
                template: None,
                payload: json!({
                    "message_id": "dm1",
                    "channel_id": "dm",
                    "channel_name": "owner-dm",
                    "author_id": "owner",
                    "content": "please sweep",
                    "mentions": [],
                    "direct_message": true,
                    "author_is_owner": false,
                    "timestamp_ms": 1000
                }),
            },
        )
        .await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            timeout(Duration::from_millis(25), rx.recv()).await.is_err(),
            "failed local intent writes must not fall through to dispatch"
        );
    }

    #[tokio::test]
    async fn discord_watch_serializes_concurrent_threshold_updates() {
        let (tx, mut rx) = mpsc::channel(5);
        let dir = tempdir().expect("tempdir");
        let intents = dir.path().join("discord-watch-intents.jsonl");
        let mut config = AppConfig::default();
        config.discord_watch.enabled = true;
        config.discord_watch.gaebal_gajae_user_id = "fixture-gaebal".into();
        config.discord_watch.watched_channels = vec![crate::config::DiscordWatchChannel {
            id: "fixture-general".into(),
            name: "general".into(),
        }];
        config.discord_watch.global_cooldown_ms = 0;
        config.discord_watch.channel_cooldown_ms = 0;
        config.discord_watch.state_file = Some(dir.path().join("discord-watch-state.json"));
        config.discord_watch.intent_file = Some(intents.clone());
        let gaebal = config.discord_watch.gaebal_gajae_user_id.clone();
        let state = AppState {
            config: Arc::new(config),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: dir.path().join("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let event = |id: &str| IncomingEvent {
            kind: "discord.message-create".into(),
            channel: Some("fixture-general".into()),
            mention: None,
            format: None,
            template: None,
            payload: json!({
                "message_id": id,
                "channel_id": "fixture-general",
                "channel_name": "general",
                "author_id": "user",
                "content": format!("<@{gaebal}>"),
                "mentions": [gaebal.as_str()],
                "direct_message": false,
                "author_is_owner": false,
                "timestamp_ms": 1000
            }),
        };

        let (r1, r2, r3, r4, r5) = tokio::join!(
            accept_event(&state, event("m1")),
            accept_event(&state, event("m2")),
            accept_event(&state, event("m3")),
            accept_event(&state, event("m4")),
            accept_event(&state, event("m5")),
        );
        for response in [r1, r2, r3, r4, r5] {
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        }

        assert!(
            timeout(Duration::from_millis(25), rx.recv()).await.is_err(),
            "discord watch ingress must remain local-only under concurrency"
        );
        let jsonl = fs::read_to_string(intents).expect("intent jsonl");
        assert_eq!(jsonl.lines().count(), 1);
        assert!(jsonl.contains("t1-pending-mentions"));
    }

    #[tokio::test]
    async fn post_native_hook_observability_counts_accepted_event() {
        let repo = git_repo();
        let payload = native_payload(repo.path(), "SessionStart");
        let (tx, mut rx) = mpsc::channel(1);
        let observability = new_shared_native_hook_observability();
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: observability.clone(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let response = post_native_hook(State(state), Json(payload))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let queued = rx.recv().await.expect("queued event");
        assert_eq!(queued.kind, "session.started");

        let snapshot = snapshot_shared(&observability);
        assert_eq!(snapshot["totals"]["received"], json!(1));
        assert_eq!(snapshot["totals"]["normalized"], json!(1));
        assert_eq!(snapshot["recent_groups"][0]["provider"], json!("codex"));
    }

    #[tokio::test]
    async fn post_native_hook_observability_counts_rejected_event() {
        let (tx, _rx) = mpsc::channel(1);
        let observability = new_shared_native_hook_observability();
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: observability.clone(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };
        let payload = json!({"provider": "codex", "event_name": "Bogus"});

        let response = post_native_hook(State(state), Json(payload))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let snapshot = snapshot_shared(&observability);
        assert_eq!(snapshot["totals"]["received"], json!(1));
        assert_eq!(snapshot["totals"]["dropped"], json!(1));
        assert_eq!(snapshot["reasons"]["normalization_failed"], json!(1));
    }

    #[tokio::test]
    async fn post_native_hook_observability_counts_non_git_drop() {
        let (tx, mut rx) = mpsc::channel(1);
        let observability = new_shared_native_hook_observability();
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: observability.clone(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };
        let dir = tempdir().expect("tempdir");
        let payload = json!({
            "provider": "codex",
            "event_name": "SessionStart",
            "directory": dir.path(),
            "event_payload": {}
        });

        let response = post_native_hook(State(state), Json(payload))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(rx.try_recv().is_err());

        let snapshot = snapshot_shared(&observability);
        assert_eq!(snapshot["totals"]["received"], json!(1));
        assert_eq!(snapshot["totals"]["normalized"], json!(1));
        assert_eq!(snapshot["totals"]["dropped"], json!(1));
        assert_eq!(snapshot["reasons"]["non_git"], json!(1));
    }

    #[tokio::test]
    async fn post_native_hook_observability_counts_stale_defer() {
        let repo = git_repo();
        let mut payload = native_payload(repo.path(), "PostToolUse");
        payload
            .as_object_mut()
            .unwrap()
            .insert("timestamp".into(), Value::String(stale_rfc3339()));
        let (tx, mut rx) = mpsc::channel(1);
        let observability = new_shared_native_hook_observability();
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: observability.clone(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let response = post_native_hook(State(state), Json(payload))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(rx.try_recv().is_err());

        let snapshot = snapshot_shared(&observability);
        assert_eq!(snapshot["totals"]["received"], json!(1));
        assert_eq!(snapshot["totals"]["normalized"], json!(1));
        assert_eq!(snapshot["totals"]["deferred"], json!(1));
        assert_eq!(snapshot["reasons"]["stale_replay"], json!(1));
    }

    #[tokio::test]
    async fn post_native_hook_accepts_codex_payload_and_queues_normalized_event() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("clawhip");
        std::fs::create_dir_all(&repo).expect("create repo");
        let git = std::process::Command::new("git")
            .args(["init"])
            .current_dir(&repo)
            .output()
            .expect("git init");
        assert!(
            git.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&git.stderr)
        );
        let (tx, mut rx) = mpsc::channel(1);
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };
        let payload = json!({
            "provider": "codex",
            "event_name": "SessionStart",
            "directory": repo,
            "cwd": repo,
            "event_payload": {
                "session_id": "sess-65",
                "cwd": repo
            }
        });

        let response = post_native_hook(State(state), Json(payload))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let response_json: Value = serde_json::from_slice(&body).unwrap();
        let event_id = response_json["event_id"].as_str().unwrap();
        assert!(!event_id.is_empty());
        assert_eq!(response_json["type"], Value::from("session.started"));

        let queued = rx.recv().await.unwrap();
        assert_eq!(queued.kind, "session.started");
        assert_eq!(queued.payload["tool"], Value::from("codex"));
        assert_eq!(queued.payload["session_id"], Value::from("sess-65"));
        assert_eq!(queued.payload["event_id"], Value::from(event_id));
    }

    #[tokio::test]
    async fn post_native_hook_queues_ask_tool_as_session_blocked() {
        let repo = git_repo();
        let mut payload = native_payload(repo.path(), "PreToolUse");
        payload["provider"] = json!("claude-code");
        payload["event_payload"]["tool_name"] = json!("askuserquestion");
        payload["event_payload"]["tool_input"] = json!({
            "question": "Need operator approval?\nDo not dump the full transcript."
        });

        let (response_json, mut rx) = post_native_payload(payload).await;
        assert_eq!(response_json["ok"], json!(true));
        assert_eq!(response_json["type"], json!("session.blocked"));

        let queued = rx.recv().await.expect("queued event");
        assert_eq!(queued.kind, "session.blocked");
        assert_eq!(queued.payload["tool"], json!("claude-code"));
        assert_eq!(queued.payload["agent_name"], json!("claude-code"));
        assert_eq!(queued.payload["route_key"], json!("question.requested"));
        assert_eq!(
            queued.payload["summary"],
            json!("Need operator approval? Do not dump the full transcript.")
        );
        assert_eq!(queued.payload["event_payload"]["redacted"], json!(true));
        assert!(queued.payload["event_payload"].get("tool_input").is_none());
    }

    #[tokio::test]
    async fn post_native_hook_rejects_unsupported_event() {
        let (tx, _rx) = mpsc::channel(1);
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };
        let payload = json!({
            "provider": "claude-code",
            "event_name": "Notification",
            "directory": "/repo/clawhip",
            "event_payload": {}
        });

        let response = post_native_hook(State(state), Json(payload))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let response_json: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            response_json["error"]
                .as_str()
                .is_some_and(|error| error.contains("unsupported native hook event"))
        );
    }

    #[tokio::test]
    async fn post_native_hook_defers_stale_payloads_from_all_trusted_timestamp_paths() {
        let repo = git_repo();
        let cases = [
            vec!["event_timestamp"],
            vec!["timestamp"],
            vec!["observed_at"],
            vec!["created_at"],
            vec!["event_payload", "event_timestamp"],
            vec!["event_payload", "timestamp"],
        ];

        for path in cases {
            let mut payload = native_payload(repo.path(), "PostToolUse");
            insert_timestamp_at_path(&mut payload, &path, stale_rfc3339());

            let (response_json, mut rx) = post_native_payload(payload).await;
            assert_eq!(response_json["ok"], json!(true));
            assert_eq!(response_json["type"], json!("tool.post"));
            assert_eq!(response_json["deferred"], json!(true));
            assert_eq!(response_json["quarantined"], json!(true));
            assert_eq!(response_json["reason"], json!(STALE_NATIVE_REPLAY_REASON));
            assert!(
                rx.try_recv().is_err(),
                "stale payload at {path:?} should not enqueue"
            );
        }
    }

    #[tokio::test]
    async fn post_native_hook_defers_all_replay_sensitive_native_kinds() {
        let repo = git_repo();
        let cases = [
            ("PreToolUse", "tool.pre"),
            ("PostToolUse", "tool.post"),
            ("UserPromptSubmit", "session.prompt-submitted"),
            ("Stop", "session.stopped"),
        ];

        for (event_name, expected_kind) in cases {
            let mut payload = native_payload(repo.path(), event_name);
            payload
                .as_object_mut()
                .unwrap()
                .insert("timestamp".into(), Value::String(stale_rfc3339()));

            let (response_json, mut rx) = post_native_payload(payload).await;
            assert_eq!(response_json["type"], json!(expected_kind));
            assert_eq!(response_json["deferred"], json!(true));
            assert!(
                rx.try_recv().is_err(),
                "{expected_kind} stale replay should not enqueue"
            );
        }
    }

    #[tokio::test]
    async fn post_native_hook_stale_session_started_still_enqueues() {
        let repo = git_repo();
        let mut payload = native_payload(repo.path(), "SessionStart");
        payload
            .as_object_mut()
            .unwrap()
            .insert("timestamp".into(), Value::String(stale_rfc3339()));

        let (response_json, mut rx) = post_native_payload(payload).await;
        assert_eq!(response_json["type"], json!("session.started"));
        assert!(response_json.get("deferred").is_none());
        let queued = rx.recv().await.expect("queued event");
        assert_eq!(queued.kind, "session.started");
    }

    #[tokio::test]
    async fn post_native_hook_preserves_fresh_timestamped_tool_post() {
        let repo = git_repo();
        let mut payload = native_payload(repo.path(), "PostToolUse");
        payload
            .as_object_mut()
            .unwrap()
            .insert("timestamp".into(), Value::String(fresh_rfc3339()));

        let (response_json, mut rx) = post_native_payload(payload).await;
        assert_eq!(response_json["type"], json!("tool.post"));
        assert!(response_json["event_id"].as_str().is_some());
        let queued = rx.recv().await.expect("queued event");
        assert_eq!(queued.kind, "tool.post");
    }

    #[tokio::test]
    async fn post_native_hook_preserves_timestampless_tool_post() {
        let repo = git_repo();
        let payload = native_payload(repo.path(), "PostToolUse");

        let (response_json, mut rx) = post_native_payload(payload).await;
        assert_eq!(response_json["type"], json!("tool.post"));
        assert!(response_json["event_id"].as_str().is_some());
        let queued = rx.recv().await.expect("queued event");
        assert_eq!(queued.kind, "tool.post");
    }

    #[tokio::test]
    async fn post_native_hook_invalid_timestamp_enqueues() {
        let repo = git_repo();
        let mut payload = native_payload(repo.path(), "PostToolUse");
        payload
            .as_object_mut()
            .unwrap()
            .insert("timestamp".into(), Value::String("not-a-time".into()));

        let (response_json, mut rx) = post_native_payload(payload).await;
        assert_eq!(response_json["type"], json!("tool.post"));
        assert!(response_json.get("deferred").is_none());
        let queued = rx.recv().await.expect("queued event");
        assert_eq!(queued.kind, "tool.post");
    }

    #[tokio::test]
    async fn post_native_hook_does_not_treat_stop_context_last_prompt_at_as_event_timestamp() {
        let repo = git_repo();
        let mut payload = native_payload(repo.path(), "Stop");
        payload.as_object_mut().unwrap().insert(
            "stop_context".into(),
            json!({ "last_prompt_at": stale_rfc3339() }),
        );

        let (response_json, mut rx) = post_native_payload(payload).await;
        assert_eq!(response_json["type"], json!("session.stopped"));
        assert!(response_json.get("deferred").is_none());
        let queued = rx.recv().await.expect("queued event");
        assert_eq!(queued.kind, "session.stopped");
    }

    #[tokio::test]
    async fn post_native_hook_accepts_but_drops_non_git_payloads_before_enqueue() {
        let (tx, mut rx) = mpsc::channel(1);
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };
        let dir = tempdir().expect("tempdir");
        let payload = json!({
            "provider": "codex",
            "event_name": "SessionStart",
            "directory": dir.path(),
            "event_payload": {},
            "normalization_outcome": NATIVE_NON_GIT_OUTCOME
        });

        let response = post_native_hook(State(state), Json(payload))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let response_json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response_json["ok"], json!(true));
        assert_eq!(response_json["dropped"], json!(true));
        assert_eq!(response_json["reason"], json!(NATIVE_NON_GIT_OUTCOME));
        assert!(rx.try_recv().is_err(), "non-git payload should not enqueue");
    }

    #[tokio::test]
    async fn list_tmux_returns_registered_sessions_with_metadata() {
        let (tx, _rx) = mpsc::channel(1);
        let registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));
        registry.write().await.insert(
            "issue-105".into(),
            RegisteredTmuxSession {
                session: "issue-105".into(),
                channel: Some("alerts".into()),
                mention: Some("<@123>".into()),
                routing: RoutingMetadata::default(),
                keywords: vec!["error".into()],
                keyword_window_secs: 30,
                stale_minutes: 15,
                format: None,
                registered_at: "2026-04-02T00:00:00Z".into(),
                registration_source: RegistrationSource::CliWatch,
                parent_process: Some(ParentProcessInfo {
                    pid: 4242,
                    name: Some("codex".into()),
                }),
                active_wrapper_monitor: true,
            },
        );
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: registry,
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let response = list_tmux(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let response_json: Value = serde_json::from_slice(&body).unwrap();
        let registrations = response_json.as_array().unwrap();
        assert_eq!(registrations.len(), 1);
        assert_eq!(registrations[0]["session"], Value::from("issue-105"));
        assert_eq!(
            registrations[0]["registration_source"],
            Value::from("cli-watch")
        );
        assert_eq!(
            registrations[0]["registered_at"],
            Value::from("2026-04-02T00:00:00Z")
        );
        assert_eq!(registrations[0]["parent_process"]["pid"], Value::from(4242));
        assert_eq!(
            registrations[0]["parent_process"]["name"],
            Value::from("codex")
        );
    }

    #[tokio::test]
    async fn update_status_returns_no_pending_when_empty() {
        let (tx, _rx) = mpsc::channel(1);
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let response = update_status(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["pending"], Value::Bool(false));
        assert_eq!(json["current_version"], Value::String(VERSION.to_string()));
    }

    #[tokio::test]
    async fn update_status_returns_pending_when_set() {
        let (tx, _rx) = mpsc::channel(1);
        let pending = update::new_shared_pending_update();
        *pending.write().await = Some(update::PendingUpdate {
            current_version: "0.5.4".into(),
            latest_version: "0.6.0".into(),
            release_url: "https://github.com/Yeachan-Heo/clawhip/releases/tag/v0.6.0".into(),
            detected_at: "2026-04-07T00:00:00Z".into(),
        });

        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: pending,
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let response = update_status(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["pending"], Value::Bool(true));
        assert_eq!(json["latest_version"], Value::from("0.6.0"));
        assert_eq!(json["current_version"], Value::from("0.5.4"));
    }

    #[tokio::test]
    async fn approve_returns_error_when_no_pending_update() {
        let (tx, _rx) = mpsc::channel(1);
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let response = approve_update(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], Value::Bool(false));
        assert!(
            json["error"]
                .as_str()
                .unwrap()
                .contains("no pending update")
        );
    }

    #[tokio::test]
    async fn dismiss_clears_pending_update() {
        let (tx, _rx) = mpsc::channel(1);
        let pending = update::new_shared_pending_update();
        *pending.write().await = Some(update::PendingUpdate {
            current_version: "0.5.4".into(),
            latest_version: "0.6.0".into(),
            release_url: "https://example.com".into(),
            detected_at: "2026-04-07T00:00:00Z".into(),
        });

        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: pending.clone(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let response = dismiss_update(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], Value::Bool(true));
        assert_eq!(json["dismissed_version"], Value::from("0.6.0"));
        assert!(pending.read().await.is_none());
    }

    #[tokio::test]
    async fn dismiss_returns_error_when_no_pending_update() {
        let (tx, _rx) = mpsc::channel(1);
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
            cron_state_path: PathBuf::from("cron-state.json"),
            discord_watch_lock: Arc::new(Mutex::new(())),
        };

        let response = dismiss_update(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], Value::Bool(false));
    }
}
