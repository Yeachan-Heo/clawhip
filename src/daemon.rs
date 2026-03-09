use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router as AxumRouter};
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::Result;
use crate::VERSION;
use crate::config::AppConfig;
use crate::discord::DiscordClient;
use crate::events::{IncomingEvent, normalize_event};
use crate::monitor::{self, RegisteredTmuxSession, SharedTmuxRegistry};
use crate::router::Router;

#[derive(Clone)]
struct AppState {
    config: Arc<AppConfig>,
    port: u16,
    router: Arc<Router>,
    discord: Arc<DiscordClient>,
    tmux_registry: SharedTmuxRegistry,
}

pub async fn run(config: Arc<AppConfig>, port_override: Option<u16>) -> Result<()> {
    let token_source = config.discord_token_source();
    println!("clawhip v{VERSION} starting (token_source: {token_source})");

    let discord = Arc::new(DiscordClient::from_config(config.clone())?);
    let router = Arc::new(Router::new(config.clone()));
    let tmux_registry: SharedTmuxRegistry = Arc::new(RwLock::new(HashMap::new()));

    tokio::spawn(monitor::run(
        config.clone(),
        router.clone(),
        discord.clone(),
        tmux_registry.clone(),
    ));

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
        router,
        discord,
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
    match state.router.dispatch(&event, state.discord.as_ref()).await {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(json!({"ok": true, "type": event.kind})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
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

    let dispatch_result = match event_name {
        "issues" if action == "opened" => {
            let repo = payload
                .pointer("/repository/full_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown/unknown")
                .to_string();
            let number = payload
                .pointer("/issue/number")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let title = payload
                .pointer("/issue/title")
                .and_then(Value::as_str)
                .unwrap_or("Untitled issue")
                .to_string();
            let event = IncomingEvent::github_issue_opened(repo, number, title, None);
            state.router.dispatch(&event, state.discord.as_ref()).await
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
            let merged = payload
                .pointer("/pull_request/merged")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let transition = match action {
                "opened" => Some(("<new>".to_string(), "open".to_string())),
                "reopened" => Some(("closed".to_string(), "open".to_string())),
                "closed" if merged => Some(("open".to_string(), "merged".to_string())),
                "closed" => Some(("open".to_string(), "closed".to_string())),
                _ => None,
            };
            if let Some((old_status, new_status)) = transition {
                let event = IncomingEvent::github_pr_status_changed(
                    repo, number, title, old_status, new_status, url, None,
                );
                state.router.dispatch(&event, state.discord.as_ref()).await
            } else {
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({"ok": true, "ignored": true, "reason": "unsupported pull_request action"})),
                )
                    .into_response();
            }
        }
        _ => {
            return (
                StatusCode::ACCEPTED,
                Json(json!({"ok": true, "ignored": true, "reason": "unsupported event"})),
            )
                .into_response();
        }
    };

    match dispatch_result {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"ok": true}))).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": error.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn health_payload_includes_version_and_token_source() {
        let mut config = AppConfig::default();
        config.discord.bot_token = Some("config-token".into());
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
}
