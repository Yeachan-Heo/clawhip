use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router as AxumRouter};
use serde_json::{Value, json};
use tokio::sync::{RwLock, mpsc};

use crate::Result;
use crate::VERSION;
use crate::config::AppConfig;
use crate::dispatch::Dispatcher;
use crate::event::compat::from_incoming_event;
use crate::events::{IncomingEvent, normalize_event};
use crate::render::{DefaultRenderer, Renderer};
use crate::router::Router;
use crate::sink::{DiscordSink, Sink, SlackSink};
use crate::source::{
    GitHubSource, GitSource, RegisteredTmuxSession, SharedTmuxRegistry, Source, TmuxSource,
};

const EVENT_QUEUE_CAPACITY: usize = 256;

#[derive(Clone)]
struct AppState {
    config: Arc<AppConfig>,
    port: u16,
    tx: mpsc::Sender<IncomingEvent>,
    tmux_registry: SharedTmuxRegistry,
}

pub async fn run(config: Arc<AppConfig>, port_override: Option<u16>) -> Result<()> {
    config.validate()?;
    let token_source = config.discord_token_source();
    println!("clawhip v{VERSION} starting (token_source: {token_source})");

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

    tokio::spawn(async move {
        let mut dispatcher = Dispatcher::new(rx, router, renderer, sinks);
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

    let app = AxumRouter::new()
        .route("/health", get(health))
        .route("/api/status", get(status))
        .route("/event", post(post_event))
        .route("/api/event", post(post_event))
        .route("/events", post(post_event))
        .route("/api/tmux/register", post(register_tmux))
        .route("/github", post(post_github));
    let port = port_override.unwrap_or(config.daemon.port);

    let app = app.with_state(AppState {
        config: config.clone(),
        port,
        tx,
        tmux_registry,
    });
    let addr: SocketAddr = format!("{}:{}", config.daemon.bind_host, port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!(
        "clawhip daemon v{VERSION} listening on http://{} (token_source: {token_source})",
        listener.local_addr()?
    );
    axum::serve(listener, app).await?;
    Ok(())
}

fn spawn_source<S>(source: S, tx: mpsc::Sender<IncomingEvent>)
where
    S: Source + Send + Sync + 'static,
{
    tokio::spawn(async move {
        println!("clawhip source '{}' starting", source.name());
        if let Err(error) = source.run(tx).await {
            eprintln!("clawhip source '{}' stopped: {error}", source.name());
        }
    });
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let registered = state.tmux_registry.read().await.len();
    Json(health_payload(
        state.config.as_ref(),
        state.port,
        registered,
    ))
}

fn health_payload(config: &AppConfig, port: u16, registered_tmux_sessions: usize) -> Value {
    json!({
        "ok": true,
        "version": VERSION,
        "token_source": config.discord_token_source(),
        "webhook_routes_configured": config.has_webhook_routes(),
        "port": port,
        "daemon_base_url": config.daemon.base_url,
        "configured_git_monitors": config.monitors.git.repos.len(),
        "configured_tmux_monitors": config.monitors.tmux.sessions.len(),
        "registered_tmux_sessions": registered_tmux_sessions,
    })
}

async fn status(State(state): State<AppState>) -> impl IntoResponse {
    health(State(state)).await
}

async fn post_event(
    State(state): State<AppState>,
    Json(event): Json<IncomingEvent>,
) -> impl IntoResponse {
    let event = normalize_event(event);
    let envelope = match from_incoming_event(&event) {
        Ok(envelope) => envelope,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": error.to_string()})),
            )
                .into_response();
        }
    };

    match enqueue_event(&state.tx, event.clone()).await {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "ok": true,
                "type": event.kind,
                "event_id": envelope.id.to_string(),
            })),
        )
            .into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": error.to_string()})),
        )
            .into_response(),
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

async fn enqueue_event(tx: &mpsc::Sender<IncomingEvent>, event: IncomingEvent) -> Result<()> {
    tx.send(event)
        .await
        .map_err(|error| format!("event queue unavailable: {error}").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use axum::body::to_bytes;

    #[test]
    fn health_payload_includes_version_and_token_source() {
        let mut config = AppConfig::default();
        config.providers.discord.bot_token = Some("config-token".into());
        config.monitors.git.repos.push(Default::default());
        config.monitors.tmux.sessions.push(Default::default());

        let payload = health_payload(&config, 25294, 3);

        assert_eq!(payload["ok"], Value::Bool(true));
        assert_eq!(payload["version"], Value::String(VERSION.to_string()));
        assert_eq!(payload["token_source"], Value::String("config".to_string()));
        assert_eq!(payload["port"], Value::from(25294));
        assert_eq!(payload["configured_git_monitors"], Value::from(1));
        assert_eq!(payload["configured_tmux_monitors"], Value::from(1));
        assert_eq!(payload["registered_tmux_sessions"], Value::from(3));
    }

    #[tokio::test]
    async fn post_event_returns_event_id_and_preserves_normalized_metadata() {
        let (tx, mut rx) = mpsc::channel(1);
        let state = AppState {
            config: Arc::new(AppConfig::default()),
            port: 25294,
            tx,
            tmux_registry: Arc::new(RwLock::new(HashMap::new())),
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
}
