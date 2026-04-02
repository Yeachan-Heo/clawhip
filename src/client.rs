use reqwest::StatusCode;
use serde::Serialize;
use serde_json::Value;

use crate::Result;
use crate::config::AppConfig;
use crate::events::IncomingEvent;
use crate::source::tmux::RegisteredTmuxSession;

#[derive(Clone)]
pub struct DaemonClient {
    http: reqwest::Client,
    base_url: String,
}

impl DaemonClient {
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: config.daemon_base_url().trim_end_matches('/').to_string(),
        }
    }

    pub async fn send_event(&self, event: &IncomingEvent) -> Result<()> {
        self.post_json("/event", event).await.map(|_| ())
    }

    pub async fn send_omx_hook(&self, envelope: &Value) -> Result<Value> {
        self.post_json("/api/omx/hook", envelope).await
    }

    pub async fn register_tmux(&self, registration: &RegisteredTmuxSession) -> Result<()> {
        self.post_json("/api/tmux/register", registration)
            .await
            .map(|_| ())
    }

    pub async fn list_tmux(&self) -> Result<Vec<RegisteredTmuxSession>> {
        let response = self
            .http
            .get(format!("{}/api/tmux", self.base_url))
            .send()
            .await?;
        if response.status().is_success() {
            Ok(response.json().await?)
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(format!("daemon tmux list failed with {status}: {body}").into())
        }
    }

    pub async fn health(&self) -> Result<Value> {
        let response = self
            .http
            .get(format!("{}/health", self.base_url))
            .send()
            .await?;
        if response.status().is_success() {
            Ok(response.json().await?)
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(format!("daemon health check failed with {status}: {body}").into())
        }
    }

    async fn post_json<T: Serialize>(&self, path: &str, payload: &T) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .json(payload)
            .send()
            .await?;
        if response.status() == StatusCode::ACCEPTED || response.status().is_success() {
            Ok(response.json().await.unwrap_or(Value::Null))
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(format!("daemon request failed with {status}: {body}").into())
        }
    }
}
