use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};

use crate::Result;
use crate::VERSION;
use crate::config::AppConfig;
use crate::events::{IncomingEvent, MessageFormat};

const GITHUB_API_BASE: &str = "https://api.github.com";
const GITHUB_REPO: &str = "Yeachan-Heo/clawhip";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UpdateConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default = "default_check_interval_secs")]
    pub check_interval_secs: u64,
    #[serde(default)]
    pub auto_restart: bool,
    /// Absolute path to the clawhip git checkout used for self-update.
    /// Required for daemon/systemd contexts where CWD is not the repo root.
    #[serde(default)]
    pub repo_root: Option<String>,
}

fn default_check_interval_secs() -> u64 {
    3600
}

impl UpdateConfig {
    pub fn is_empty(&self) -> bool {
        !self.enabled && self.channel.is_none() && self.repo_root.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingUpdate {
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
    pub detected_at: String,
}

pub type SharedPendingUpdate = Arc<RwLock<Option<PendingUpdate>>>;

pub fn new_shared_pending_update() -> SharedPendingUpdate {
    Arc::new(RwLock::new(None))
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
}

pub async fn run_checker(
    config: Arc<AppConfig>,
    tx: mpsc::Sender<IncomingEvent>,
    pending: SharedPendingUpdate,
) {
    if !config.update.enabled {
        return;
    }

    let check_secs = config.update.check_interval_secs.max(60);
    let mut tick = interval(Duration::from_secs(check_secs));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let http = reqwest::Client::builder()
        .user_agent(format!("clawhip/{VERSION}"))
        .build()
        .expect("http client");

    println!("clawhip update checker starting (interval: {check_secs}s)");

    loop {
        tick.tick().await;

        if pending.read().await.is_some() {
            continue;
        }

        match check_latest_release(&http).await {
            Ok(Some(release)) if version_is_newer(&release.tag_name) => {
                let latest = normalize_version(&release.tag_name);
                let update = PendingUpdate {
                    current_version: VERSION.to_string(),
                    latest_version: latest.clone(),
                    release_url: release.html_url.clone(),
                    detected_at: now_rfc3339(),
                };
                *pending.write().await = Some(update);

                let message = format!(
                    "clawhip update available: v{VERSION} \u{2192} v{latest}\n\
                     Approve via: POST /api/update/approve  or  clawhip update approve\n\
                     {}",
                    release.html_url,
                );
                let event = IncomingEvent::custom(config.update.channel.clone(), message)
                    .with_format(Some(MessageFormat::Alert));

                if let Err(error) = tx.send(event).await {
                    eprintln!("clawhip update checker: failed to send notification: {error}");
                }
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("clawhip update checker: release check failed: {error}");
            }
        }
    }
}

pub async fn check_latest_version(http: &reqwest::Client) -> Result<Option<(String, String)>> {
    match check_latest_release(http).await? {
        Some(release) => {
            let version = normalize_version(&release.tag_name);
            Ok(Some((version, release.html_url)))
        }
        None => Ok(None),
    }
}

async fn check_latest_release(http: &reqwest::Client) -> Result<Option<GitHubRelease>> {
    let url = format!("{GITHUB_API_BASE}/repos/{GITHUB_REPO}/releases/latest");
    let response = http.get(&url).send().await?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("GitHub API {status}: {body}").into());
    }

    Ok(Some(response.json().await?))
}

pub async fn approve_update(
    pending: &SharedPendingUpdate,
    config: &AppConfig,
    tx: &mpsc::Sender<IncomingEvent>,
) -> Result<PendingUpdate> {
    let update = pending
        .write()
        .await
        .take()
        .ok_or("no pending update to approve")?;

    let auto_restart = config.update.auto_restart;
    let repo_root = config.effective_update_repo_root().map(str::to_owned);
    let config_path = config.config_path();
    let channel = config.update.channel.clone();
    let result_version = update.latest_version.clone();

    let result = tokio::task::spawn_blocking(move || {
        crate::lifecycle::update_from_repo(repo_root.as_deref(), auto_restart, &config_path)
    })
    .await
    .map_err(|error| format!("update task panicked: {error}"))?;

    match &result {
        Ok(()) => {
            let message =
                format!("clawhip updated to v{result_version} successfully. Restart may follow.");
            let event =
                IncomingEvent::custom(channel, message).with_format(Some(MessageFormat::Alert));
            let _ = tx.send(event).await;
        }
        Err(error) => {
            let message = format!("clawhip update to v{result_version} failed: {error}");
            let event =
                IncomingEvent::custom(channel, message).with_format(Some(MessageFormat::Alert));
            let _ = tx.send(event).await;
        }
    }

    result.map(|()| update)
}

