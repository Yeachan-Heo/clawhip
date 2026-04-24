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
use tokio::sync::{RwLock, mpsc};

use crate::Result;
use crate::VERSION;
use crate::config::AppConfig;
use crate::cron::CronSource;
use crate::dispatch::Dispatcher;
use crate::event::compat::from_incoming_event;
use crate::events::{IncomingEvent, MessageFormat, normalize_event};
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
use crate::sink::{DiscordSink, Sink, SlackSink};
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
    spawn_source(CronSource::new(config.clone(), cron_state_path), tx.clone());

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

    match enqueue_event(&state.tx, event.clone()).await {
        Ok(()) => {
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
            },
            rx,
        )
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
    async fn post_native_hook_rejects_unsupported_event() {
        let (tx, _rx) = mpsc::channel(1);
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
            pending_update: update::new_shared_pending_update(),
            native_observability: new_shared_native_hook_observability(),
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
        };

        let response = dismiss_update(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["ok"], Value::Bool(false));
    }
}
