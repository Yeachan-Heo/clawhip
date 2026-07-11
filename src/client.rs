use reqwest::StatusCode;
use serde::Serialize;
use serde_json::Value;

use crate::Result;
use crate::config::AppConfig;
use crate::events::IncomingEvent;
use crate::source::tmux::RegisteredTmuxSession;
use crate::source::tmux::{
    LaneDeliveryMutation, LaneDetail, LaneEvidenceMutation, LaneRegistrationInput,
    LaneRetirementMutation, LaneSnapshot, LaneVerificationMutation,
};

#[derive(Clone)]
pub struct DaemonClient {
    http: reqwest::Client,
    base_url: String,
}

impl DaemonClient {
    pub fn from_config(config: &AppConfig) -> Self {
        Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client construction"),
            base_url: config.daemon_base_url().trim_end_matches('/').to_string(),
        }
    }

    pub async fn send_event(&self, event: &IncomingEvent) -> Result<()> {
        self.post_json("/event", event).await.map(|_| ())
    }

    pub async fn send_native_hook(&self, envelope: &Value) -> Result<Value> {
        self.post_json("/api/native/hook", envelope).await
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

    pub async fn get_update_status(&self) -> Result<Value> {
        let response = self
            .http
            .get(format!("{}/api/update/status", self.base_url))
            .send()
            .await?;
        if response.status().is_success() {
            Ok(response.json().await?)
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(format!("daemon update status failed with {status}: {body}").into())
        }
    }

    pub async fn post_update_action(&self, action: &str) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}/api/update/{action}", self.base_url))
            .json(&serde_json::json!({}))
            .send()
            .await?;
        if response.status().is_success() {
            Ok(response.json().await?)
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(format!("daemon update {action} failed with {status}: {body}").into())
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

    pub async fn list_lanes(&self) -> Result<Vec<LaneSnapshot>> {
        self.private_get_json("/api/lane").await
    }

    pub async fn claim_lane(
        &self,
        session: &str,
        generation_id: &str,
        executor_id: &str,
        expected_revision: u64,
    ) -> Result<LaneSnapshot> {
        self.private_post_typed("/api/lane/claim", &serde_json::json!({"session": session, "generation_id": generation_id, "executor_id": executor_id, "expected_revision": expected_revision})).await
    }

    pub async fn update_lane_evidence(&self, input: &LaneEvidenceMutation) -> Result<LaneSnapshot> {
        self.private_post_typed("/api/lane/evidence", input).await
    }

    pub async fn update_lane_workflow(
        &self,
        input: &crate::source::tmux::LaneWorkflowMutation,
    ) -> Result<LaneSnapshot> {
        self.private_post_typed("/api/lane/workflow", input).await
    }

    async fn private_get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.ensure_loopback_daemon()?;
        self.get_json(path).await
    }
    async fn private_post_typed<T: serde::de::DeserializeOwned, P: Serialize>(
        &self,
        path: &str,
        payload: &P,
    ) -> Result<T> {
        self.ensure_loopback_daemon()?;
        self.post_typed(path, payload).await
    }
    fn ensure_loopback_daemon(&self) -> Result<()> {
        let url = reqwest::Url::parse(&self.base_url)?;
        let host = url
            .host_str()
            .ok_or("private daemon URL lacks host")?
            .trim_matches(['[', ']']);
        let allowed = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .ok()
                .is_some_and(|ip| match ip {
                    std::net::IpAddr::V4(ip) => ip.is_loopback(),
                    std::net::IpAddr::V6(ip) => {
                        ip.is_loopback() || ip.to_ipv4().is_some_and(|mapped| mapped.is_loopback())
                    }
                });
        if allowed {
            Ok(())
        } else {
            Err("private lane requests require a loopback daemon URL".into())
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .http
            .get(format!("{}{}", self.base_url, path))
            .send()
            .await?;
        if response.status().is_success() {
            Ok(response.json().await?)
        } else {
            Err(format!("daemon lane request failed with {}", response.status()).into())
        }
    }
    async fn lane_error_response(
        response: reqwest::Response,
    ) -> Box<dyn std::error::Error + Send + Sync> {
        let status = response.status();
        let payload = response.json::<Value>().await.unwrap_or(Value::Null);
        let message = if payload.get("error_code").and_then(Value::as_str) == Some("runtime-active")
        {
            "lane runtime remains active; quiesce it before retirement".to_owned()
        } else {
            format!("daemon lane request failed with {status}")
        };
        Box::new(std::io::Error::other(message))
    }

    async fn post_typed<T: serde::de::DeserializeOwned, P: Serialize>(
        &self,
        path: &str,
        payload: &P,
    ) -> Result<T> {
        let response = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .json(payload)
            .send()
            .await?;
        if response.status().is_success() {
            Ok(response.json().await?)
        } else {
            Err(Self::lane_error_response(response).await)
        }
    }

    pub async fn lane_detail(&self, session: &str) -> Result<LaneDetail> {
        self.ensure_loopback_daemon()?;
        let mut url = reqwest::Url::parse(&self.base_url)?;
        url.path_segments_mut()
            .map_err(|_| "private daemon URL cannot accept path segments")?
            .extend(["api", "lane", "detail", session]);
        let response = self.http.get(url).send().await?;
        if response.status().is_success() {
            Ok(response.json().await?)
        } else {
            Err(format!("daemon lane request failed with {}", response.status()).into())
        }
    }
    pub async fn register_lane(&self, input: &LaneRegistrationInput) -> Result<LaneDetail> {
        self.private_post_typed("/api/lane/register", input).await
    }
    pub async fn record_lane_verification(
        &self,
        input: &LaneVerificationMutation,
    ) -> Result<LaneSnapshot> {
        self.private_post_typed("/api/lane/verification", input)
            .await
    }
    pub async fn record_lane_delivery(&self, input: &LaneDeliveryMutation) -> Result<LaneSnapshot> {
        self.private_post_typed("/api/lane/delivery", input).await
    }
    pub async fn retire_lane(&self, input: &LaneRetirementMutation) -> Result<LaneSnapshot> {
        self.private_post_typed("/api/lane/retire", input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::AsyncWriteExt;
    use tokio::time::{Duration, timeout};

    #[test]
    fn private_lane_client_rejects_non_loopback_urls() {
        let remote = DaemonClient {
            http: reqwest::Client::new(),
            base_url: "http://example.com".into(),
        };
        let ipv4 = DaemonClient {
            http: reqwest::Client::new(),
            base_url: "http://127.0.0.1:8080".into(),
        };
        let ipv6 = DaemonClient {
            http: reqwest::Client::new(),
            base_url: "http://[::1]:8080".into(),
        };
        assert!(remote.ensure_loopback_daemon().is_err());
        assert!(ipv4.ensure_loopback_daemon().is_ok());
        assert!(ipv6.ensure_loopback_daemon().is_ok());
    }

    #[test]
    fn lane_detail_path_segments_escape_unsafe_session_text() {
        let mut url = reqwest::Url::parse("http://127.0.0.1:8080").unwrap();
        url.path_segments_mut()
            .unwrap()
            .extend(["api", "lane", "detail", "a/b?c#d"]);
        assert_eq!(url.path(), "/api/lane/detail/a%2Fb%3Fc%23d");
    }

    #[tokio::test]
    async fn private_lane_requests_do_not_follow_redirects() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target.local_addr().unwrap();
        let target_hits = Arc::new(AtomicUsize::new(0));
        let target_counter = target_hits.clone();
        let target_task = tokio::spawn(async move {
            if timeout(Duration::from_millis(150), target.accept())
                .await
                .is_ok()
            {
                target_counter.fetch_add(1, Ordering::SeqCst);
            }
        });
        let redirector = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirector_address = redirector.local_addr().unwrap();
        let redirect_task = tokio::spawn(async move {
            let (mut stream, _) = redirector.accept().await.unwrap();
            let response = format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{target_address}/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let client = DaemonClient {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            base_url: format!("http://{redirector_address}"),
        };
        assert!(client.lane_detail("safe-session").await.is_err());
        redirect_task.await.unwrap();
        target_task.await.unwrap();
        assert_eq!(target_hits.load(Ordering::SeqCst), 0);
    }
}