pub async fn dismiss_update(pending: &SharedPendingUpdate) -> Result<PendingUpdate> {
    pending
        .write()
        .await
        .take()
        .ok_or_else(|| "no pending update to dismiss".into())
}

fn normalize_version(tag: &str) -> String {
    tag.strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag)
        .to_string()
}

pub fn version_is_newer(tag: &str) -> bool {
    let latest = normalize_version(tag);
    compare_versions(&latest, VERSION)
        .is_some_and(|ordering| ordering == std::cmp::Ordering::Greater)
}

fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    let parse = |v: &str| -> Option<Vec<u64>> {
        v.split('.')
            .map(|segment| segment.parse::<u64>().ok())
            .collect()
    };

    let a_parts = parse(a)?;
    let b_parts = parse(b)?;
    let max_len = a_parts.len().max(b_parts.len());

    for i in 0..max_len {
        let a_val = a_parts.get(i).copied().unwrap_or(0);
        let b_val = b_parts.get(i).copied().unwrap_or(0);
        match a_val.cmp(&b_val) {
            std::cmp::Ordering::Equal => continue,
            other => return Some(other),
        }
    }

    Some(std::cmp::Ordering::Equal)
}

fn now_rfc3339() -> String {
    let now = time::OffsetDateTime::now_utc();
    now.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_newer() {
        assert!(version_is_newer_pair("0.6.0", "0.5.4"));
        assert!(version_is_newer_pair("0.5.5", "0.5.4"));
        assert!(version_is_newer_pair("1.0.0", "0.9.9"));
    }

    #[test]
    fn version_comparison_not_newer() {
        assert!(!version_is_newer_pair("0.5.4", "0.5.4"));
        assert!(!version_is_newer_pair("0.5.3", "0.5.4"));
        assert!(!version_is_newer_pair("0.4.0", "0.5.4"));
    }

    #[test]
    fn version_comparison_handles_prefix() {
        assert_eq!(normalize_version("v0.5.5"), "0.5.5");
        assert_eq!(normalize_version("V0.5.5"), "0.5.5");
        assert_eq!(normalize_version("0.5.5"), "0.5.5");
    }

    #[test]
    fn version_comparison_different_lengths() {
        assert!(version_is_newer_pair("0.5.4.1", "0.5.4"));
        assert!(!version_is_newer_pair("0.5.4", "0.5.4.1"));
    }

    #[test]
    fn compare_versions_equal() {
        assert_eq!(
            compare_versions("0.5.4", "0.5.4"),
            Some(std::cmp::Ordering::Equal)
        );
    }

    #[test]
    fn compare_versions_rejects_non_numeric() {
        assert_eq!(compare_versions("abc", "0.5.4"), None);
    }

    #[test]
    fn update_config_is_empty_when_default() {
        assert!(UpdateConfig::default().is_empty());
    }

    #[test]
    fn update_config_is_not_empty_when_enabled() {
        let config = UpdateConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(!config.is_empty());
    }

    #[test]
    fn default_check_interval() {
        assert_eq!(default_check_interval_secs(), 3600);
    }

    #[tokio::test]
    async fn pending_update_lifecycle() {
        let pending = new_shared_pending_update();
        assert!(pending.read().await.is_none());

        *pending.write().await = Some(PendingUpdate {
            current_version: "0.5.4".into(),
            latest_version: "0.5.5".into(),
            release_url: "https://example.com".into(),
            detected_at: "2026-04-07T00:00:00Z".into(),
        });
        assert!(pending.read().await.is_some());

        let dismissed = dismiss_update(&pending).await.expect("dismiss");
        assert_eq!(dismissed.latest_version, "0.5.5");
        assert!(pending.read().await.is_none());
    }

    #[tokio::test]
    async fn dismiss_empty_pending_returns_error() {
        let pending = new_shared_pending_update();
        assert!(dismiss_update(&pending).await.is_err());
    }

    fn version_is_newer_pair(latest: &str, current: &str) -> bool {
        compare_versions(latest, current).is_some_and(|o| o == std::cmp::Ordering::Greater)
    }
}
