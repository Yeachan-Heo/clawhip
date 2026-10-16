use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, LINK, USER_AGENT};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::Result;
use crate::config::{AppConfig, GitRepoMonitor};
use crate::event::{GitHubPRCommentEvent, GitHubPRReviewCommentEvent, GitHubPRReviewEvent};
use crate::events::IncomingEvent;
use crate::source::Source;
use crate::source::git::{GitSnapshot, repo_display_name, snapshot_git_repo};
use crate::telemetry;

const GH_AUTH_TIMEOUT: Duration = Duration::from_secs(2);
const GITHUB_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(60);
/// Format the current UTC time as ISO 8601 with 'Z' suffix (e.g., "2026-10-15T10:00:00Z")
fn current_utc_iso8601() -> String {
    use time::format_description::well_known::Iso8601;
    time::OffsetDateTime::now_utc()
        .format(&Iso8601::DEFAULT)
        .unwrap_or_else(|_| "2000-01-01T00:00:00Z".to_string())
}

/// J1 fix: Subtract 1 second from an ISO 8601 timestamp.
/// GitHub's `since` parameter is strictly-after (exclusive), so we query with
/// `since = cursor - 1s` and rely on dedup to handle boundary-second comments.
/// Returns the input if parsing fails.
fn subtract_one_second(timestamp: &str) -> String {
    use time::Duration as TimeDuration;
    use time::OffsetDateTime;
    use time::format_description::well_known::Iso8601;

    match OffsetDateTime::parse(timestamp, &Iso8601::DEFAULT) {
        Ok(dt) => {
            if let Some(earlier) = dt.checked_sub(TimeDuration::seconds(1)) {
                earlier
                    .format(&Iso8601::DEFAULT)
                    .unwrap_or_else(|_| timestamp.to_string())
            } else {
                timestamp.to_string()
            }
        }
        Err(_) => timestamp.to_string(),
    }
}

// MoveFileExW with write-through and the following file flush can exceed the
// old 200 ms retry window on loaded Windows runners.
const BASELINE_LOCK_MAX_WAIT: Duration = Duration::from_secs(5);
const BASELINE_LOCK_RETRY_DELAY: Duration = Duration::from_millis(5);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GitHubMonitorAuthStatus {
    pub ready: bool,
    pub source: &'static str,
    pub error: Option<&'static str>,
}

pub type SharedGitHubMonitorAuthStatus = Arc<Mutex<GitHubMonitorAuthStatus>>;

pub fn new_shared_github_monitor_auth_status() -> SharedGitHubMonitorAuthStatus {
    static STATUS: OnceLock<SharedGitHubMonitorAuthStatus> = OnceLock::new();
    STATUS
        .get_or_init(|| {
            Arc::new(Mutex::new(GitHubMonitorAuthStatus {
                ready: false,
                source: "unavailable",
                error: Some("GitHub monitor authentication has not been checked"),
            }))
        })
        .clone()
}

pub fn snapshot_github_monitor_auth_status(
    status: &SharedGitHubMonitorAuthStatus,
) -> GitHubMonitorAuthStatus {
    status
        .lock()
        .map(|value| value.clone())
        .unwrap_or(GitHubMonitorAuthStatus {
            ready: false,
            source: "unavailable",
            error: Some("GitHub monitor authentication status is unavailable"),
        })
}

fn explicit_monitor_github_token(config: &AppConfig) -> Option<(String, &'static str)> {
    explicit_monitor_github_token_from(
        std::env::var("CLAWHIP_GITHUB_TOKEN").ok(),
        config.monitors.github_token.clone(),
    )
}

fn explicit_monitor_github_token_from(
    environment: Option<String>,
    configured: Option<String>,
) -> Option<(String, &'static str)> {
    environment
        .filter(|value| !value.trim().is_empty())
        .map(|value| (value, "environment"))
        .or_else(|| {
            configured
                .filter(|value| !value.trim().is_empty())
                .map(|value| (value, "config"))
        })
}

pub(crate) async fn resolve_monitor_github_token(
    config: &AppConfig,
) -> (Option<String>, GitHubMonitorAuthStatus) {
    if let Some((token, source)) = explicit_monitor_github_token(config) {
        return (
            Some(token),
            GitHubMonitorAuthStatus {
                ready: true,
                source,
                error: None,
            },
        );
    }

    if let Ok(Ok(output)) = tokio::time::timeout(
        GH_AUTH_TIMEOUT,
        tokio::process::Command::new("gh")
            .args(["auth", "token"])
            .output(),
    )
    .await
        && output.status.success()
    {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !token.is_empty() {
            return (
                Some(token),
                GitHubMonitorAuthStatus {
                    ready: true,
                    source: "gh_cli",
                    error: None,
                },
            );
        }
    }

    (
        None,
        GitHubMonitorAuthStatus {
            ready: false,
            source: "unavailable",
            error: Some(
                "No GitHub monitor credential available; set CLAWHIP_GITHUB_TOKEN, configure monitors.github_token, or run gh auth login",
            ),
        },
    )
}

#[derive(Default)]
struct GitHubRequestBackoff {
    until: Option<std::time::Instant>,
    reported: bool,
}

pub(crate) struct GitHubClient {
    pub(crate) http: reqwest::Client,
    backoff: Mutex<GitHubRequestBackoff>,
}

thread_local! {
    static SYNC_FILE_CALLS: Cell<u32> = const { Cell::new(0) };
}

static WINDOW_GENERATION_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct GitHubSource {
    config: Arc<AppConfig>,
    ci_baseline_path: Option<PathBuf>,
    auth_status: SharedGitHubMonitorAuthStatus,
}

impl GitHubSource {
    /// Restarts and re-enrollments reuse a persisted terminal-run baseline so
    /// historical workflow results never replay as fresh events (#317).
    #[allow(dead_code)]
    pub fn with_ci_baseline_path(config: Arc<AppConfig>, ci_baseline_path: PathBuf) -> Self {
        Self::with_ci_baseline_path_and_auth(
            config,
            ci_baseline_path,
            new_shared_github_monitor_auth_status(),
        )
    }

    pub fn with_ci_baseline_path_and_auth(
        config: Arc<AppConfig>,
        ci_baseline_path: PathBuf,
        auth_status: SharedGitHubMonitorAuthStatus,
    ) -> Self {
        Self {
            config,
            ci_baseline_path: Some(ci_baseline_path),
            auth_status,
        }
    }
}

#[async_trait::async_trait]
impl Source for GitHubSource {
    fn name(&self) -> &str {
        "github"
    }

    async fn run(&self, tx: mpsc::Sender<IncomingEvent>) -> Result<()> {
        let (token, auth_status) = resolve_monitor_github_token(self.config.as_ref()).await;
        if let Ok(mut status) = self.auth_status.lock() {
            *status = auth_status;
        }
        if token.is_none() {
            eprintln!(
                "clawhip source github: no monitor credential; set CLAWHIP_GITHUB_TOKEN, configure monitors.github_token, or run gh auth login"
            );
        }
        let github_client = match build_github_client(token) {
            Ok(client) => Some(client),
            Err(error) => {
                eprintln!("clawhip source github: failed to build GitHub client: {error}");
                None
            }
        };
        let ci_baseline_path = self.ci_baseline_path.clone();
        let mut ci_baseline = load_ci_baseline(ci_baseline_path.as_deref());
        let mut state = HashMap::new();

        loop {
            run_github_poll_cycle(
                self.config.as_ref(),
                github_client.as_ref(),
                &tx,
                &mut state,
                &mut ci_baseline,
                ci_baseline_path.as_deref(),
            )
            .await;
            sleep(Duration::from_secs(
                self.config.monitors.poll_interval_secs.max(1),
            ))
            .await;
        }
    }
}

/// Persisted terminal-run identity per monitored GitHub repository. Survives
/// restart and re-enrollment so historical workflow results never replay as
/// fresh events (#317). Bounded: at most [`MAX_BASELINE_RUNS_PER_REPO`]
/// records per repo, evicted oldest-first by GitHub timestamps / run ids.
#[derive(Default, Clone)]
struct CIBaseline {
    repos: HashMap<String, RepoCIBaseline>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct RepoCIBaseline {
    /// Terminal run records observed for this repo, oldest evicted first.
    #[serde(default, deserialize_with = "deserialize_terminal_runs")]
    terminal_runs: VecDeque<TerminalRunRecord>,
    /// Durable outbox: events persisted before publish and retried after a
    /// crash between the write and the channel send.
    #[serde(default)]
    pending: Vec<PendingCiDelivery>,
    /// Alias-aware ACK records. Compaction retires stale writers via
    /// `min_writer_epoch` instead of dropping tombstones while they can
    /// still be merged back in.
    #[serde(default, deserialize_with = "deserialize_acked")]
    acked: VecDeque<AckedDelivery>,
    /// Exhausted outbox entries that will not be retried.
    #[serde(default)]
    dead_letter: VecDeque<PendingCiDelivery>,
    #[serde(default)]
    epoch: u64,
    #[serde(default)]
    min_writer_epoch: u64,
    #[serde(default)]
    ci_established: bool,
    /// Durable identity set of the terminal runs in the last authoritative
    /// fetched window. Pinned against cap eviction so the live window —
    /// including an oversized fetched window (#317) — always survives merge
    /// and reload. Canonicalized, deduplicated, hard-capped at
    /// [`MAX_CURRENT_WINDOW_IDS_PER_SCOPE`] per scope; sibling scopes are
    /// retained independently and are never evicted by a global union cap.
    /// Superseded only by an epoch-unfenced
    /// writer whose pinned receipts are at least as recent, and never
    /// dropped by a fenced stale writer.
    #[serde(default)]
    current_window_ids: Vec<String>,
    /// Independently fetched windows keyed by monitor scope. Windows are
    /// merged rather than selected by receipt recency so branch/checkout
    /// scopes cannot evict one another's live receipts.
    #[serde(default)]
    current_window_ids_by_scope: HashMap<String, Vec<String>>,
    #[serde(default)]
    current_window_generations: HashMap<String, u64>,
    #[serde(default)]
    window_generation: u64,
}

/// Durable receipt for one terminal run. Identities are aliases (run-id,
/// check-run id, fallback) so representation drift still suppresses.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct TerminalRunRecord {
    identities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    #[serde(default = "default_run_attempt")]
    run_attempt: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    workflow: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    conclusion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct PendingCiDelivery {
    identities: Vec<String>,
    kind: String,
    repo_name: String,
    pr_number: Option<u64>,
    workflow: String,
    status: String,
    conclusion: Option<String>,
    sha: String,
    url: String,
    branch: Option<String>,
    run_id: Option<String>,
    #[serde(default = "default_run_attempt")]
    run_attempt: u32,
    run_job_count: usize,
    run_all_terminal: bool,
    channel: Option<String>,
    mention: Option<String>,
    #[serde(default)]
    format: Option<crate::events::MessageFormat>,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    send_attempts: u32,
    #[serde(default)]
    last_sent_unix: Option<u64>,
    #[serde(default)]
    delivery_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct AckedDelivery {
    identities: Vec<String>,
    kind: String,
    conclusion: Option<String>,
    run_id: Option<String>,
    #[serde(default = "default_run_attempt")]
    run_attempt: u32,
    workflow: String,
    sha: String,
    pr_number: Option<u64>,
    #[serde(default)]
    repo_name: String,
    #[serde(default)]
    acked_unix: u64,
}

impl AckedDelivery {
    fn from_pending(pending: &PendingCiDelivery, now: u64) -> Self {
        Self {
            identities: pending.identities.clone(),
            kind: pending.kind.clone(),
            conclusion: pending.conclusion.clone(),
            run_id: pending.run_id.clone(),
            run_attempt: run_attempt_or_one(pending.run_attempt),
            workflow: pending.workflow.clone(),
            sha: pending.sha.clone(),
            pr_number: pending.pr_number,
            repo_name: pending.repo_name.clone(),
            acked_unix: now,
        }
    }

    fn matches_pending(&self, pending: &PendingCiDelivery) -> bool {
        if self.repo_name != pending.repo_name {
            return false;
        }
        if self.kind != pending.kind || self.conclusion != pending.conclusion {
            return false;
        }
        let self_checks = pending_check_identities(&self.identities);
        let pending_checks = pending_check_identities(&pending.identities);
        let self_has_run = self.run_id.is_some();
        let pending_has_run = pending.run_id.is_some();
        if !self_checks.is_empty() && !pending_checks.is_empty() {
            let check_match = self_checks
                .iter()
                .any(|identity| pending_checks.iter().any(|candidate| candidate == identity));
            if !check_match {
                return false;
            }
            if self_has_run && pending_has_run {
                return self.run_id == pending.run_id
                    && run_attempt_or_one(self.run_attempt)
                        == run_attempt_or_one(pending.run_attempt);
            }
            return run_attempt_or_one(self.run_attempt) == run_attempt_or_one(pending.run_attempt)
                && (!self_has_run || !pending_has_run || self.run_id == pending.run_id);
        }
        if self_has_run && pending_has_run {
            return self.run_id == pending.run_id
                && run_attempt_or_one(self.run_attempt) == run_attempt_or_one(pending.run_attempt)
                && self.workflow == pending.workflow;
        }
        let check_to_fallback =
            (!self_checks.is_empty() && pending_checks.is_empty() && !pending_has_run)
                || (!pending_checks.is_empty() && self_checks.is_empty() && !self_has_run);
        if !check_to_fallback
            && (self_has_run
                || pending_has_run
                || !self_checks.is_empty()
                || !pending_checks.is_empty())
        {
            return false;
        }
        self.sha == pending.sha
            && self.workflow == pending.workflow
            && self.pr_number == pending.pr_number
            && run_attempt_or_one(self.run_attempt) == run_attempt_or_one(pending.run_attempt)
    }

    fn merge_aliases(&mut self, pending: &PendingCiDelivery) {
        for identity in &pending.identities {
            if !self.identities.iter().any(|existing| existing == identity) {
                self.identities.push(identity.clone());
            }
        }
        if self.run_id.is_none() {
            self.run_id = pending.run_id.clone();
            if pending.run_id.is_some() {
                self.run_attempt = run_attempt_or_one(pending.run_attempt);
            }
        }
        if self.sha.is_empty() {
            self.sha = pending.sha.clone();
        }
    }
}

impl PendingCiDelivery {
    fn from_event(event: &IncomingEvent, identities: Vec<String>) -> Self {
        let payload = event.payload.as_object();
        let field = |name: &str| {
            payload
                .and_then(|object| object.get(name))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let mut identities = identities;
        if let Some(check_run_id) = payload
            .and_then(|object| object.get("check_run_id"))
            .and_then(serde_json::Value::as_str)
        {
            let identity = format!("check:{check_run_id}");
            if !identities.iter().any(|existing| existing == &identity) {
                identities.push(identity);
            }
        }
        Self {
            identities,
            kind: event.kind.clone(),
            repo_name: field("repo"),
            pr_number: payload
                .and_then(|object| object.get("number"))
                .and_then(serde_json::Value::as_u64),
            workflow: field("workflow"),
            status: field("status"),
            conclusion: payload
                .and_then(|object| object.get("conclusion"))
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
            sha: field("sha"),
            url: field("url"),
            branch: payload
                .and_then(|object| object.get("branch"))
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
            run_id: payload
                .and_then(|object| object.get("run_id"))
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
            run_attempt: payload
                .and_then(|object| object.get("run_attempt"))
                .and_then(serde_json::Value::as_u64)
                .map(|value| value as u32)
                .unwrap_or(1),
            run_job_count: payload
                .and_then(|object| object.get("run_job_count"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1) as usize,
            run_all_terminal: payload
                .and_then(|object| object.get("run_all_terminal"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            channel: event.channel.clone(),
            mention: event.mention.clone(),
            format: event.format.clone(),
            template: event.template.clone(),
            send_attempts: 0,
            last_sent_unix: None,
            delivery_id: String::new(),
        }
    }

    fn same_event(&self, other: &Self) -> bool {
        if self.kind != other.kind || self.conclusion != other.conclusion {
            return false;
        }
        let self_checks = pending_check_identities(&self.identities);
        let other_checks = pending_check_identities(&other.identities);
        if !self_checks.is_empty() && !other_checks.is_empty() {
            let check_match = self_checks
                .iter()
                .any(|identity| other_checks.iter().any(|candidate| candidate == identity));
            if !check_match {
                return false;
            }
            if self.run_id.is_some() && other.run_id.is_some() {
                return self.run_id == other.run_id
                    && run_attempt_or_one(self.run_attempt)
                        == run_attempt_or_one(other.run_attempt);
            }
            return run_attempt_or_one(self.run_attempt) == run_attempt_or_one(other.run_attempt)
                && (self.run_id.is_none()
                    || other.run_id.is_none()
                    || self.run_id == other.run_id);
        }
        let self_runs = pending_run_identities(&self.identities);
        let other_runs = pending_run_identities(&other.identities);
        if !self_checks.is_empty() {
            return other_runs.is_empty() && self.fallback_matches(other);
        }
        if !other_checks.is_empty() {
            return self_runs.is_empty() && self.fallback_matches(other);
        }
        if self.run_id.is_some() && other.run_id.is_some() {
            return self.run_id == other.run_id
                && run_attempt_or_one(self.run_attempt) == run_attempt_or_one(other.run_attempt)
                && self.workflow == other.workflow;
        }
        self.fallback_matches(other)
    }

    fn fallback_matches(&self, other: &Self) -> bool {
        self.sha == other.sha
            && self.workflow == other.workflow
            && self.pr_number == other.pr_number
            && run_attempt_or_one(self.run_attempt) == run_attempt_or_one(other.run_attempt)
    }

    fn same_output(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.conclusion == other.conclusion
            && self.identities == other.identities
            && self.repo_name == other.repo_name
            && self.workflow == other.workflow
            && self.sha == other.sha
            && self.pr_number == other.pr_number
            && self.run_id == other.run_id
            && run_attempt_or_one(self.run_attempt) == run_attempt_or_one(other.run_attempt)
            && self.channel == other.channel
            && self.mention == other.mention
            && self.format == other.format
            && self.template == other.template
            && self.url == other.url
            && self.status == other.status
            && self.branch == other.branch
            && self.run_job_count == other.run_job_count
            && self.run_all_terminal == other.run_all_terminal
    }

    fn into_event(self) -> IncomingEvent {
        let mut event = IncomingEvent::github_ci(
            &self.kind,
            self.repo_name,
            self.pr_number,
            self.workflow,
            self.status,
            self.conclusion,
            self.sha,
            self.url,
            self.branch,
            self.channel,
        )
        .with_mention(self.mention)
        .with_format(self.format)
        .with_template(self.template);
        if let Some(payload) = event.payload.as_object_mut() {
            if let Some(run_id) = &self.run_id {
                payload.insert("run_id".to_string(), json!(run_id));
                payload.insert(
                    "run_attempt".to_string(),
                    json!(run_attempt_or_one(self.run_attempt)),
                );
            }
            if let Some(check_id) = pending_check_identities(&self.identities)
                .first()
                .and_then(|identity| identity.strip_prefix("check:"))
            {
                payload.insert("check_run_id".to_string(), json!(check_id));
            }
            payload.insert("run_job_count".to_string(), json!(self.run_job_count));
            payload.insert("run_all_terminal".to_string(), json!(self.run_all_terminal));
        }
        event
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum PersistedTerminalRun {
    Legacy(String),
    Record(TerminalRunRecord),
}

impl From<PersistedTerminalRun> for TerminalRunRecord {
    fn from(value: PersistedTerminalRun) -> Self {
        match value {
            PersistedTerminalRun::Legacy(identity) => TerminalRunRecord::from_legacy(identity),
            PersistedTerminalRun::Record(record) => record,
        }
    }
}

fn deserialize_terminal_runs<'de, D>(
    deserializer: D,
) -> std::result::Result<VecDeque<TerminalRunRecord>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Vec::<PersistedTerminalRun>::deserialize(deserializer)?;
    Ok(raw.into_iter().map(TerminalRunRecord::from).collect())
}

fn deserialize_acked<'de, D>(
    deserializer: D,
) -> std::result::Result<VecDeque<AckedDelivery>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let Some(items) = value.as_array() else {
        return Ok(VecDeque::new());
    };
    Ok(items
        .iter()
        .filter_map(|item| serde_json::from_value::<AckedDelivery>(item.clone()).ok())
        .collect())
}

const MAX_EMITTED_COMMENT_IDS: usize = 1000;
const CI_PAGE_SIZE: usize = 100;
const MAX_CI_PAGES: usize = 5;
const MAX_BASELINE_RUNS_PER_REPO: usize = CI_PAGE_SIZE * MAX_CI_PAGES;
/// Hard bound on protected current-window identities per repo. The fetched
/// window is normally at most [`MAX_BASELINE_RUNS_PER_REPO`] runs, but each
/// run can contribute several durable aliases (attempt-qualified run ids,
/// check-run ids) and an oversized fetch can exceed the receipt cap;
/// protection stops here so alias multiplication cannot grow the pinned
/// window unboundedly.
// Pull-request inventory is fetched with the GitHub API's 100-item page cap
// and is not paginated here. The aggregate CI window is therefore bounded by
// 100 PR windows plus the direct push window, with two durable aliases per
// A complete scope can contain every fetched check run for every paginated
// pull request plus the direct push window. Bound each scope from that actual
// fetch shape; never apply this bound to the union of independent scopes.
const MAX_CURRENT_WINDOW_IDS_PER_SCOPE: usize =
    (CI_PAGE_SIZE * MAX_CI_PAGES) * (CI_PAGE_SIZE * MAX_CI_PAGES + 1) * 2;
const MAX_PENDING_SEND_ATTEMPTS: u32 = 3;
const PENDING_RETRY_BACKOFF_SECS: u64 = 3_600;
const ACK_RETENTION_SECS: u64 = 7 * 24 * 3_600;
const MAX_DEAD_LETTER: usize = 64;

/// Beside the cron state file, matching `tmux-watch-registry.json` and
/// `discord-watch-state.json`.
pub fn default_github_ci_baseline_path(cron_state_path: &Path) -> PathBuf {
    cron_state_path.with_file_name("github-ci-baseline.json")
}

fn canonicalize_github_repo(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let (owner, name) = trimmed.split_once('/')?;
    let owner = owner.trim();
    let name = name.trim();
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(format!(
        "{}/{}",
        owner.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    ))
}

fn ci_baseline_repo_key(snapshot: &GitSnapshot, repo: &GitRepoMonitor) -> String {
    snapshot
        .github_repo
        .as_deref()
        .or(repo.github_repo.as_deref())
        .and_then(canonicalize_github_repo)
        .map(|remote| format!("repo:{remote}"))
        .unwrap_or_else(|| format!("path:{}", repo.path))
}

fn pending_check_identities(identities: &[String]) -> Vec<&str> {
    identities
        .iter()
        .filter(|identity| identity.starts_with("check:"))
        .map(String::as_str)
        .collect()
}

fn pending_run_identities(identities: &[String]) -> Vec<&str> {
    identities
        .iter()
        .filter(|identity| identity.starts_with("run:"))
        .map(String::as_str)
        .collect()
}

fn run_attempt_or_one(attempt: u32) -> u32 {
    attempt.max(1)
}

fn run_identity(run_id: &str, attempt: u32) -> String {
    let attempt = run_attempt_or_one(attempt);
    if attempt == 1 {
        format!("run:{run_id}")
    } else {
        format!("run:{run_id}:{attempt}")
    }
}

impl TerminalRunRecord {
    fn from_snapshot(ci: &GitHubCISnapshot) -> Self {
        Self {
            identities: ci.identities(),
            run_id: ci.run_id.clone(),
            run_attempt: ci.run_attempt(),
            workflow: ci.workflow.clone(),
            conclusion: ci.conclusion.clone(),
            created_at: ci.created_at.clone(),
        }
    }

    fn from_legacy(identity: String) -> Self {
        let run_id = identity
            .strip_prefix("run:")
            .filter(|id| !id.is_empty() && !id.contains(':'))
            .map(ToString::to_string);
        Self {
            identities: vec![identity],
            run_id,
            run_attempt: 1,
            workflow: String::new(),
            conclusion: None,
            created_at: None,
        }
    }

    fn contains_identity(&self, identity: &str) -> bool {
        self.identities.iter().any(|stored| stored == identity)
    }

    fn unique_identities(&self) -> impl Iterator<Item = &str> {
        self.identities.iter().filter_map(|identity| {
            if identity.starts_with("run:") || identity.starts_with("check:") {
                Some(identity.as_str())
            } else {
                None
            }
        })
    }

    fn matches_snapshot(&self, ci: &GitHubCISnapshot) -> bool {
        let snap_ids = ci.unique_identities();
        let rec_checks = pending_check_identities(&self.identities);
        let snap_checks = pending_check_identities(&snap_ids);
        if !rec_checks.is_empty() && !snap_checks.is_empty() {
            let overlap = rec_checks
                .iter()
                .any(|identity| snap_checks.iter().any(|candidate| candidate == identity));
            return overlap
                && run_attempt_or_one(self.run_attempt) == ci.run_attempt()
                && self.conclusion == ci.conclusion;
        }
        if !snap_checks.is_empty() || !rec_checks.is_empty() {
            return false;
        }
        let unique = ci.unique_identities();
        if unique
            .iter()
            .any(|identity| self.contains_identity(identity))
        {
            return run_attempt_or_one(self.run_attempt) == ci.run_attempt()
                && self.conclusion == ci.conclusion;
        }
        false
    }

    fn merge_snapshot(&mut self, ci: &GitHubCISnapshot) -> bool {
        let mut dirty = false;
        for identity in ci.identities() {
            if !self.contains_identity(&identity) {
                self.identities.push(identity);
                dirty = true;
            }
        }
        if self.run_id.is_none() && ci.run_id.is_some() {
            self.run_id = ci.run_id.clone();
            dirty = true;
        }
        if self.created_at.is_none() && ci.created_at.is_some() {
            self.created_at = ci.created_at.clone();
            dirty = true;
        }
        dirty
    }

    fn eviction_key(&self) -> (u8, String, u64, String) {
        // Unknown-age legacy receipts sort first so they evict before
        // timestamped records. Tie-break by created_at, then run_id, then
        // identity so mixed load/commit is deterministic.
        let created_rank = u8::from(self.created_at.is_some());
        let created = self.created_at.clone().unwrap_or_default();
        let run_num = self
            .run_id
            .as_deref()
            .and_then(|id| id.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        let ident = self.identities.first().cloned().unwrap_or_default();
        (created_rank, created, run_num, ident)
    }
}

fn repo_ci_baseline_is_suppressed(
    repo_ci_baseline: Option<&RepoCIBaseline>,
    ci: &GitHubCISnapshot,
) -> bool {
    let Some(baseline) = repo_ci_baseline else {
        return false;
    };
    baseline
        .terminal_runs
        .iter()
        .any(|record| record.matches_snapshot(ci))
}

fn snapshot_eviction_key(ci: &GitHubCISnapshot) -> (u8, String, u64, String) {
    TerminalRunRecord::from_snapshot(ci).eviction_key()
}

/// Canonicalizes the pinned current-window identity set: only durable
/// aliases (`run:` / `check:` prefixes), each stored once, order-stable by
/// first appearance, hard-capped at [`MAX_CURRENT_WINDOW_IDS_PER_SCOPE`]. Identities
/// arrive oldest-first (eviction-key order); when the bound trims, the
/// OLDEST aliases drop so the newest fetched terminal runs — the ones still
/// inside GitHub's fetch window — always stay protected. Fallback
/// (`pr:sha:workflow`) identities are excluded: they are not used for
/// current-window authority and pinning them would alias-multiply the
/// protection set.
fn canonicalize_current_window_ids(incoming: &[&GitHubCISnapshot]) -> Vec<String> {
    cap_window_ids(canonical_window_identities(
        incoming.iter().flat_map(|ci| ci.unique_identities()),
    ))
}

/// Same canonical form for an already-pinned identity set (merge/load
/// paths): deduplicated, order-stable, hard-capped keeping the newest within
/// this one scope.
fn canonicalize_current_window_ids_from(ids: &[String]) -> Vec<String> {
    cap_window_ids(canonical_window_identities(ids.iter().cloned()))
}

/// Newest eviction key among the receipts pinned by a current window. Used
/// to pick the current-window authority during merge: the window whose
/// pinned receipts are the newest reflects the latest GitHub fetch state
/// and stays protected; a writer whose window pins only older receipts
/// (restart with a stale view, fenced writer) must not evict or replace it.
fn canonical_window_identities(identities: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    identities
        .filter(|identity| seen.insert(identity.clone()))
        .collect()
}

/// Applies the explicit window bound. Identities are oldest-first, so
/// trimming drops from the front and the newest survive.
fn cap_window_ids(mut ids: Vec<String>) -> Vec<String> {
    let excess = ids.len().saturating_sub(MAX_CURRENT_WINDOW_IDS_PER_SCOPE);
    ids.drain(..excess);
    ids
}

/// Post-load bound for a persisted `current_window_ids`. Writers already
/// store a canonicalized set; this only guards files written by older or
/// differently-shaped writers.
fn canonicalize_loaded_window(repo_baseline: &mut RepoCIBaseline) {
    if repo_baseline.current_window_ids_by_scope.is_empty()
        && !repo_baseline.current_window_ids.is_empty()
    {
        repo_baseline.current_window_ids_by_scope.insert(
            "legacy".to_string(),
            repo_baseline.current_window_ids.clone(),
        );
        repo_baseline
            .current_window_generations
            .insert("legacy".to_string(), repo_baseline.window_generation);
    }
    let mut scopes: Vec<(String, Vec<String>)> = repo_baseline
        .current_window_ids_by_scope
        .drain()
        .map(|(scope, ids)| (scope, canonicalize_current_window_ids_from(&ids)))
        .collect();
    scopes.sort_by(|left, right| left.0.cmp(&right.0));
    repo_baseline.current_window_ids_by_scope = scopes.into_iter().collect();
    let mut scope_names: Vec<&str> = repo_baseline
        .current_window_ids_by_scope
        .keys()
        .map(String::as_str)
        .collect();
    scope_names.sort_unstable();
    let ids = scope_names.into_iter().flat_map(|scope| {
        repo_baseline
            .current_window_ids_by_scope
            .get(scope)
            .into_iter()
            .flat_map(|ids| ids.iter().cloned())
    });
    // This union is an index for retention, not another bounded window. Each
    // scope was bounded independently above; truncating the union would drop
    // valid sibling-scope protection.
    repo_baseline.current_window_ids = canonical_window_identities(ids);
    repo_baseline.current_window_generations.retain(|scope, _| {
        repo_baseline
            .current_window_ids_by_scope
            .contains_key(scope)
    });
}

fn next_window_generation(previous: u64) -> u64 {
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(previous);
    let floor = previous.saturating_add(1).max(clock);
    let mut observed = WINDOW_GENERATION_COUNTER.load(Ordering::Relaxed);
    loop {
        let next = observed
            .max(floor)
            .saturating_add(u64::from(observed >= floor));
        match WINDOW_GENERATION_COUNTER.compare_exchange_weak(
            observed,
            next,
            Ordering::SeqCst,
            Ordering::Relaxed,
        ) {
            Ok(_) => return next,
            Err(current) => observed = current,
        }
    }
}

fn cap_repo_baseline_keeping_ids(
    repo_baseline: &mut RepoCIBaseline,
    keep_ids: &std::collections::HashSet<String>,
) -> bool {
    if repo_baseline.terminal_runs.len() <= MAX_BASELINE_RUNS_PER_REPO {
        return false;
    }
    let mut records: Vec<TerminalRunRecord> = repo_baseline.terminal_runs.drain(..).collect();
    records.sort_by_key(|record| record.eviction_key());
    let excess = records.len().saturating_sub(MAX_BASELINE_RUNS_PER_REPO);
    let mut kept = VecDeque::new();
    let mut dropped = 0_usize;
    for record in records {
        let protected = record
            .unique_identities()
            .any(|identity| keep_ids.contains(identity));
        if !protected && dropped < excess {
            dropped += 1;
            continue;
        }
        kept.push_back(record);
    }
    repo_baseline.terminal_runs = kept;
    dropped > 0
}

#[cfg(test)]
impl RepoCIBaseline {
    fn contains_identity(&self, identity: &str) -> bool {
        self.terminal_runs
            .iter()
            .any(|record| record.contains_identity(identity))
    }
}

impl CIBaseline {
    fn reserve_window_generation(
        &mut self,
        repo_key: &str,
        repo_path: &str,
        window_scope: &str,
    ) -> u64 {
        let repo = self.repo_baseline_mut(repo_key, repo_path);
        let generation = next_window_generation(repo.window_generation);
        repo.window_generation = generation;
        repo.current_window_generations
            .entry(window_scope.to_string())
            .or_insert(0);
        generation
    }

    fn repo_baseline_mut(&mut self, repo_key: &str, repo_path: &str) -> &mut RepoCIBaseline {
        let mut inherited = Vec::new();
        if let Some(legacy) = self.repos.remove(repo_path) {
            inherited.push(legacy);
        }
        let path_key = format!("path:{repo_path}");
        if path_key != repo_key
            && let Some(legacy) = self.repos.remove(&path_key)
        {
            inherited.push(legacy);
        }
        if let Some(canonical) = repo_key.strip_prefix("repo:") {
            let stale: Vec<String> = self
                .repos
                .keys()
                .filter(|key| {
                    *key != repo_key
                        && key
                            .strip_prefix("repo:")
                            .and_then(canonicalize_github_repo)
                            .as_deref()
                            == Some(canonical)
                })
                .cloned()
                .collect();
            for key in stale {
                if let Some(legacy) = self.repos.remove(&key) {
                    inherited.push(legacy);
                }
            }
        }
        let dest = self.repos.entry(repo_key.to_string()).or_default();
        for legacy in inherited {
            merge_repo_baseline(dest, legacy);
        }
        dest
    }

    fn enqueue_pending(
        &mut self,
        repo_key: &str,
        repo_path: &str,
        events: &[IncomingEvent],
        current: &HashMap<String, GitHubCISnapshot>,
    ) {
        if events.is_empty() {
            return;
        }
        let deliveries: Vec<PendingCiDelivery> = events
            .iter()
            .map(|event| {
                let identities = previous_snapshot_for_event(current, event)
                    .map(GitHubCISnapshot::identities)
                    .unwrap_or_default();
                PendingCiDelivery::from_event(event, identities)
            })
            .collect();
        let repo_baseline = self.repo_baseline_mut(repo_key, repo_path);
        for delivery in deliveries {
            if pending_is_acked(repo_baseline, &delivery) {
                continue;
            }
            if !repo_baseline
                .pending
                .iter()
                .any(|existing| existing.same_event(&delivery))
            {
                let mut delivery = delivery;
                if delivery.delivery_id.is_empty() {
                    delivery.delivery_id = uuid::Uuid::new_v4().to_string();
                }
                repo_baseline.pending.push(delivery);
            }
        }
    }

    /// Records terminal-run identities for one canonical GitHub repo. Returns
    /// whether the persisted state changed (bounded, oldest GitHub runs
    /// evicted first).
    #[cfg(test)]
    fn record_terminal_runs(
        &mut self,
        repo_key: &str,
        repo_path: &str,
        current: &HashMap<String, GitHubCISnapshot>,
    ) -> bool {
        self.record_terminal_runs_filtered(
            repo_key,
            repo_path,
            repo_path,
            current,
            (0, true),
            |_| true,
        )
    }

    fn record_terminal_runs_filtered(
        &mut self,
        repo_key: &str,
        repo_path: &str,
        window_scope: &str,
        current: &HashMap<String, GitHubCISnapshot>,
        window: (u64, bool),
        include: impl Fn(&GitHubCISnapshot) -> bool,
    ) -> bool {
        let mut incoming: Vec<&GitHubCISnapshot> = current
            .values()
            .filter(|ci| ci.is_terminal() && include(ci))
            .collect();
        incoming.sort_by_key(|ci| snapshot_eviction_key(ci));

        let repo_baseline = self.repo_baseline_mut(repo_key, repo_path);
        let mut dirty = false;
        for ci in &incoming {
            if let Some(existing) = repo_baseline
                .terminal_runs
                .iter_mut()
                .find(|record| record.matches_snapshot(ci))
            {
                dirty |= existing.merge_snapshot(ci);
            } else {
                repo_baseline
                    .terminal_runs
                    .push_back(TerminalRunRecord::from_snapshot(ci));
                dirty = true;
            }
        }
        if window.1 {
            let generation = if window.0 == 0 {
                next_window_generation(repo_baseline.window_generation)
            } else {
                window.0
            };
            let existing_generation = repo_baseline
                .current_window_generations
                .get(window_scope)
                .copied()
                .unwrap_or(0);
            if generation >= existing_generation {
                repo_baseline.window_generation = repo_baseline.window_generation.max(generation);
                repo_baseline.current_window_ids_by_scope.insert(
                    window_scope.to_string(),
                    canonicalize_current_window_ids(&incoming),
                );
                repo_baseline
                    .current_window_generations
                    .insert(window_scope.to_string(), generation);
            }
        }
        canonicalize_loaded_window(repo_baseline);
        let mut keep_ids: std::collections::HashSet<String> =
            repo_baseline.current_window_ids.iter().cloned().collect();
        if !window.1 {
            keep_ids.extend(incoming.iter().flat_map(|ci| ci.unique_identities()));
        }
        dirty |= cap_repo_baseline_keeping_ids(repo_baseline, &keep_ids);
        dirty
    }
}

fn terminal_records_same_run(left: &TerminalRunRecord, right: &TerminalRunRecord) -> bool {
    let left_checks = pending_check_identities(&left.identities);
    let right_checks = pending_check_identities(&right.identities);
    if !left_checks.is_empty() && !right_checks.is_empty() {
        return left_checks
            .iter()
            .any(|identity| right_checks.iter().any(|candidate| candidate == identity))
            && run_attempt_or_one(left.run_attempt) == run_attempt_or_one(right.run_attempt)
            && left.conclusion == right.conclusion;
    }
    let left_unique: Vec<&str> = left.unique_identities().collect();
    let right_unique: Vec<&str> = right.unique_identities().collect();
    if !left_unique.is_empty() && !right_unique.is_empty() {
        return left_unique
            .iter()
            .any(|identity| right_unique.contains(identity))
            && run_attempt_or_one(left.run_attempt) == run_attempt_or_one(right.run_attempt)
            && left.conclusion == right.conclusion;
    }
    left.identities
        .iter()
        .any(|identity| right.contains_identity(identity))
}

fn pending_is_acked(repo: &RepoCIBaseline, delivery: &PendingCiDelivery) -> bool {
    repo.acked
        .iter()
        .any(|acked| acked.matches_pending(delivery))
}

fn record_acked(repo: &mut RepoCIBaseline, delivery: &PendingCiDelivery) {
    if let Some(existing) = repo
        .acked
        .iter_mut()
        .find(|acked| acked.matches_pending(delivery))
    {
        existing.merge_aliases(delivery);
        return;
    }
    repo.acked
        .push_back(AckedDelivery::from_pending(delivery, unix_now()));
}

#[cfg(test)]
fn retire_exhausted_pending(repo: &mut RepoCIBaseline) {
    let mut still_pending = Vec::new();
    for pending in repo.pending.drain(..) {
        if pending.send_attempts >= MAX_PENDING_SEND_ATTEMPTS {
            repo.dead_letter.push_back(pending);
        } else {
            still_pending.push(pending);
        }
    }
    repo.pending = still_pending;
    while repo.dead_letter.len() > MAX_DEAD_LETTER {
        repo.dead_letter.pop_front();
    }
}

fn dead_letter_unsent_final_attempts(ci_baseline: &mut CIBaseline, unsent: &[PendingCiDelivery]) {
    for repo in ci_baseline.repos.values_mut() {
        let mut keep = Vec::new();
        let mut retired = Vec::new();
        for pending in repo.pending.drain(..) {
            if unsent
                .iter()
                .any(|item| pending.same_event(item) && pending.repo_name == item.repo_name)
                && pending.send_attempts >= MAX_PENDING_SEND_ATTEMPTS
            {
                eprintln!(
                    "clawhip source github CI outbox dead-lettered {} {} after {} send attempts",
                    pending.repo_name, pending.workflow, pending.send_attempts
                );
                telemetry::emit(source_record(
                    telemetry::event_name::SOURCE_DEGRADED,
                    "ci_outbox_dead_lettered",
                    Some(&pending.repo_name),
                    Some(format!(
                        "workflow={} attempts={}",
                        pending.workflow, pending.send_attempts
                    )),
                ));
                retired.push(pending);
            } else {
                keep.push(pending);
            }
        }
        repo.pending = keep;
        for pending in retired {
            record_acked(repo, &pending);
            repo.dead_letter.push_back(pending);
        }
        while repo.dead_letter.len() > MAX_DEAD_LETTER {
            repo.dead_letter.pop_front();
        }
    }
}

fn compact_acked(repo: &mut RepoCIBaseline, now: u64) {
    let before = repo.acked.len();
    repo.acked
        .retain(|acked| now.saturating_sub(acked.acked_unix) < ACK_RETENTION_SECS);
    if repo.acked.len() < before {
        repo.min_writer_epoch = repo.min_writer_epoch.max(repo.epoch);
    }
}

fn normalize_repo_outbox(repo: &mut RepoCIBaseline) {
    while repo.dead_letter.len() > MAX_DEAD_LETTER {
        repo.dead_letter.pop_front();
    }
    let acked = repo.acked.clone();
    let dead = repo.dead_letter.clone();
    repo.pending.retain(|pending| {
        !acked.iter().any(|item| item.matches_pending(pending))
            && !dead
                .iter()
                .any(|item| item.same_event(pending) && item.repo_name == pending.repo_name)
    });
}

fn merge_repo_baseline(dest: &mut RepoCIBaseline, source: RepoCIBaseline) {
    // A writer fenced by `min_writer_epoch` predates the current authority:
    // its window can cap-evict nothing from the pinned current window and
    // must not replace it either.
    let source_stale = dest.min_writer_epoch > 0 && source.epoch <= dest.min_writer_epoch;
    canonicalize_loaded_window(dest);
    let source_generations = source.current_window_generations;
    let source_window_generation = source.window_generation;
    let mut source_windows = source.current_window_ids_by_scope;
    if source_windows.is_empty() && !source.current_window_ids.is_empty() {
        source_windows.insert("legacy".to_string(), source.current_window_ids);
    }
    for (scope, ids) in source_windows {
        let source_generation = source_generations
            .get(&scope)
            .copied()
            .unwrap_or(source_window_generation);
        let dest_generation = dest
            .current_window_generations
            .get(&scope)
            .copied()
            .unwrap_or(dest.window_generation);
        if source_stale || source_generation <= dest_generation {
            dest.current_window_ids_by_scope.entry(scope).or_insert(ids);
        } else {
            dest.current_window_ids_by_scope.insert(scope.clone(), ids);
            dest.current_window_generations
                .insert(scope, source_generation);
        }
    }
    dest.window_generation = dest.window_generation.max(source_window_generation);
    canonicalize_loaded_window(dest);
    let keep_ids: std::collections::HashSet<String> =
        dest.current_window_ids.iter().cloned().collect();
    for record in source.terminal_runs {
        if let Some(existing) = dest
            .terminal_runs
            .iter_mut()
            .find(|candidate| terminal_records_same_run(candidate, &record))
        {
            for identity in record.identities {
                if !existing.contains_identity(&identity) {
                    existing.identities.push(identity);
                }
            }
            if existing.run_id.is_none() {
                existing.run_id = record.run_id;
            }
            if existing.created_at.is_none() {
                existing.created_at = record.created_at;
            }
        } else {
            dest.terminal_runs.push_back(record);
        }
    }
    cap_repo_baseline_keeping_ids(dest, &keep_ids);
    dest.epoch = dest.epoch.max(source.epoch);
    dest.min_writer_epoch = dest.min_writer_epoch.max(source.min_writer_epoch);
    dest.ci_established = dest.ci_established || source.ci_established;
    for acked in source.acked {
        if let Some(existing) = dest.acked.iter_mut().find(|candidate| {
            candidate.repo_name == acked.repo_name
                && candidate.kind == acked.kind
                && candidate.conclusion == acked.conclusion
                && run_attempt_or_one(candidate.run_attempt)
                    == run_attempt_or_one(acked.run_attempt)
                && (pending_check_identities(&candidate.identities)
                    .iter()
                    .any(|identity| pending_check_identities(&acked.identities).contains(identity))
                    || (candidate.run_id.is_some()
                        && candidate.run_id == acked.run_id
                        && candidate.workflow == acked.workflow))
        }) {
            for identity in acked.identities {
                if !existing.identities.iter().any(|item| item == &identity) {
                    existing.identities.push(identity);
                }
            }
        } else {
            dest.acked.push_back(acked);
        }
    }
    for dead in source.dead_letter {
        if !dest
            .dead_letter
            .iter()
            .any(|existing| existing.same_event(&dead))
        {
            dest.dead_letter.push_back(dead);
        }
    }
    while dest.dead_letter.len() > MAX_DEAD_LETTER {
        dest.dead_letter.pop_front();
    }
    for delivery in source.pending {
        if source_stale
            || pending_is_acked(dest, &delivery)
            || dest
                .dead_letter
                .iter()
                .any(|dead| dead.same_event(&delivery) && dead.repo_name == delivery.repo_name)
        {
            continue;
        }
        if let Some(existing) = dest
            .pending
            .iter_mut()
            .find(|existing| existing.same_event(&delivery))
        {
            merge_pending_retry_state(existing, &delivery);
        } else {
            dest.pending.push(delivery);
        }
    }
    let acked = dest.acked.clone();
    let dead = dest.dead_letter.clone();
    dest.pending.retain(|pending| {
        !acked.iter().any(|item| item.matches_pending(pending))
            && !dead
                .iter()
                .any(|item| item.same_event(pending) && item.repo_name == pending.repo_name)
    });
    while dest.dead_letter.len() > MAX_DEAD_LETTER {
        dest.dead_letter.pop_front();
    }
}

fn load_ci_baseline(path: Option<&Path>) -> CIBaseline {
    let Some(path) = path else {
        return CIBaseline::default();
    };
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return CIBaseline::default(),
        Err(error) => {
            eprintln!(
                "clawhip source github CI baseline '{}' unreadable; starting a fresh baseline: {error}",
                path.display()
            );
            return CIBaseline::default();
        }
    };
    match serde_json::from_str::<HashMap<String, RepoCIBaseline>>(&raw) {
        Ok(repos) => {
            let mut baseline = CIBaseline { repos };
            for repo_baseline in baseline.repos.values_mut() {
                // Reloaded baselines may predate canonicalization or come
                // from a differently-shaped writer: re-canonicalize the
                // pinned window so a stale file cannot un-bound it.
                canonicalize_loaded_window(repo_baseline);
                normalize_repo_outbox(repo_baseline);
            }
            baseline
        }
        Err(error) => {
            eprintln!(
                "clawhip source github CI baseline '{}' invalid; starting a fresh baseline: {error}",
                path.display()
            );
            CIBaseline::default()
        }
    }
}

fn merge_ci_baseline(dest: &mut CIBaseline, source: &CIBaseline) {
    for (key, repo) in &source.repos {
        let dest_repo = dest.repos.entry(key.clone()).or_default();
        merge_repo_baseline(dest_repo, repo.clone());
    }
}

#[derive(Debug)]
struct BaselineLock {
    file: File,
}

impl Drop for BaselineLock {
    fn drop(&mut self) {
        unlock_baseline_file(&self.file);
    }
}

/// Kernel advisory lock is bound to the opened file descriptor. Replacing
/// the lock path (symlink/rename) after the FD is held cannot steal the
/// lock. Leftover directory locks fail closed and are not deleted.
fn acquire_baseline_lock(path: &Path) -> Result<BaselineLock> {
    let lock_path = path.with_extension("json.lock");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if lock_path.is_dir() {
        return Err(
            "GitHub CI baseline lock path is a leftover directory lock; stop older clawhip processes and remove it before retrying"
                .into(),
        );
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    let deadline = Instant::now() + BASELINE_LOCK_MAX_WAIT;
    loop {
        if try_lock_baseline_file(&file) {
            return Ok(BaselineLock { file });
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(remaining.min(BASELINE_LOCK_RETRY_DELAY));
    }
    Err("timed out waiting for GitHub CI baseline lock".into())
}

#[cfg(unix)]
fn try_lock_baseline_file(file: &File) -> bool {
    use std::os::unix::io::AsRawFd;
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

#[cfg(unix)]
fn unlock_baseline_file(file: &File) {
    use std::os::unix::io::AsRawFd;
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}

#[cfg(windows)]
#[repr(C)]
struct WindowsOverlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut core::ffi::c_void,
}

#[cfg(windows)]
fn try_lock_baseline_file(file: &File) -> bool {
    use std::os::windows::io::AsRawHandle;
    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x1;
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x2;
    let mut overlapped = WindowsOverlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: std::ptr::null_mut(),
    };
    unsafe {
        let ok = lock_file_ex(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        );
        if ok != 0 {
            true
        } else {
            let _ = get_last_error();
            false
        }
    }
}

#[cfg(windows)]
fn unlock_baseline_file(file: &File) {
    use std::os::windows::io::AsRawHandle;
    let mut overlapped = WindowsOverlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event: std::ptr::null_mut(),
    };
    unsafe {
        let _ = unlock_file_ex(file.as_raw_handle(), 0, 1, 0, &mut overlapped);
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "LockFileEx"]
    fn lock_file_ex(
        handle: *mut core::ffi::c_void,
        flags: u32,
        reserved: u32,
        length_low: u32,
        length_high: u32,
        overlapped: *mut WindowsOverlapped,
    ) -> i32;
    #[link_name = "UnlockFileEx"]
    fn unlock_file_ex(
        handle: *mut core::ffi::c_void,
        reserved: u32,
        length_low: u32,
        length_high: u32,
        overlapped: *mut WindowsOverlapped,
    ) -> i32;
    #[link_name = "GetLastError"]
    fn get_last_error() -> u32;
    #[link_name = "MoveFileExW"]
    fn move_file_ex_w(existing_file_name: *const u16, new_file_name: *const u16, flags: u32)
    -> i32;
}

#[cfg(windows)]
fn replace_file_windows(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    let from_w: Vec<u16> = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let to_w: Vec<u16> = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        if move_file_ex_w(
            from_w.as_ptr(),
            to_w.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        ) == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn pending_due_for_send(pending: &PendingCiDelivery, now: u64) -> bool {
    if pending.send_attempts >= MAX_PENDING_SEND_ATTEMPTS {
        return false;
    }
    match pending.last_sent_unix {
        None => true,
        Some(timestamp) => now.saturating_sub(timestamp) >= PENDING_RETRY_BACKOFF_SECS,
    }
}

fn merge_pending_retry_state(dest: &mut PendingCiDelivery, source: &PendingCiDelivery) {
    dest.send_attempts = dest.send_attempts.max(source.send_attempts);
    dest.last_sent_unix = match (dest.last_sent_unix, source.last_sent_unix) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    };
}

fn sync_file(file: &File) -> Result<()> {
    #[cfg(test)]
    SYNC_FILE_CALLS.with(|count| count.set(count.get() + 1));
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_directory(_dir: &Path) -> Result<()> {
    // NTFS treats replace-on-rename as durable once the destination file
    // contents have been flushed. Directory metadata is not separately
    // fsync'd here.
    Ok(())
}

fn write_ci_baseline_atomic(baseline: &CIBaseline, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_string(&baseline.repos)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let temp_name = format!(
        "{}.{}.{nonce}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("github-ci-baseline.json"),
        std::process::id()
    );
    let temp_path = match path.parent() {
        Some(parent) => parent.join(temp_name),
        None => PathBuf::from(temp_name),
    };
    let write_result = (|| -> Result<()> {
        let mut file = File::create(&temp_path)?;
        file.write_all(payload.as_bytes())?;
        sync_file(&file)?;
        drop(file);
        #[cfg(windows)]
        replace_file_windows(&temp_path, path)?;
        #[cfg(not(windows))]
        fs::rename(&temp_path, path)?;
        let dest = OpenOptions::new().read(true).write(true).open(path)?;
        sync_file(&dest)?;
        drop(dest);
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

fn commit_ci_baseline(path: Option<&Path>, local: &CIBaseline) -> Result<CIBaseline> {
    let Some(path) = path else {
        return Ok(local.clone());
    };
    let _lock = acquire_baseline_lock(path)?;
    let mut disk = load_ci_baseline(Some(path));
    merge_ci_baseline(&mut disk, local);
    let now = unix_now();
    for repo in disk.repos.values_mut() {
        repo.epoch = repo.epoch.saturating_add(1);
        compact_acked(repo, now);
        normalize_repo_outbox(repo);
    }
    write_ci_baseline_atomic(&disk, path)?;
    Ok(disk)
}

#[cfg(test)]
fn save_ci_baseline(baseline: &CIBaseline, path: Option<&Path>) -> Result<()> {
    commit_ci_baseline(path, baseline).map(|_| ())
}

async fn send_then_ack_prefix(
    tx: &mpsc::Sender<IncomingEvent>,
    ci_baseline: &mut CIBaseline,
    path: Option<&Path>,
    _events: Vec<IncomingEvent>,
    delivered: &[PendingCiDelivery],
) -> Result<()> {
    let now = unix_now();
    for delivery in delivered {
        let Some(queued) = outbox_entry(ci_baseline, delivery) else {
            continue;
        };
        if !pending_due_for_send(&queued, now) {
            continue;
        }
        if persist_pending_send_attempts(ci_baseline, path, std::slice::from_ref(&queued), now)
            .is_err()
        {
            eprintln!("clawhip source github CI baseline outbox attempt persist failed");
            return Ok(());
        }
        let Some(durable) = outbox_entry(ci_baseline, &queued) else {
            continue;
        };
        match send_event(tx, durable.clone().into_event()).await {
            Ok(()) => {
                if let Err(error) =
                    ack_pending_deliveries(ci_baseline, path, std::slice::from_ref(&durable))
                {
                    eprintln!("clawhip source github CI baseline outbox ack failed: {error}");
                }
            }
            Err(_) => {
                dead_letter_unsent_final_attempts(ci_baseline, std::slice::from_ref(&durable));
                let _ = commit_ci_baseline(path, ci_baseline);
                return Ok(());
            }
        }
    }
    Ok(())
}

async fn drain_pending_outbox(
    ci_baseline: &mut CIBaseline,
    path: Option<&Path>,
    tx: &mpsc::Sender<IncomingEvent>,
) -> Result<()> {
    let now = unix_now();
    let due: Vec<PendingCiDelivery> = ci_baseline
        .repos
        .values()
        .flat_map(|repo| repo.pending.iter().cloned())
        .filter(|pending| pending_due_for_send(pending, now))
        .collect();
    for delivery in due {
        if persist_pending_send_attempts(ci_baseline, path, std::slice::from_ref(&delivery), now)
            .is_err()
        {
            eprintln!("clawhip source github CI baseline outbox attempt persist failed");
            return Ok(());
        }
        let Some(durable) = outbox_entry(ci_baseline, &delivery) else {
            continue;
        };
        if send_event(tx, durable.clone().into_event()).await.is_err() {
            dead_letter_unsent_final_attempts(ci_baseline, std::slice::from_ref(&durable));
            let _ = commit_ci_baseline(path, ci_baseline);
            return Ok(());
        }
        if let Err(error) =
            ack_pending_deliveries(ci_baseline, path, std::slice::from_ref(&durable))
        {
            eprintln!("clawhip source github CI baseline outbox ack failed: {error}");
        }
    }
    Ok(())
}

fn outbox_entry(
    ci_baseline: &CIBaseline,
    delivery: &PendingCiDelivery,
) -> Option<PendingCiDelivery> {
    for repo in ci_baseline.repos.values() {
        if let Some(pending) = repo.pending.iter().find(|pending| {
            if !delivery.delivery_id.is_empty() {
                pending.delivery_id == delivery.delivery_id
            } else {
                pending.same_output(delivery)
            }
        }) {
            return Some(pending.clone());
        }
    }
    None
}

fn persist_pending_send_attempts(
    ci_baseline: &mut CIBaseline,
    path: Option<&Path>,
    items: &[PendingCiDelivery],
    now: u64,
) -> Result<()> {
    let mut next = ci_baseline.clone();
    bump_pending_send_attempts(&mut next, items, now);
    *ci_baseline = commit_ci_baseline(path, &next)?;
    Ok(())
}

fn bump_pending_send_attempts(ci_baseline: &mut CIBaseline, sent: &[PendingCiDelivery], now: u64) {
    for repo in ci_baseline.repos.values_mut() {
        for pending in &mut repo.pending {
            if sent.iter().any(|item| {
                if !item.delivery_id.is_empty() {
                    pending.delivery_id == item.delivery_id
                } else {
                    pending.same_output(item)
                }
            }) {
                pending.send_attempts = pending.send_attempts.saturating_add(1);
                pending.last_sent_unix = Some(now);
            }
        }
    }
}

fn ack_pending_deliveries(
    ci_baseline: &mut CIBaseline,
    path: Option<&Path>,
    delivered: &[PendingCiDelivery],
) -> Result<()> {
    let Some(path) = path else {
        for repo in ci_baseline.repos.values_mut() {
            for delivery in delivered {
                if repo.pending.iter().any(|pending| {
                    pending.repo_name == delivery.repo_name
                        && (pending.same_event(delivery)
                            || AckedDelivery::from_pending(delivery, 0).matches_pending(pending))
                }) {
                    record_acked(repo, delivery);
                }
            }
            let acked = repo.acked.clone();
            repo.pending
                .retain(|pending| !acked.iter().any(|item| item.matches_pending(pending)));
        }
        return Ok(());
    };
    let _lock = acquire_baseline_lock(path)?;
    let mut disk = load_ci_baseline(Some(path));
    merge_ci_baseline(&mut disk, ci_baseline);
    for (key, repo) in disk.repos.iter_mut() {
        let local_repo = ci_baseline.repos.get(key);
        for delivery in delivered {
            let in_disk = repo.pending.iter().any(|pending| {
                pending.repo_name == delivery.repo_name
                    && (pending.same_event(delivery)
                        || AckedDelivery::from_pending(delivery, 0).matches_pending(pending))
            });
            let in_local = local_repo
                .map(|local| {
                    local.pending.iter().any(|pending| {
                        pending.repo_name == delivery.repo_name
                            && (pending.same_event(delivery)
                                || AckedDelivery::from_pending(delivery, 0)
                                    .matches_pending(pending))
                    })
                })
                .unwrap_or(false);
            if in_disk || in_local {
                record_acked(repo, delivery);
            }
        }
        let acked = repo.acked.clone();
        repo.pending
            .retain(|pending| !acked.iter().any(|item| item.matches_pending(pending)));
    }
    write_ci_baseline_atomic(&disk, path)?;
    *ci_baseline = disk;
    Ok(())
}

fn merge_incomplete_ci(
    previous: &HashMap<String, GitHubCISnapshot>,
    mut current: HashMap<String, GitHubCISnapshot>,
) -> HashMap<String, GitHubCISnapshot> {
    for old in previous.values() {
        if current.contains_key(&old.dedupe_key()) {
            continue;
        }
        if current.values().any(|ci| snapshots_same_run(ci, old)) {
            continue;
        }
        current.insert(old.dedupe_key(), old.clone());
    }
    current
}

fn snapshots_same_run(left: &GitHubCISnapshot, right: &GitHubCISnapshot) -> bool {
    let left_ids = left.unique_identities();
    let right_ids = right.unique_identities();
    let left_checks = pending_check_identities(&left_ids);
    let right_checks = pending_check_identities(&right_ids);
    if !left_checks.is_empty() && !right_checks.is_empty() {
        return left_checks
            .iter()
            .any(|identity| right_checks.iter().any(|candidate| candidate == identity))
            && left.run_attempt() == right.run_attempt();
    }
    if !left_checks.is_empty() || !right_checks.is_empty() {
        return false;
    }
    if !left_ids.is_empty()
        && left_ids
            .iter()
            .any(|identity| right_ids.iter().any(|candidate| candidate == identity))
    {
        return left.run_attempt() == right.run_attempt();
    }
    left.dedupe_key() == right.dedupe_key()
}

struct GitHubRepoState {
    activity: PRActivityState,
    issues: HashMap<u64, IssueSnapshot>,
    prs: HashMap<u64, PullRequestSnapshot>,
    ci: HashMap<String, GitHubCISnapshot>,
    ci_baseline_established: bool,
}

#[derive(Clone)]
struct IssueSnapshot {
    title: String,
    state: String,
    comments: u64,
}

#[derive(Clone)]
struct PullRequestSnapshot {
    updated_at: String,
    author: String,
    title: String,
    status: String,
    url: String,
    head_branch: String,
    head_sha: String,
}

#[derive(Clone, Default)]
struct PRActivityState {
    pr_comment_cursor: Option<String>,
    pr_comment_lower: Option<String>,
    review_comment_cursor: Option<String>,
    // Creation eligibility stays fixed while the updated-at cursor drains a capped feed.
    review_comment_lower: Option<String>,
    emitted_review_comment_ids: VecDeque<u64>,
    emitted_pr_comment_ids: VecDeque<u64>,
    review_watermark: Option<String>,
    review_scan: Option<ReviewScan>,
    emitted_review_ids: VecDeque<u64>,
    metadata: HashMap<u64, Option<PRActivityMetadata>>,
    metadata_order: VecDeque<u64>,
    pending_comments: VecDeque<PendingPRComment>,
}

#[derive(Clone)]
struct PendingPRComment {
    id: u64,
    number: u64,
    author: String,
    url: String,
    // Some(path) identifies an inline comment; conversation comments have no path.
    path: Option<String>,
    created_at: String,
    attempts: usize,
    is_issue_comment: bool,
}

#[derive(Clone)]
struct PRActivityMetadata {
    title: String,
    author: String,
}

#[derive(Clone)]
struct ReviewScan {
    lower: String,
    upper: String,
    pending: VecDeque<u64>,
    // Retain payload metadata while the per-poll budget drains the scan.
    prs: HashMap<u64, PullRequestSnapshot>,
}

const MAX_REVIEW_FETCH_PRS: usize = 20;
const MAX_COMMENT_PAGES: usize = 10;
const MAX_PR_METADATA_LOOKUPS: usize = 20;
const MAX_PR_METADATA_CACHE: usize = 256;
const MAX_PENDING_PR_COMMENTS: usize = 500;
const MAX_METADATA_ATTEMPTS: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
struct GitHubCISnapshot {
    pr_number: Option<u64>,
    workflow: String,
    status: String,
    conclusion: Option<String>,
    sha: String,
    url: String,
    branch: Option<String>,
    run_id: Option<String>,
    run_attempt: u32,
    check_run_id: Option<String>,
    created_at: Option<String>,
    run_job_count: usize,
    run_all_terminal: bool,
}

impl GitHubCISnapshot {
    fn fallback_identity(&self) -> String {
        format!(
            "{}:{}:{}",
            self.pr_number
                .map(|number| number.to_string())
                .unwrap_or_else(|| "none".to_string()),
            self.sha,
            self.workflow
        )
    }

    fn run_attempt(&self) -> u32 {
        run_attempt_or_one(self.run_attempt)
    }

    fn unique_identities(&self) -> Vec<String> {
        let mut identities = Vec::new();
        if let Some(run_id) = &self.run_id {
            identities.push(run_identity(run_id, self.run_attempt()));
        }
        if let Some(check_run_id) = &self.check_run_id {
            identities.push(format!("check:{check_run_id}"));
        }
        identities
    }

    fn identities(&self) -> Vec<String> {
        let mut identities = self.unique_identities();
        identities.push(self.fallback_identity());
        identities
    }

    fn dedupe_key(&self) -> String {
        if let Some(check_run_id) = &self.check_run_id {
            return format!("check:{check_run_id}");
        }
        if let Some(run_id) = &self.run_id {
            return format!("run:{run_id}:{}:{}", self.run_attempt(), self.workflow);
        }
        self.fallback_identity()
    }

    fn event_kind(&self) -> &'static str {
        classify_ci_event_kind(&self.status, self.conclusion.as_deref())
    }

    /// A workflow is terminal only when every job is done. A single completed
    /// job in a still-running workflow must not be persisted, or restart would
    /// suppress the eventual completion (#317).
    fn is_terminal(&self) -> bool {
        self.status == "completed" && self.run_all_terminal
    }
}

async fn run_github_poll_cycle(
    config: &AppConfig,
    github_client: Option<&GitHubClient>,
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut HashMap<String, GitHubRepoState>,
    ci_baseline: &mut CIBaseline,
    ci_baseline_path: Option<&Path>,
) {
    if let Err(error) = poll_github(
        config,
        github_client,
        tx,
        state,
        ci_baseline,
        ci_baseline_path,
    )
    .await
        && emit_source_degraded(None, &error.to_string())
    {
        eprintln!("clawhip source github poll failed: {error}");
    }
}

async fn snapshot_github_repo(repo: &GitRepoMonitor) -> Result<GitSnapshot> {
    match snapshot_git_repo(repo).await {
        Ok(snapshot) => Ok(snapshot),
        Err(error) => match repo.github_repo.clone() {
            Some(github_repo) => {
                telemetry::emit(source_record(
                    telemetry::event_name::SOURCE_INVENTORY,
                    "source_snapshot_fallback",
                    Some(&repo.path),
                    Some(error.to_string()),
                ));
                eprintln!(
                    "clawhip source github snapshot failed for {}: {error}; using configured github_repo={github_repo}",
                    repo.path
                );
                Ok(GitSnapshot {
                    repo_name: repo_display_name(repo),
                    repo_path: repo.path.clone(),
                    worktree_path: repo.path.clone(),
                    branch: String::new(),
                    head: String::new(),
                    commits: Vec::new(),
                    github_repo: Some(github_repo),
                })
            }
            None => Err(error),
        },
    }
}

async fn poll_github(
    config: &AppConfig,
    github_client: Option<&GitHubClient>,
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut HashMap<String, GitHubRepoState>,
    ci_baseline: &mut CIBaseline,
    ci_baseline_path: Option<&Path>,
) -> Result<()> {
    if let Err(error) = drain_pending_outbox(ci_baseline, ci_baseline_path, tx).await {
        eprintln!("clawhip source github CI outbox drain failed: {error}");
    }

    for repo in &config.monitors.git.repos {
        if !repo.emit_issue_opened && !repo.emit_pr_status && !repo.emit_pr_activity {
            continue;
        }

        let snapshot = match snapshot_github_repo(repo).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                telemetry::emit(source_record(
                    telemetry::event_name::SOURCE_DEGRADED,
                    "source_snapshot_failed",
                    Some(&repo.path),
                    Some(error.to_string()),
                ));
                eprintln!(
                    "clawhip source github snapshot failed for {}: {error}",
                    repo.path
                );
                continue;
            }
        };

        let previous = state.get(&repo.path);
        let activity = previous.map(|p| p.activity.clone()).unwrap_or_default();
        let (activity_prs, activity_window_complete, activity_baseline_cutoff) = if repo
            .emit_pr_activity
            && activity.review_scan.is_none()
            && let Some(client) = github_client
        {
            match fetch_updated_pull_requests(
                client,
                &config.monitors.github_api_base,
                repo,
                &snapshot,
                previous.map(|p| &p.activity),
            )
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    emit_source_degraded(Some(&repo.path), &error.to_string());
                    eprintln!(
                        "clawhip source GitHub activity PR fetch failed for {}: {error}",
                        repo.path
                    );
                    (HashMap::new(), false, None)
                }
            }
        } else {
            (HashMap::new(), true, None)
        };
        let issues = match poll_issues(config, github_client, repo, &snapshot, previous, tx).await {
            Ok(issues) => issues,
            Err(error) => {
                eprintln!(
                    "clawhip source GitHub issue processing failed for {}: {error}",
                    repo.path
                );
                previous
                    .map(|entry| entry.issues.clone())
                    .unwrap_or_default()
            }
        };
        let (prs, prs_window_complete) =
            match poll_pull_requests(config, github_client, repo, &snapshot, previous, tx).await {
                Ok(result) => result,
                Err(error) => {
                    eprintln!(
                        "clawhip source GitHub pull request processing failed for {}: {error}",
                        repo.path
                    );
                    (
                        previous.map(|entry| entry.prs.clone()).unwrap_or_default(),
                        false,
                    )
                }
            };
        let activity = poll_pr_activity(
            config,
            github_client,
            repo,
            &snapshot,
            activity,
            &activity_prs,
            &issues,
            activity_window_complete,
            activity_baseline_cutoff.as_deref(),
            tx,
        )
        .await?;
        let repo_key = ci_baseline_repo_key(&snapshot, repo);
        let fetch_generation =
            ci_baseline.reserve_window_generation(&repo_key, &repo.path, &repo.path);
        let repo_ci_baseline = ci_baseline.repos.get(&repo_key).cloned();
        let (ci, ci_baseline_established, window_complete) = match poll_ci_statuses(
            config,
            github_client,
            repo,
            &snapshot,
            previous,
            &prs,
            prs_window_complete,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => {
                eprintln!(
                    "clawhip source GitHub CI processing failed for {}: {error}",
                    repo.path
                );
                (
                    previous.map(|entry| entry.ci.clone()).unwrap_or_default(),
                    previous
                        .map(|entry| entry.ci_baseline_established)
                        .unwrap_or(false),
                    false,
                )
            }
        };
        let generation_is_current = ci_baseline
            .repos
            .get(&repo_key)
            .is_some_and(|baseline| baseline.window_generation == fetch_generation);
        let ci_events = if generation_is_current {
            if let Some(previous) = previous {
                collect_ci_events(
                    repo,
                    &snapshot.repo_name,
                    previous.ci_baseline_established,
                    &previous.ci,
                    &ci,
                    repo_ci_baseline.as_ref(),
                )
            } else if repo_ci_baseline
                .as_ref()
                .is_some_and(|baseline| baseline.ci_established)
            {
                collect_ci_events(
                    repo,
                    &snapshot.repo_name,
                    true,
                    &HashMap::new(),
                    &ci,
                    repo_ci_baseline.as_ref(),
                )
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // Durable outbox: persist pending receipts, then publish, then
        // drop the outbox. Persistence failure must not mutate in-memory
        // suppression state. A crash after persist and before send retries
        // from the outbox on the next poll.
        let mut next_baseline = ci_baseline.clone();
        let previous_ci = previous.map(|entry| &entry.ci);
        next_baseline.record_terminal_runs_filtered(
            &repo_key,
            &repo.path,
            &repo.path,
            &ci,
            (fetch_generation, window_complete),
            |snapshot| {
                if previous_ci.is_none() || window_complete {
                    return true;
                }
                previous_ci.is_some_and(|old_map| {
                    old_map
                        .values()
                        .any(|old| snapshots_same_run(old, snapshot) && old.is_terminal())
                }) || ci_events.iter().any(|event| {
                    previous_snapshot_for_event(&ci, event)
                        .is_some_and(|matched| snapshots_same_run(matched, snapshot))
                })
            },
        );
        next_baseline.enqueue_pending(&repo_key, &repo.path, &ci_events, &ci);
        if ci_baseline_established {
            next_baseline
                .repo_baseline_mut(&repo_key, &repo.path)
                .ci_established = true;
        }
        match commit_ci_baseline(ci_baseline_path, &next_baseline) {
            Ok(committed) => {
                *ci_baseline = committed;
            }
            Err(error) => {
                telemetry::emit(source_record(
                    telemetry::event_name::SOURCE_DEGRADED,
                    "ci_baseline_persist_failed",
                    Some(&repo.path),
                    Some(error.to_string()),
                ));
                eprintln!(
                    "clawhip source github CI baseline persist failed for {}: {error}",
                    repo.path
                );
                state.insert(
                    repo.path.clone(),
                    GitHubRepoState {
                        activity,
                        issues,
                        prs,
                        ci: previous.map(|entry| entry.ci.clone()).unwrap_or_default(),
                        ci_baseline_established: previous
                            .map(|entry| entry.ci_baseline_established)
                            .unwrap_or(false),
                    },
                );
                continue;
            }
        }

        let delivered: Vec<PendingCiDelivery> = ci_events
            .iter()
            .map(|event| {
                let identities = previous_snapshot_for_event(&ci, event)
                    .map(GitHubCISnapshot::identities)
                    .unwrap_or_default();
                PendingCiDelivery::from_event(event, identities)
            })
            .collect();
        if !delivered.is_empty() {
            send_then_ack_prefix(tx, ci_baseline, ci_baseline_path, ci_events, &delivered).await?;
        }

        state.insert(
            repo.path.clone(),
            GitHubRepoState {
                activity,
                issues,
                prs,
                ci,
                ci_baseline_established,
            },
        );
    }

    Ok(())
}

async fn poll_issues(
    config: &AppConfig,
    github_client: Option<&GitHubClient>,
    repo: &GitRepoMonitor,
    snapshot: &GitSnapshot,
    previous: Option<&GitHubRepoState>,
    tx: &mpsc::Sender<IncomingEvent>,
) -> Result<HashMap<u64, IssueSnapshot>> {
    if !repo.emit_issue_opened {
        return Ok(previous
            .map(|entry| entry.issues.clone())
            .unwrap_or_default());
    }

    let Some(client) = github_client else {
        return Ok(previous
            .map(|entry| entry.issues.clone())
            .unwrap_or_default());
    };

    match fetch_issues(client, &config.monitors.github_api_base, repo, snapshot).await {
        Ok(issues) => {
            if let Some(previous) = previous {
                for event in
                    collect_issue_events(repo, &snapshot.repo_name, &previous.issues, &issues)
                {
                    send_event(tx, event).await?;
                }
            }
            Ok(issues)
        }
        Err(error) => {
            if emit_source_degraded(Some(&repo.path), &error.to_string()) {
                eprintln!(
                    "clawhip source GitHub issue polling failed for {}: {error}",
                    repo.path
                );
            }
            Ok(previous
                .map(|entry| entry.issues.clone())
                .unwrap_or_default())
        }
    }
}

fn remember_activity_id(ids: &mut VecDeque<u64>, id: u64) {
    if !ids.contains(&id) {
        ids.push_back(id);
        if ids.len() > MAX_EMITTED_COMMENT_IDS {
            ids.pop_front();
        }
    }
}

/// Ends pending ownership of a delivered comment. Its dedupe receipt may have
/// been evicted while it waited for metadata, so move it to the newest end of
/// the matching feed FIFO; otherwise the feed could emit an edit of it again.
fn refresh_delivered_comment_id(state: &mut PRActivityState, comment: &PendingPRComment) {
    let ids = if comment.path.is_some() {
        &mut state.emitted_review_comment_ids
    } else {
        &mut state.emitted_pr_comment_ids
    };
    if let Some(index) = ids.iter().position(|id| *id == comment.id) {
        ids.remove(index);
    }
    remember_activity_id(ids, comment.id);
}

fn pr_comment_payload(
    repo: &str,
    number: u64,
    pr: Option<&PRActivityMetadata>,
    comment_id: u64,
    author: &str,
    url: &str,
) -> GitHubPRCommentEvent {
    GitHubPRCommentEvent {
        repo: repo.to_string(),
        number,
        title: pr.map(|pr| pr.title.clone()).unwrap_or_default(),
        author: author.to_string(),
        pr_author: pr.map(|pr| pr.author.clone()).unwrap_or_default(),
        url: url.to_string(),
        comment_id,
    }
}

fn cache_pr_metadata(
    state: &mut PRActivityState,
    number: u64,
    metadata: Option<PRActivityMetadata>,
) {
    if !state.metadata.contains_key(&number) {
        state.metadata_order.push_back(number);
        if state.metadata_order.len() > MAX_PR_METADATA_CACHE
            && let Some(oldest) = state.metadata_order.pop_front()
        {
            state.metadata.remove(&oldest);
        }
    }
    state.metadata.insert(number, metadata);
}

// Outer None is unknown; inner None is a cached tombstone.
fn known_pr_metadata(
    number: u64,
    state: &mut PRActivityState,
    prs: &HashMap<u64, PullRequestSnapshot>,
) -> Option<Option<PRActivityMetadata>> {
    if let Some(pr) = prs.get(&number).or_else(|| {
        state
            .review_scan
            .as_ref()
            .and_then(|scan| scan.prs.get(&number))
    }) && !pr.author.is_empty()
    {
        let metadata = PRActivityMetadata {
            title: pr.title.clone(),
            author: pr.author.clone(),
        };
        cache_pr_metadata(state, number, Some(metadata.clone()));
        return Some(Some(metadata));
    }
    state.metadata.get(&number).cloned()
}

enum MetadataLookup {
    Resolved,
    Failed,
    Backoff,
}

async fn resolve_pr_metadata(
    client: &GitHubClient,
    api_base: &str,
    github_repo: &str,
    repo_path: &str,
    number: u64,
    state: &mut PRActivityState,
) -> MetadataLookup {
    let result: Result<Option<PRActivityMetadata>> = async {
        let response = github_get_allow_unavailable(
            client,
            api_base,
            &format!("repos/{github_repo}/pulls/{number}"),
            &[],
            "PR activity metadata",
        )
        .await?;
        if matches!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
        ) {
            return Ok(None);
        }
        let (_, pr) = pull_request_snapshot(response.json::<GitHubPullRequest>().await?);
        if pr.author.is_empty() {
            return Err("GitHub PR metadata response has no author".into());
        }
        Ok(Some(PRActivityMetadata {
            title: pr.title,
            author: pr.author,
        }))
    }
    .await;
    match result {
        Ok(metadata) => {
            cache_pr_metadata(state, number, metadata.clone());
            MetadataLookup::Resolved
        }
        Err(error) => {
            emit_source_degraded(
                Some(repo_path),
                &format!("GitHub PR metadata lookup failed for #{number}: {error}"),
            );
            if client.backoff.lock().is_ok_and(|backoff| {
                backoff
                    .until
                    .is_some_and(|until| until > std::time::Instant::now())
            }) {
                MetadataLookup::Backoff
            } else {
                MetadataLookup::Failed
            }
        }
    }
}

async fn emit_pending_comment(
    comment: &PendingPRComment,
    metadata: Option<&PRActivityMetadata>,
    repo: &GitRepoMonitor,
    repo_name: &str,
    issues: &HashMap<u64, IssueSnapshot>,
    tx: &mpsc::Sender<IncomingEvent>,
) -> Result<()> {
    if comment.is_issue_comment {
        // Emit issue comment event
        let (title, comment_count) = if let Some(issue) = issues.get(&comment.number) {
            (issue.title.clone(), issue.comments)
        } else {
            (String::new(), 0)
        };
        let event = IncomingEvent::github_issue_commented(
            repo_name.to_string(),
            comment.number,
            title,
            comment_count,
            repo.channel.clone(),
        );
        send_event(
            tx,
            event
                .with_mention(repo.mention.clone())
                .with_format(repo.format.clone()),
        )
        .await
    } else {
        // Emit PR comment event
        let payload = pr_comment_payload(
            repo_name,
            comment.number,
            metadata,
            comment.id,
            &comment.author,
            &comment.url,
        );
        let event = if let Some(path) = &comment.path {
            IncomingEvent::github_pr_review_commented(
                GitHubPRReviewCommentEvent {
                    comment: payload,
                    path: path.clone(),
                },
                repo.channel.clone(),
            )
        } else {
            IncomingEvent::github_pr_commented(payload, repo.channel.clone())
        };
        send_event(
            tx,
            event
                .with_mention(repo.mention.clone())
                .with_format(repo.format.clone()),
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn drain_pending_comments(
    state: &mut PRActivityState,
    client: &GitHubClient,
    api_base: &str,
    github_repo: &str,
    repo: &GitRepoMonitor,
    repo_name: &str,
    prs: &HashMap<u64, PullRequestSnapshot>,
    issues: &HashMap<u64, IssueSnapshot>,
    tx: &mpsc::Sender<IncomingEvent>,
    attempted: &mut HashMap<u64, MetadataLookup>,
    counted: &mut HashSet<(bool, u64)>,
) -> Result<()> {
    let mut retained = VecDeque::new();
    let mut deferred = false;
    while let Some(mut comment) = state.pending_comments.pop_front() {
        // Eligibility was established at enqueue time, not against a later feed lower bound.
        debug_assert!(!comment.created_at.is_empty());
        if comment.is_issue_comment {
            // Issue comments don't need metadata fetching, emit immediately
            emit_pending_comment(&comment, None, repo, repo_name, issues, tx).await?;
            refresh_delivered_comment_id(state, &comment);
        } else {
            // PR comments may need metadata fetching
            if known_pr_metadata(comment.number, state, prs).is_none()
                && !attempted.contains_key(&comment.number)
            {
                if attempted.len() < MAX_PR_METADATA_LOOKUPS {
                    let outcome = resolve_pr_metadata(
                        client,
                        api_base,
                        github_repo,
                        &repo.path,
                        comment.number,
                        state,
                    )
                    .await;
                    attempted.insert(comment.number, outcome);
                } else {
                    deferred = true;
                }
            }
            if let Some(metadata) = known_pr_metadata(comment.number, state, prs) {
                emit_pending_comment(&comment, metadata.as_ref(), repo, repo_name, issues, tx)
                    .await?;
                refresh_delivered_comment_id(state, &comment);
            } else {
                if matches!(attempted.get(&comment.number), Some(MetadataLookup::Failed))
                    && counted.insert((comment.path.is_some(), comment.id))
                {
                    comment.attempts += 1;
                }
                if comment.attempts >= MAX_METADATA_ATTEMPTS {
                    emit_source_degraded(
                        Some(&repo.path),
                        "GitHub PR metadata attempt cap reached; emitting empty metadata",
                    );
                    emit_pending_comment(&comment, None, repo, repo_name, issues, tx).await?;
                    refresh_delivered_comment_id(state, &comment);
                } else {
                    retained.push_back(comment);
                }
            }
        }
    }
    state.pending_comments = retained;
    if deferred {
        emit_source_degraded(
            Some(&repo.path),
            "GitHub PR metadata lookup budget exhausted; pending comments deferred",
        );
    }
    Ok(())
}

async fn accept_activity_comment(
    comment: PendingPRComment,
    state: &mut PRActivityState,
    prs: &HashMap<u64, PullRequestSnapshot>,
    issues: &HashMap<u64, IssueSnapshot>,
    repo: &GitRepoMonitor,
    repo_name: &str,
    tx: &mpsc::Sender<IncomingEvent>,
) -> Result<()> {
    if let Some(metadata) = known_pr_metadata(comment.number, state, prs) {
        emit_pending_comment(&comment, metadata.as_ref(), repo, repo_name, issues, tx).await?;
    } else {
        if state.pending_comments.len() == MAX_PENDING_PR_COMMENTS {
            let oldest = state.pending_comments.pop_front().expect("queue is full");
            let metadata = known_pr_metadata(oldest.number, state, prs).flatten();
            emit_source_degraded(
                Some(&repo.path),
                "GitHub PR metadata pending queue overflow; emitting oldest comment",
            );
            emit_pending_comment(&oldest, metadata.as_ref(), repo, repo_name, issues, tx).await?;
            refresh_delivered_comment_id(state, &oldest);
        }
        state.pending_comments.push_back(comment);
    }
    Ok(())
}

fn github_response_date(headers: &reqwest::header::HeaderMap) -> Option<String> {
    use time::format_description::well_known::{Rfc2822, Rfc3339};

    let date = headers.get(reqwest::header::DATE)?.to_str().ok()?;
    time::OffsetDateTime::parse(date, &Rfc2822)
        .ok()?
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .ok()
}

/// Fetch a bounded window; cold comment baselines only need the newest page.
async fn fetch_pr_activity_pages<T: serde::de::DeserializeOwned>(
    client: &GitHubClient,
    api_base: &str,
    endpoint: &str,
    is_comment_feed: bool,
    cursor: Option<&str>,
) -> Result<(Vec<T>, bool, Option<String>)> {
    let since = cursor.map(subtract_one_second).unwrap_or_default();
    let mut entries = Vec::new();
    let mut fetched_at = None;
    let mut seen_next_urls = HashSet::new();
    for page in 1..=MAX_COMMENT_PAGES {
        let page_str = page.to_string();
        seen_next_urls.insert(pagination_page_key(&format!(
            "{}/{endpoint}?page={page_str}",
            api_base.trim_end_matches('/')
        )));
        let page_query = [("per_page", "100"), ("page", page_str.as_str())];
        let comment_query = [
            ("sort", "updated"),
            ("direction", if cursor.is_none() { "desc" } else { "asc" }),
            ("since", since.as_str()),
            ("per_page", "100"),
            ("page", page_str.as_str()),
        ];
        let response = github_get(
            client,
            api_base,
            endpoint,
            if is_comment_feed {
                &comment_query
            } else {
                &page_query
            },
            endpoint,
        )
        .await?;
        let next_url = github_link_next(response.headers());
        if page == 1 {
            fetched_at = github_response_date(response.headers());
        }
        entries.extend(response.json::<Vec<T>>().await?);
        if is_comment_feed && cursor.is_none() {
            return Ok((entries, true, fetched_at));
        }
        let Some(next_url) = next_url else {
            return Ok((entries, true, fetched_at));
        };
        if !seen_next_urls.insert(pagination_page_key(&next_url)) {
            return Ok((entries, false, fetched_at));
        }
    }
    Ok((entries, false, fetched_at))
}

#[allow(clippy::too_many_arguments)]
async fn poll_pr_activity(
    config: &AppConfig,
    github_client: Option<&GitHubClient>,
    repo: &GitRepoMonitor,
    snapshot: &GitSnapshot,
    mut state: PRActivityState,
    prs: &HashMap<u64, PullRequestSnapshot>,
    issues: &HashMap<u64, IssueSnapshot>,
    prs_complete: bool,
    baseline_cutoff: Option<&str>,
    tx: &mpsc::Sender<IncomingEvent>,
) -> Result<PRActivityState> {
    // Process comments from /issues/comments if either PR activity or issue opened is enabled
    if !repo.emit_pr_activity && !repo.emit_issue_opened {
        return Ok(state);
    }
    let Some(client) = github_client else {
        return Ok(state);
    };
    let Some(github_repo) = snapshot.github_repo.as_deref() else {
        return Ok(state);
    };
    let api_base = &config.monitors.github_api_base;
    let cutoff = current_utc_iso8601();
    // Refresh cached titles when a later PR window includes them, even without comments.
    for (number, pr) in prs {
        if state.metadata.contains_key(number) && !pr.author.is_empty() {
            cache_pr_metadata(
                &mut state,
                *number,
                Some(PRActivityMetadata {
                    title: pr.title.clone(),
                    author: pr.author.clone(),
                }),
            );
        }
    }
    let mut metadata_attempted = HashMap::new();
    let mut metadata_counted = HashSet::new();
    drain_pending_comments(
        &mut state,
        client,
        api_base,
        github_repo,
        repo,
        &snapshot.repo_name,
        prs,
        issues,
        tx,
        &mut metadata_attempted,
        &mut metadata_counted,
    )
    .await?;
    let pr_comment_cursor = state.pr_comment_cursor.clone();
    match fetch_pr_activity_pages::<GitHubComment>(
        client,
        api_base,
        &format!("repos/{github_repo}/issues/comments"),
        true,
        pr_comment_cursor.as_deref(),
    )
    .await
    {
        Ok((comments, complete, fetched_at)) => {
            // Issue comments also advance the feed position, but never enter PR dedup.
            let max_updated = comments
                .iter()
                .map(|comment| comment.updated_at.clone())
                .chain(pr_comment_cursor.clone())
                .max()
                .unwrap_or_else(|| fetched_at.clone().unwrap_or_else(|| cutoff.clone()));
            let lower = state.pr_comment_lower.clone();
            for comment in comments {
                // Skip if we've already processed this comment
                if state.emitted_pr_comment_ids.contains(&comment.id)
                    || state
                        .pending_comments
                        .iter()
                        .any(|pending| pending.path.is_none() && pending.id == comment.id)
                {
                    continue;
                }
                if let Some(lower) = lower.as_deref()
                    && comment.created_at.as_str() < lower
                {
                    continue;
                }

                let mut was_queued = false;
                if let Some(pr_number) = comment.pr_number() {
                    // Handle PR comment only if PR activity is enabled
                    if repo.emit_pr_activity {
                        accept_activity_comment(
                            PendingPRComment {
                                id: comment.id,
                                number: pr_number,
                                author: comment.user.map(|user| user.login).unwrap_or_default(),
                                url: comment.html_url,
                                path: None,
                                created_at: comment.created_at,
                                attempts: 0,
                                is_issue_comment: false,
                            },
                            &mut state,
                            prs,
                            issues,
                            repo,
                            &snapshot.repo_name,
                            tx,
                        )
                        .await?;
                        was_queued = true;
                    }
                } else if let Some(issue_number) = comment.issue_number() {
                    // Handle issue comment only if issue opened is enabled
                    if repo.emit_issue_opened {
                        accept_activity_comment(
                            PendingPRComment {
                                id: comment.id,
                                number: issue_number,
                                author: comment.user.map(|user| user.login).unwrap_or_default(),
                                url: comment.html_url,
                                path: None,
                                created_at: comment.created_at,
                                attempts: 0,
                                is_issue_comment: true,
                            },
                            &mut state,
                            prs,
                            issues,
                            repo,
                            &snapshot.repo_name,
                            tx,
                        )
                        .await?;
                        was_queued = true;
                    }
                }
                // Only mark as emitted if it was actually queued (will be marked again after actual emission)
                if was_queued {
                    remember_activity_id(&mut state.emitted_pr_comment_ids, comment.id);
                }
            }
            state.pr_comment_cursor = Some(max_updated.clone());
            if pr_comment_cursor.is_none() {
                state.pr_comment_lower = Some(fetched_at.unwrap_or(max_updated));
            } else if complete {
                state.pr_comment_lower = Some(lower.unwrap().max(max_updated));
            }
            if !complete {
                emit_source_degraded(
                    Some(&repo.path),
                    "GitHub PR conversation comment pagination cap reached",
                );
            }
        }
        Err(error) => {
            emit_source_degraded(
                Some(&repo.path),
                &format!("GitHub PR conversation comments fetch failed: {error}"),
            );
        }
    }
    let comment_cursor = state.review_comment_cursor.clone();
    match fetch_pr_activity_pages::<GitHubReviewComment>(
        client,
        api_base,
        &format!("repos/{github_repo}/pulls/comments"),
        true,
        comment_cursor.as_deref(),
    )
    .await
    {
        Ok((comments, complete, fetched_at)) => {
            let max_updated = comments
                .iter()
                .map(|comment| comment.updated_at.clone())
                .chain(comment_cursor.clone())
                .max()
                .unwrap_or_else(|| fetched_at.clone().unwrap_or_else(|| cutoff.clone()));
            let lower = state.review_comment_lower.clone();
            for comment in comments {
                if state.emitted_review_comment_ids.contains(&comment.id)
                    || state
                        .pending_comments
                        .iter()
                        .any(|pending| pending.path.is_some() && pending.id == comment.id)
                {
                    continue;
                }
                if let Some(lower) = lower.as_deref() {
                    if comment.created_at.as_str() < lower {
                        continue;
                    }
                    let Some(number) = comment
                        .pull_request_url
                        .rsplit('/')
                        .next()
                        .and_then(|n| n.parse().ok())
                    else {
                        continue;
                    };
                    accept_activity_comment(
                        PendingPRComment {
                            id: comment.id,
                            number,
                            author: comment.user.map(|user| user.login).unwrap_or_default(),
                            url: comment.html_url,
                            path: Some(comment.path),
                            created_at: comment.created_at,
                            attempts: 0,
                            is_issue_comment: false,
                        },
                        &mut state,
                        prs,
                        issues,
                        repo,
                        &snapshot.repo_name,
                        tx,
                    )
                    .await?;
                }
                remember_activity_id(&mut state.emitted_review_comment_ids, comment.id);
            }
            state.review_comment_cursor = Some(max_updated.clone());
            if comment_cursor.is_none() {
                state.review_comment_lower = Some(fetched_at.unwrap_or(max_updated));
            } else if complete {
                state.review_comment_lower = Some(lower.unwrap().max(max_updated));
            }
            if !complete {
                emit_source_degraded(
                    Some(&repo.path),
                    "GitHub inline review comment pagination cap reached",
                );
            }
        }
        Err(error) => {
            emit_source_degraded(
                Some(&repo.path),
                &format!("GitHub inline review comments fetch failed: {error}"),
            );
        }
    }

    // Newly queued comments may use the remaining budget, after both feeds have
    // joined the same FIFO. Existing pending work always gets first access.
    drain_pending_comments(
        &mut state,
        client,
        api_base,
        github_repo,
        repo,
        &snapshot.repo_name,
        prs,
        issues,
        tx,
        &mut metadata_attempted,
        &mut metadata_counted,
    )
    .await?;
    if state.review_scan.is_none() {
        // An empty failed list must not establish or advance the baseline.
        if !prs_complete && prs.is_empty() {
            return Ok(state);
        }
        let newest_update = prs
            .values()
            .map(|pr| pr.updated_at.clone())
            .chain(state.review_watermark.clone())
            .max()
            .unwrap_or(cutoff);
        let upper = if state.review_watermark.is_none() {
            baseline_cutoff.unwrap_or(&newest_update).to_string()
        } else {
            newest_update.clone()
        };
        let lower = state
            .review_watermark
            .clone()
            .unwrap_or_else(|| upper.clone());
        let mut candidates: Vec<_> = prs
            .iter()
            .filter(|(_, pr)| {
                if state.review_watermark.is_none() {
                    pr.updated_at == newest_update
                } else {
                    pr.updated_at >= lower
                }
            })
            .collect();
        candidates.sort_unstable_by(|(a_number, a), (b_number, b)| {
            a.updated_at
                .cmp(&b.updated_at)
                .then_with(|| a_number.cmp(b_number))
        });
        state.review_scan = Some(ReviewScan {
            lower,
            upper,
            pending: candidates.into_iter().map(|(number, _)| *number).collect(),
            prs: prs.clone(),
        });
    }
    let mut scan = state.review_scan.take().expect("review scan was built");
    for _ in 0..MAX_REVIEW_FETCH_PRS {
        let Some(number) = scan.pending.pop_front() else {
            break;
        };
        let pr = &scan.prs[&number];
        match fetch_pr_activity_pages::<GitHubReview>(
            client,
            api_base,
            &format!("repos/{github_repo}/pulls/{number}/reviews"),
            false,
            None,
        )
        .await
        {
            Ok((reviews, fetched_all, _)) => {
                for review in reviews {
                    let review_state = match review.state.as_str() {
                        "APPROVED" => "approved",
                        "CHANGES_REQUESTED" => "changes_requested",
                        "COMMENTED" => "commented",
                        _ => continue,
                    };
                    let Some(submitted_at) = review.submitted_at.as_deref() else {
                        continue;
                    };
                    if state.emitted_review_ids.contains(&review.id) {
                        continue;
                    }
                    let seeding = state.review_watermark.is_none();
                    if !seeding && submitted_at < scan.lower.as_str() {
                        continue;
                    }
                    if !seeding {
                        send_event(
                            tx,
                            IncomingEvent::github_pr_reviewed(
                                GitHubPRReviewEvent {
                                    repo: snapshot.repo_name.clone(),
                                    number,
                                    title: pr.title.clone(),
                                    author: review.user.map(|user| user.login).unwrap_or_default(),
                                    pr_author: pr.author.clone(),
                                    url: review.html_url,
                                    review_id: review.id,
                                    state: review_state.to_string(),
                                },
                                repo.channel.clone(),
                            )
                            .with_mention(repo.mention.clone())
                            .with_format(repo.format.clone()),
                        )
                        .await?;
                    }
                    remember_activity_id(&mut state.emitted_review_ids, review.id);
                }
                if !fetched_all {
                    emit_source_degraded(
                        Some(&repo.path),
                        "GitHub per-PR review pagination cap reached",
                    );
                    scan.pending.push_front(number);
                    break;
                }
            }
            Err(error) => {
                emit_source_degraded(
                    Some(&repo.path),
                    &format!("GitHub reviews fetch failed for #{number}: {error}"),
                );
                scan.pending.push_front(number);
                break;
            }
        }
    }
    if scan.pending.is_empty() {
        state.review_watermark = Some(scan.upper);
    } else {
        state.review_scan = Some(scan);
    }
    Ok(state)
}

async fn poll_pull_requests(
    config: &AppConfig,
    github_client: Option<&GitHubClient>,
    repo: &GitRepoMonitor,
    snapshot: &GitSnapshot,
    previous: Option<&GitHubRepoState>,
    tx: &mpsc::Sender<IncomingEvent>,
) -> Result<(HashMap<u64, PullRequestSnapshot>, bool)> {
    if !repo.emit_pr_status {
        return Ok((
            previous.map(|entry| entry.prs.clone()).unwrap_or_default(),
            true,
        ));
    }

    let Some(client) = github_client else {
        return Ok((
            previous.map(|entry| entry.prs.clone()).unwrap_or_default(),
            true,
        ));
    };

    match fetch_pull_requests(client, &config.monitors.github_api_base, repo, snapshot).await {
        Ok((prs, complete)) => {
            if let Some(previous) = previous {
                for (number, pr) in &prs {
                    match previous.prs.get(number) {
                        Some(old) if old.status == pr.status => {}
                        old => {
                            send_event(
                                tx,
                                IncomingEvent::github_pr_status_changed(
                                    snapshot.repo_name.clone(),
                                    *number,
                                    pr.title.clone(),
                                    old.map(|value| value.status.clone())
                                        .unwrap_or_else(|| "<new>".to_string()),
                                    pr.status.clone(),
                                    pr.url.clone(),
                                    repo.channel.clone(),
                                )
                                .with_mention(repo.mention.clone())
                                .with_format(repo.format.clone()),
                            )
                            .await?;
                        }
                    }
                }
            }
            Ok((prs, complete))
        }
        Err(error) => {
            if emit_source_degraded(Some(&repo.path), &error.to_string()) {
                eprintln!(
                    "clawhip source GitHub polling failed for {}: {error}",
                    repo.path
                );
            }
            Ok((
                previous.map(|entry| entry.prs.clone()).unwrap_or_default(),
                false,
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn poll_ci_statuses(
    config: &AppConfig,
    github_client: Option<&GitHubClient>,
    repo: &GitRepoMonitor,
    snapshot: &GitSnapshot,
    previous: Option<&GitHubRepoState>,
    prs: &HashMap<u64, PullRequestSnapshot>,
    prs_window_complete: bool,
) -> Result<(HashMap<String, GitHubCISnapshot>, bool, bool)> {
    if !repo.emit_pr_status {
        return Ok((
            previous.map(|entry| entry.ci.clone()).unwrap_or_default(),
            previous
                .map(|entry| entry.ci_baseline_established)
                .unwrap_or(false),
            true,
        ));
    }

    let Some(client) = github_client else {
        return Ok((
            previous.map(|entry| entry.ci.clone()).unwrap_or_default(),
            previous
                .map(|entry| entry.ci_baseline_established)
                .unwrap_or(false),
            true,
        ));
    };

    let open_prs = prs
        .iter()
        .filter(|(_, pr)| pr.status == "open")
        .map(|(number, pr)| (*number, pr))
        .collect::<Vec<_>>();

    match fetch_ci_statuses(
        client,
        &config.monitors.github_api_base,
        repo,
        snapshot,
        &open_prs,
    )
    .await
    {
        Ok((fetched, fetched_window_complete)) => {
            let window_complete = prs_window_complete && fetched_window_complete;
            let ci = if !window_complete {
                if let Some(previous) = previous {
                    merge_incomplete_ci(&previous.ci, fetched)
                } else {
                    fetched
                }
            } else {
                fetched
            };
            let established = true;
            Ok((ci, established, window_complete))
        }
        Err(error) => {
            if emit_source_degraded(Some(&repo.path), &error.to_string()) {
                eprintln!(
                    "clawhip source GitHub CI polling failed for {}: {error}",
                    repo.path
                );
            }
            // Keep the last snapshot for diffing but drop the established flag:
            // the failed poll may be a partial view (pagination/cursor loss), and
            // treating it as complete would replay whatever the next full poll
            // returns — including terminal runs already delivered before (#317).
            Ok((
                previous.map(|entry| entry.ci.clone()).unwrap_or_default(),
                false,
                false,
            ))
        }
    }
}

fn previous_snapshot_for_event<'a>(
    previous: &'a HashMap<String, GitHubCISnapshot>,
    event: &IncomingEvent,
) -> Option<&'a GitHubCISnapshot> {
    let payload = event.payload.as_object()?;
    let workflow = payload.get("workflow")?.as_str()?;
    let check_run_id = payload.get("check_run_id").and_then(|value| value.as_str());
    let run_id = payload.get("run_id").and_then(|value| value.as_str());
    let run_attempt = payload
        .get("run_attempt")
        .and_then(|value| value.as_u64())
        .map(|value| value as u32)
        .unwrap_or(1);
    previous.values().find(|ci| {
        if let Some(check_run_id) = check_run_id {
            if ci.check_run_id.as_deref() != Some(check_run_id) {
                return false;
            }
            return match (run_id, ci.run_id.as_deref()) {
                (Some(event_run), Some(ci_run)) => {
                    ci_run == event_run && ci.run_attempt() == run_attempt_or_one(run_attempt)
                }
                _ => true,
            };
        }
        if ci.check_run_id.is_some() {
            return false;
        }
        if let Some(run_id) = run_id {
            return ci.run_id.as_deref() == Some(run_id)
                && ci.run_attempt() == run_attempt_or_one(run_attempt);
        }
        ci.workflow == workflow
            && ci.sha
                == payload
                    .get("sha")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
            && ci.run_attempt() == run_attempt_or_one(run_attempt)
    })
}

fn emit_source_degraded(repo_path: Option<&str>, error: &str) -> bool {
    static LAST_REPORTS: OnceLock<Mutex<HashMap<String, std::time::Instant>>> = OnceLock::new();
    let reports = LAST_REPORTS.get_or_init(|| Mutex::new(HashMap::new()));
    let key = error.to_string();
    // Current-thread async tests must not throttle one another's telemetry.
    #[cfg(test)]
    let key = format!("{:?}:{key}", std::thread::current().id());
    let should_report = reports
        .lock()
        .map(|mut entries| {
            let now = std::time::Instant::now();
            if entries
                .get(&key)
                .is_some_and(|last| now.duration_since(*last) < GITHUB_RATE_LIMIT_BACKOFF)
            {
                false
            } else {
                entries.insert(key, now);
                true
            }
        })
        .unwrap_or(true);
    if should_report {
        telemetry::emit(source_record(
            telemetry::event_name::SOURCE_DEGRADED,
            "source_poll_failed",
            repo_path,
            Some(error.to_string()),
        ));
    }
    should_report
}

fn source_record(
    event_name: &str,
    reason_code: &str,
    repo_path: Option<&str>,
    error: Option<String>,
) -> serde_json::Map<String, serde_json::Value> {
    let correlation = format!("source:github:{}", repo_path.unwrap_or("inventory"));
    let mut record = telemetry::record(event_name, reason_code, correlation);
    record.insert("source".to_string(), serde_json::json!("github"));
    if let Some(repo_path) = repo_path {
        record.insert("repo_path".to_string(), serde_json::json!(repo_path));
    }
    if let Some(error) = error {
        record.insert("error".to_string(), serde_json::json!(error));
    }
    record
}

async fn send_event(tx: &mpsc::Sender<IncomingEvent>, event: IncomingEvent) -> Result<()> {
    tx.send(event)
        .await
        .map_err(|error| format!("github source channel closed: {error}").into())
}

async fn github_get(
    client: &GitHubClient,
    api_base: &str,
    path: &str,
    query: &[(&str, &str)],
    context: &str,
) -> Result<reqwest::Response> {
    let url = format!(
        "{}/{}",
        api_base.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let response = github_request(client, client.http.get(&url).query(query), context).await?;
    if matches!(
        response.status(),
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
    ) {
        return Err(github_api_error(response.status()).into());
    }
    Ok(response)
}

/// Shared transport/backoff. Callers inspect status so conditional pollers can read
/// polling headers on failures and accept 304 without changing legacy GET behavior.
pub(crate) async fn github_request(
    client: &GitHubClient,
    request: reqwest::RequestBuilder,
    context: &str,
) -> Result<reqwest::Response> {
    if let Ok(backoff) = client.backoff.lock()
        && backoff
            .until
            .is_some_and(|until| until > std::time::Instant::now())
    {
        return Err("GitHub API rate limit backoff is active; retrying later".into());
    }
    eprintln!("clawhip source github: GET ({context})");

    let response = request.send().await?;
    let status = response.status();

    if matches!(
        status,
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
    ) {
        return Ok(response);
    }
    if !status.is_success() {
        let rate_limited = status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
        if rate_limited && let Ok(mut backoff) = client.backoff.lock() {
            backoff.until = Some(std::time::Instant::now() + GITHUB_RATE_LIMIT_BACKOFF);
            if !backoff.reported {
                eprintln!(
                    "clawhip source github: API rate limit reached; backing off for 60 seconds"
                );
                backoff.reported = true;
            }
        }
        return Ok(response);
    }

    if let Ok(mut backoff) = client.backoff.lock() {
        backoff.until = None;
        backoff.reported = false;
    }
    eprintln!("clawhip source github: GET ({context}) -> {status}");
    Ok(response)
}

async fn github_get_allow_unavailable(
    client: &GitHubClient,
    api_base: &str,
    path: &str,
    query: &[(&str, &str)],
    context: &str,
) -> Result<reqwest::Response> {
    let url = format!(
        "{}/{}",
        api_base.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let response = github_request(client, client.http.get(&url).query(query), context).await?;
    let status = response.status();

    if matches!(
        status,
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
    ) {
        return Ok(response);
    }
    if !status.is_success() {
        let rate_limited = status == reqwest::StatusCode::FORBIDDEN
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
        if rate_limited && let Ok(mut backoff) = client.backoff.lock() {
            backoff.until = Some(std::time::Instant::now() + GITHUB_RATE_LIMIT_BACKOFF);
            if !backoff.reported {
                eprintln!(
                    "clawhip source github: API rate limit reached; backing off for 60 seconds"
                );
                backoff.reported = true;
            }
        }
        return Err(github_api_error(status).into());
    }

    if let Ok(mut backoff) = client.backoff.lock() {
        backoff.until = None;
        backoff.reported = false;
    }
    Ok(response)
}

fn github_api_error(status: reqwest::StatusCode) -> String {
    format!("GitHub API request failed with {status}")
}

fn collect_issue_events(
    repo: &GitRepoMonitor,
    repo_name: &str,
    previous: &HashMap<u64, IssueSnapshot>,
    current: &HashMap<u64, IssueSnapshot>,
) -> Vec<IncomingEvent> {
    let mut events = Vec::new();
    for (number, issue) in current {
        match previous.get(number) {
            None => events.push(
                IncomingEvent::github_issue_opened(
                    repo_name.to_string(),
                    *number,
                    issue.title.clone(),
                    repo.channel.clone(),
                )
                .with_mention(repo.mention.clone())
                .with_format(repo.format.clone()),
            ),
            Some(old) => {
                if old.state != issue.state && issue.state == "closed" {
                    events.push(
                        IncomingEvent::github_issue_closed(
                            repo_name.to_string(),
                            *number,
                            issue.title.clone(),
                            repo.channel.clone(),
                        )
                        .with_mention(repo.mention.clone())
                        .with_format(repo.format.clone()),
                    );
                }
                // Note: issue-commented events are now emitted from the /issues/comments feed
                // to provide per-comment granularity instead of just comment count deltas
            }
        }
    }
    events
}

fn previous_snapshot<'a>(
    previous: &'a HashMap<String, GitHubCISnapshot>,
    ci: &GitHubCISnapshot,
) -> Option<&'a GitHubCISnapshot> {
    if let Some(old) = previous.get(&ci.dedupe_key()) {
        let same_attempt = old.run_attempt() == ci.run_attempt()
            && (old.run_id.is_none() || ci.run_id.is_none() || old.run_id == ci.run_id);
        if same_attempt {
            return Some(old);
        }
    }
    let unique = ci.unique_identities();
    if unique.is_empty() {
        return None;
    }
    previous.values().find(|old| {
        let old_ids = old.unique_identities();
        let old_checks = pending_check_identities(&old_ids);
        let new_checks = pending_check_identities(&unique);
        if !old_checks.is_empty() || !new_checks.is_empty() {
            return old.check_run_id == ci.check_run_id
                && old.run_id == ci.run_id
                && old.run_attempt() == ci.run_attempt();
        }
        old_ids
            .iter()
            .any(|identity| unique.iter().any(|candidate| candidate == identity))
            && old.run_attempt() == ci.run_attempt()
    })
}

fn collect_ci_events(
    repo: &GitRepoMonitor,
    repo_name: &str,
    previous_ci_baseline_established: bool,
    previous: &HashMap<String, GitHubCISnapshot>,
    current: &HashMap<String, GitHubCISnapshot>,
    repo_ci_baseline: Option<&RepoCIBaseline>,
) -> Vec<IncomingEvent> {
    let mut events = Vec::new();
    for ci in current.values() {
        let old = previous_snapshot(previous, ci);
        let changed = match old {
            Some(old) => {
                if ci.is_terminal() {
                    !old.is_terminal() || old.conclusion != ci.conclusion
                } else if ci.status != "completed" {
                    old.status != ci.status || old.conclusion != ci.conclusion
                } else {
                    false
                }
            }
            None => {
                if ci.status != "completed" {
                    true
                } else if ci.is_terminal() {
                    previous_ci_baseline_established
                        && !repo_ci_baseline_is_suppressed(repo_ci_baseline, ci)
                } else {
                    false
                }
            }
        };
        if !changed {
            continue;
        }

        let mut event = IncomingEvent::github_ci(
            ci.event_kind(),
            repo_name.to_string(),
            ci.pr_number,
            ci.workflow.clone(),
            ci.status.clone(),
            ci.conclusion.clone(),
            ci.sha.clone(),
            ci.url.clone(),
            ci.branch.clone(),
            repo.channel.clone(),
        )
        .with_mention(repo.mention.clone())
        .with_format(repo.format.clone());
        if let Some(payload) = event.payload.as_object_mut() {
            if let Some(run_id) = &ci.run_id {
                payload.insert("run_id".to_string(), json!(run_id));
                payload.insert("run_attempt".to_string(), json!(ci.run_attempt()));
            }
            if let Some(check_run_id) = &ci.check_run_id {
                payload.insert("check_run_id".to_string(), json!(check_run_id));
            }
            payload.insert("run_job_count".to_string(), json!(ci.run_job_count));
            payload.insert("run_all_terminal".to_string(), json!(ci.run_all_terminal));
        }
        events.push(event);
    }

    events.sort_by(|left, right| {
        left.payload["workflow"]
            .as_str()
            .cmp(&right.payload["workflow"].as_str())
            .then_with(|| {
                left.payload["number"]
                    .as_u64()
                    .cmp(&right.payload["number"].as_u64())
            })
    });
    events
}

async fn fetch_issues(
    client: &GitHubClient,
    api_base: &str,
    repo: &GitRepoMonitor,
    snapshot: &GitSnapshot,
) -> Result<HashMap<u64, IssueSnapshot>> {
    let github_repo = snapshot
        .github_repo
        .clone()
        .ok_or_else(|| format!("no GitHub repo configured or inferred for {}", repo.path))?;
    let response = github_get(
        client,
        api_base,
        &format!("repos/{github_repo}/issues"),
        &[("state", "all"), ("per_page", "100")],
        &format!("issues for {github_repo}"),
    )
    .await?;
    let issues: Vec<GitHubIssue> = response.json().await?;
    Ok(issues
        .into_iter()
        .filter(|issue| !issue.is_pull_request())
        .map(|issue| {
            (
                issue.number,
                IssueSnapshot {
                    title: issue.title,
                    state: issue.state,
                    comments: issue.comments,
                },
            )
        })
        .collect())
}

async fn fetch_pull_requests(
    client: &GitHubClient,
    api_base: &str,
    repo: &GitRepoMonitor,
    snapshot: &GitSnapshot,
) -> Result<(HashMap<u64, PullRequestSnapshot>, bool)> {
    let github_repo = snapshot
        .github_repo
        .clone()
        .ok_or_else(|| format!("no GitHub repo configured or inferred for {}", repo.path))?;
    let mut pulls = Vec::new();
    let mut page = 1_usize;
    let mut complete = true;
    let mut seen_next_urls = HashSet::new();
    loop {
        let page_str = page.to_string();
        let current_page = format!(
            "{}/repos/{github_repo}/pulls?page={page_str}",
            api_base.trim_end_matches('/')
        );
        seen_next_urls.insert(pagination_page_key(&current_page));
        let response = github_get(
            client,
            api_base,
            &format!("repos/{github_repo}/pulls"),
            &[
                ("state", "all"),
                ("per_page", "100"),
                ("page", page_str.as_str()),
            ],
            &format!("pull requests for {github_repo}"),
        )
        .await?;
        let next_url = github_link_next(response.headers());
        let page_pulls: Vec<GitHubPullRequest> = response.json().await?;
        pulls.extend(page_pulls);
        let Some(next_url) = next_url else {
            break;
        };
        if page >= MAX_CI_PAGES || !seen_next_urls.insert(pagination_page_key(&next_url)) {
            complete = false;
            break;
        }
        page += 1;
    }
    Ok((
        pulls.into_iter().map(pull_request_snapshot).collect(),
        complete,
    ))
}

fn pull_request_snapshot(pull: GitHubPullRequest) -> (u64, PullRequestSnapshot) {
    let status = if pull.merged_at.is_some() {
        "merged".to_string()
    } else {
        pull.state
    };
    (
        pull.number,
        PullRequestSnapshot {
            updated_at: pull.updated_at,
            author: pull.user.map(|user| user.login).unwrap_or_default(),
            title: pull.title,
            status,
            url: pull.html_url,
            head_branch: pull.head.reference,
            head_sha: pull.head.sha,
        },
    )
}

/// Separate updated-desc window: do not change the PR-status poller's membership/order.
async fn fetch_updated_pull_requests(
    client: &GitHubClient,
    api_base: &str,
    repo: &GitRepoMonitor,
    snapshot: &GitSnapshot,
    previous: Option<&PRActivityState>,
) -> Result<(HashMap<u64, PullRequestSnapshot>, bool, Option<String>)> {
    let github_repo = snapshot
        .github_repo
        .as_deref()
        .ok_or_else(|| format!("no GitHub repo configured or inferred for {}", repo.path))?;
    let cursor = previous.and_then(|state| state.review_watermark.as_deref());
    let mut prs = HashMap::new();
    let mut baseline_cutoff = None;
    let mut seen_next_urls = HashSet::new();
    for page in 1..=MAX_CI_PAGES {
        let page_str = page.to_string();
        seen_next_urls.insert(pagination_page_key(&format!(
            "{}/repos/{github_repo}/pulls?page={page_str}",
            api_base.trim_end_matches('/')
        )));
        let response = github_get(
            client,
            api_base,
            &format!("repos/{github_repo}/pulls"),
            &[
                ("state", "all"),
                ("sort", "updated"),
                ("direction", "desc"),
                ("per_page", "100"),
                ("page", page_str.as_str()),
            ],
            &format!("updated pull requests for {github_repo}"),
        )
        .await?;
        let next_url = github_link_next(response.headers());
        if cursor.is_none() {
            baseline_cutoff = github_response_date(response.headers());
        }
        let pulls: Vec<GitHubPullRequest> = response.json().await?;
        if pulls.iter().any(|pull| pull.updated_at.is_empty()) {
            return Err("GitHub updated PR response is missing updated_at".into());
        }
        // Always recheck the boundary second; a PR may receive another review
        // without its second-resolution updated_at changing.
        let reached_cursor =
            cursor.is_some_and(|cursor| pulls.iter().any(|pull| pull.updated_at.as_str() < cursor));
        prs.extend(pulls.into_iter().map(pull_request_snapshot));
        // Cold start never traverses historical pages.
        if cursor.is_none() {
            return Ok((prs, true, baseline_cutoff));
        }
        let Some(next_url) = next_url.filter(|_| !reached_cursor) else {
            return Ok((prs, true, baseline_cutoff));
        };
        if !seen_next_urls.insert(pagination_page_key(&next_url)) {
            break;
        }
    }
    emit_source_degraded(
        Some(&repo.path),
        "GitHub updated PR pagination cap reached: reviews on PRs older than the newest 500-PR window may be lost",
    );
    Ok((prs, false, baseline_cutoff))
}

async fn fetch_ci_statuses(
    client: &GitHubClient,
    api_base: &str,
    repo: &GitRepoMonitor,
    snapshot: &GitSnapshot,
    open_prs: &[(u64, &PullRequestSnapshot)],
) -> Result<(HashMap<String, GitHubCISnapshot>, bool)> {
    let github_repo = snapshot
        .github_repo
        .clone()
        .ok_or_else(|| format!("no GitHub repo configured or inferred for {}", repo.path))?;
    let mut check_runs = HashMap::new();
    let mut seen_run_ids = HashSet::new();
    let mut window_complete = true;

    for (number, pr) in open_prs {
        let (fetched, complete) =
            fetch_check_runs(client, api_base, &github_repo, *number, pr).await?;
        window_complete &= complete;
        for check_run in fetched {
            if let Some(run_id) = &check_run.run_id {
                seen_run_ids.insert(run_id.clone());
            }
            check_runs.insert(check_run.dedupe_key(), check_run);
        }
    }

    let mut attempts_by_run = HashMap::new();
    for (_, pr) in open_prs {
        let (pr_attempts, attempts_complete) =
            fetch_workflow_attempts_by_head_sha(client, api_base, &github_repo, &pr.head_sha)
                .await?;
        window_complete &= attempts_complete;
        for (run_id, attempt) in pr_attempts {
            attempts_by_run
                .entry(run_id)
                .and_modify(|existing: &mut u32| *existing = (*existing).max(attempt))
                .or_insert(attempt);
        }
    }
    let (workflow_runs, complete, direct_attempts) =
        fetch_direct_workflow_runs(client, api_base, &github_repo, snapshot).await?;
    window_complete &= complete;
    for (run_id, attempt) in direct_attempts {
        attempts_by_run
            .entry(run_id)
            .and_modify(|existing: &mut u32| *existing = (*existing).max(attempt))
            .or_insert(attempt);
    }
    apply_workflow_run_attempts(&mut check_runs, &attempts_by_run);
    for workflow_run in workflow_runs {
        if workflow_run
            .run_id
            .as_ref()
            .is_some_and(|run_id| seen_run_ids.contains(run_id))
        {
            continue;
        }
        check_runs.insert(workflow_run.dedupe_key(), workflow_run);
    }

    Ok((check_runs, window_complete))
}

fn github_link_next(headers: &HeaderMap) -> Option<String> {
    let header = headers.get(LINK)?.to_str().ok()?;
    for part in header.split(',') {
        let mut href = None;
        let mut is_next = false;
        for item in part.split(';') {
            let item = item.trim();
            if let Some(url) = item
                .strip_prefix('<')
                .and_then(|value| value.strip_suffix('>'))
            {
                href = Some(url.to_string());
            } else if item.contains("rel=") && item.contains("next") {
                is_next = true;
            }
        }
        if is_next {
            return href;
        }
    }
    None
}

fn canonical_next_url(url: &str) -> String {
    let Ok(mut parsed) = reqwest::Url::parse(url.trim()) else {
        return url.trim().trim_end_matches('/').to_string();
    };
    parsed.set_fragment(None);
    let scheme = parsed.scheme().to_ascii_lowercase();
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let authority_host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    let port = parsed
        .port()
        .filter(|port| Some(*port) != parsed.port_or_known_default());
    let authority = port
        .map(|port| format!("{authority_host}:{port}"))
        .unwrap_or(authority_host);
    let path = {
        let path = parsed.path().trim_end_matches('/');
        if path.is_empty() { "/" } else { path }
    };
    let mut pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    pairs.sort();
    let query = pairs
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    if query.is_empty() {
        format!("{scheme}://{authority}{path}")
    } else {
        format!("{scheme}://{authority}{path}?{query}")
    }
}

fn pagination_page_key(url: &str) -> String {
    let Ok(parsed) = reqwest::Url::parse(url.trim()) else {
        return canonical_next_url(url);
    };
    let page = parsed
        .query_pairs()
        .find(|(key, _)| key == "page")
        .map(|(_, value)| value.into_owned());
    let Some(page) = page else {
        return canonical_next_url(url);
    };
    let mut key = parsed;
    key.set_query(Some(&format!("page={page}")));
    canonical_next_url(key.as_str())
}

async fn fetch_check_runs(
    client: &GitHubClient,
    api_base: &str,
    github_repo: &str,
    pr_number: u64,
    pr: &PullRequestSnapshot,
) -> Result<(Vec<GitHubCISnapshot>, bool)> {
    let mut all_runs = Vec::new();
    let mut page = 1_usize;
    let mut window_complete = true;
    let mut seen_next_urls = HashSet::new();
    loop {
        let page_str = page.to_string();
        let current_page = format!(
            "{}/repos/{github_repo}/commits/{}/check-runs?page={page_str}",
            api_base.trim_end_matches('/'),
            pr.head_sha
        );
        seen_next_urls.insert(pagination_page_key(&current_page));
        let response = github_get(
            client,
            api_base,
            &format!("repos/{github_repo}/commits/{}/check-runs", pr.head_sha),
            &[("per_page", "100"), ("page", page_str.as_str())],
            &format!("check runs for {github_repo} PR #{pr_number}"),
        )
        .await?;
        let next_url = github_link_next(response.headers());
        let runs: GitHubCheckRunsResponse = response.json().await?;
        all_runs.extend(runs.check_runs);
        let Some(next_url) = next_url else {
            break;
        };
        if page >= MAX_CI_PAGES || !seen_next_urls.insert(pagination_page_key(&next_url)) {
            window_complete = false;
            break;
        }
        page += 1;
    }

    let run_summaries = summarize_workflow_runs(&all_runs);
    Ok((
        all_runs
            .into_iter()
            .map(|check_run| {
                let url = check_run
                    .details_url
                    .clone()
                    .unwrap_or_else(|| pr.url.clone());
                let run_id = workflow_run_id(&url);
                let (run_job_count, summarized_terminal) = run_id
                    .as_deref()
                    .and_then(|id| run_summaries.get(id).copied())
                    .unwrap_or((1, check_run.status == "completed"));
                let run_all_terminal = summarized_terminal && window_complete;
                let run_attempt = run_attempt_from_url(&url);
                GitHubCISnapshot {
                    pr_number: Some(pr_number),
                    workflow: check_run.name,
                    status: check_run.status,
                    conclusion: check_run.conclusion,
                    sha: check_run.head_sha,
                    url,
                    branch: Some(pr.head_branch.clone()),
                    run_id,
                    run_attempt,
                    check_run_id: (check_run.id != 0).then(|| check_run.id.to_string()),
                    created_at: check_run.started_at.or(check_run.completed_at),
                    run_job_count,
                    run_all_terminal,
                }
            })
            .collect(),
        window_complete,
    ))
}

fn summarize_workflow_runs(check_runs: &[GitHubCheckRun]) -> HashMap<String, (usize, bool)> {
    let mut summaries = HashMap::new();
    for check_run in check_runs {
        let Some(run_id) = check_run.details_url.as_deref().and_then(workflow_run_id) else {
            continue;
        };
        let entry = summaries.entry(run_id).or_insert((0, true));
        entry.0 += 1;
        entry.1 &= check_run.status == "completed";
    }
    summaries
}

async fn fetch_direct_workflow_runs(
    client: &GitHubClient,
    api_base: &str,
    github_repo: &str,
    snapshot: &GitSnapshot,
) -> Result<(Vec<GitHubCISnapshot>, bool, HashMap<String, u32>)> {
    let mut all_runs = Vec::new();
    let mut page = 1_usize;
    let mut window_complete = true;
    let mut seen_next_urls = HashSet::new();
    loop {
        let page_str = page.to_string();
        let current_page = format!(
            "{}/repos/{github_repo}/actions/runs?page={page_str}",
            api_base.trim_end_matches('/')
        );
        seen_next_urls.insert(pagination_page_key(&current_page));
        let mut query = vec![
            ("per_page", "100"),
            ("event", "push"),
            ("page", page_str.as_str()),
        ];
        if !snapshot.branch.is_empty() {
            query.push(("branch", snapshot.branch.as_str()));
        }

        let response = github_get(
            client,
            api_base,
            &format!("repos/{github_repo}/actions/runs"),
            &query,
            &format!("workflow runs for {github_repo}"),
        )
        .await?;
        let next_url = github_link_next(response.headers());
        let runs: GitHubWorkflowRunsResponse = response.json().await?;
        all_runs.extend(runs.workflow_runs);
        let Some(next_url) = next_url else {
            break;
        };
        if page >= MAX_CI_PAGES || !seen_next_urls.insert(pagination_page_key(&next_url)) {
            window_complete = false;
            break;
        }
        page += 1;
    }

    let mut attempts_by_run = HashMap::new();
    for run in &all_runs {
        let attempt = run_attempt_or_one(run.run_attempt);
        attempts_by_run
            .entry(run.id.to_string())
            .and_modify(|existing: &mut u32| *existing = (*existing).max(attempt))
            .or_insert(attempt);
    }
    Ok((
        all_runs
            .into_iter()
            .filter(|run| run.pull_requests.is_empty())
            .map(|run| {
                let run_all_terminal = run.status == "completed";
                GitHubCISnapshot {
                    pr_number: None,
                    workflow: run
                        .name
                        .unwrap_or_else(|| format!("workflow-run-{}", run.id)),
                    status: run.status,
                    conclusion: run.conclusion,
                    sha: run.head_sha,
                    url: run.html_url,
                    branch: non_empty_string(run.head_branch),
                    run_id: Some(run.id.to_string()),
                    run_attempt: run_attempt_or_one(run.run_attempt),
                    check_run_id: None,
                    created_at: run.created_at,
                    run_job_count: 1,
                    run_all_terminal,
                }
            })
            .collect(),
        window_complete,
        attempts_by_run,
    ))
}

async fn fetch_workflow_attempts_by_head_sha(
    client: &GitHubClient,
    api_base: &str,
    github_repo: &str,
    head_sha: &str,
) -> Result<(HashMap<String, u32>, bool)> {
    let mut attempts = HashMap::new();
    let mut page = 1_usize;
    let mut complete = true;
    let mut seen_next_urls = HashSet::new();
    loop {
        let page_str = page.to_string();
        let current_page = format!(
            "{}/repos/{github_repo}/actions/runs?page={page_str}",
            api_base.trim_end_matches('/')
        );
        seen_next_urls.insert(pagination_page_key(&current_page));
        let response = github_get(
            client,
            api_base,
            &format!("repos/{github_repo}/actions/runs"),
            &[
                ("per_page", "100"),
                ("head_sha", head_sha),
                ("page", page_str.as_str()),
            ],
            &format!("workflow run attempts for {github_repo} sha {head_sha}"),
        )
        .await?;
        let next_url = github_link_next(response.headers());
        let runs: GitHubWorkflowRunsResponse = response.json().await?;
        for run in runs.workflow_runs {
            let attempt = run_attempt_or_one(run.run_attempt);
            attempts
                .entry(run.id.to_string())
                .and_modify(|existing: &mut u32| *existing = (*existing).max(attempt))
                .or_insert(attempt);
        }
        let Some(next_url) = next_url else {
            break;
        };
        if page >= MAX_CI_PAGES || !seen_next_urls.insert(pagination_page_key(&next_url)) {
            complete = false;
            break;
        }
        page += 1;
    }
    Ok((attempts, complete))
}

fn workflow_run_id(url: &str) -> Option<String> {
    url.split("/actions/runs/")
        .nth(1)
        .and_then(|tail| tail.split('/').next())
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
}

fn run_attempt_from_url(url: &str) -> u32 {
    url.split("/attempts/")
        .nth(1)
        .and_then(|tail| tail.split('/').next())
        .and_then(|part| part.parse::<u32>().ok())
        .filter(|attempt| *attempt >= 1)
        .unwrap_or(1)
}

fn apply_workflow_run_attempts(
    snapshots: &mut HashMap<String, GitHubCISnapshot>,
    attempts: &HashMap<String, u32>,
) {
    if attempts.is_empty() {
        return;
    }
    let existing: Vec<GitHubCISnapshot> = snapshots.drain().map(|(_, snapshot)| snapshot).collect();
    for mut snapshot in existing {
        if let Some(run_id) = &snapshot.run_id
            && let Some(&attempt) = attempts.get(run_id)
        {
            snapshot.run_attempt = snapshot.run_attempt.max(attempt);
        }
        snapshots.insert(snapshot.dedupe_key(), snapshot);
    }
}

pub(crate) fn build_github_client(token: Option<String>) -> Result<GitHubClient> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static("clawhip/0.1"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    if let Some(token) = token {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
    }
    Ok(GitHubClient {
        http: reqwest::Client::builder()
            .default_headers(headers)
            .build()?,
        backoff: Mutex::new(GitHubRequestBackoff::default()),
    })
}

#[derive(Deserialize)]
struct GitHubIssue {
    number: u64,
    title: String,
    state: String,
    comments: u64,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

impl GitHubIssue {
    fn is_pull_request(&self) -> bool {
        self.pull_request.is_some()
    }
}

#[derive(Deserialize)]
struct GitHubUser {
    login: String,
}

#[derive(Deserialize)]
struct GitHubReviewComment {
    id: u64,
    pull_request_url: String,
    html_url: String,
    created_at: String,
    updated_at: String,
    path: String,
    user: Option<GitHubUser>,
}

#[derive(Deserialize)]
struct GitHubReview {
    id: u64,
    html_url: String,
    submitted_at: Option<String>,
    state: String,
    user: Option<GitHubUser>,
}

#[derive(Deserialize)]
struct GitHubComment {
    #[serde(default)]
    user: Option<GitHubUser>,
    id: u64,
    issue_url: String,
    html_url: String,
    created_at: String,
    updated_at: String,
}

impl GitHubComment {
    /// Inspect the type segment AFTER owner/repo, never a host or repository name.
    /// GitHub Enterprise uses the same web path and may prefix API paths with /api/v3.
    fn pr_number(&self) -> Option<u64> {
        if let Ok(url) = reqwest::Url::parse(&self.html_url)
            && let Some(segments) = url.path_segments()
        {
            let mut segments = segments.skip(2);
            if segments.next() == Some("pull") {
                return segments.next()?.parse().ok();
            }
        }
        let url = reqwest::Url::parse(&self.issue_url).ok()?;
        let path = url.path().strip_prefix("/api/v3").unwrap_or(url.path());
        let mut segments = path.strip_prefix("/repos/")?.split('/').skip(2);
        if segments.next() == Some("pulls") {
            segments.next()?.parse().ok()
        } else {
            None
        }
    }

    /// Extract issue number from issue_url or html_url.
    fn issue_number(&self) -> Option<u64> {
        let url = reqwest::Url::parse(&self.issue_url).ok()?;
        let path = url.path().strip_prefix("/api/v3").unwrap_or(url.path());
        let mut segments = path.strip_prefix("/repos/")?.split('/').skip(2);
        if segments.next() == Some("issues") {
            segments.next()?.parse().ok()
        } else {
            None
        }
    }
}

#[derive(Deserialize)]
struct GitHubPullRequest {
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    user: Option<GitHubUser>,
    number: u64,
    title: String,
    state: String,
    html_url: String,
    merged_at: Option<String>,
    head: GitHubPullRequestHead,
}

#[derive(Deserialize)]
struct GitHubPullRequestHead {
    #[serde(rename = "ref")]
    reference: String,
    sha: String,
}

#[derive(Deserialize)]
struct GitHubCheckRunsResponse {
    check_runs: Vec<GitHubCheckRun>,
}

#[derive(Deserialize)]
struct GitHubCheckRun {
    #[serde(default)]
    id: u64,
    name: String,
    status: String,
    conclusion: Option<String>,
    details_url: Option<String>,
    head_sha: String,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    completed_at: Option<String>,
}

#[derive(Deserialize)]
struct GitHubWorkflowRunsResponse {
    workflow_runs: Vec<GitHubWorkflowRun>,
}

#[derive(Deserialize)]
struct GitHubWorkflowRun {
    id: u64,
    #[serde(default)]
    name: Option<String>,
    status: String,
    conclusion: Option<String>,
    head_branch: String,
    head_sha: String,
    html_url: String,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default = "default_run_attempt")]
    run_attempt: u32,
    #[serde(default)]
    pull_requests: Vec<serde_json::Value>,
}

fn default_run_attempt() -> u32 {
    1
}

fn non_empty_string(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

fn classify_ci_event_kind(status: &str, conclusion: Option<&str>) -> &'static str {
    if status != "completed" {
        return "github.ci-started";
    }

    match conclusion {
        Some("success" | "neutral" | "skipped") => "github.ci-passed",
        Some("cancelled") => "github.ci-cancelled",
        _ => "github.ci-failed",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn explicit_auth_precedence_is_environment_then_config() {
        assert_eq!(
            explicit_monitor_github_token_from(
                Some("environment-secret".into()),
                Some("config-secret".into())
            ),
            Some(("environment-secret".into(), "environment"))
        );
        assert_eq!(
            explicit_monitor_github_token_from(None, Some("config-secret".into())),
            Some(("config-secret".into(), "config"))
        );
        assert_eq!(explicit_monitor_github_token_from(None, None), None);
    }

    #[test]
    fn github_api_errors_are_redacted() {
        let error = github_api_error(reqwest::StatusCode::FORBIDDEN);
        assert_eq!(error, "GitHub API request failed with 403 Forbidden");
        assert!(!error.contains("secret"));
        assert!(!error.contains("token"));
    }

    #[test]
    fn missing_auth_status_is_actionable_without_secret_material() {
        let status = GitHubMonitorAuthStatus {
            ready: false,
            source: "unavailable",
            error: Some(
                "No GitHub monitor credential available; set CLAWHIP_GITHUB_TOKEN, configure monitors.github_token, or run gh auth login",
            ),
        };
        let rendered = serde_json::to_string(&status).unwrap();
        assert!(rendered.contains("gh auth login"));
        assert!(!rendered.contains("secret"));
    }

    #[tokio::test]
    async fn repeated_rate_limit_requests_are_backed_off() {
        let client = build_github_client(None).unwrap();
        client.backoff.lock().unwrap().until =
            Some(std::time::Instant::now() + Duration::from_secs(60));
        let error = github_get(&client, "http://127.0.0.1:1", "rate-limit", &[], "test")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("rate limit backoff is active"));
    }

    #[tokio::test]
    async fn successful_request_on_another_client_does_not_clear_rate_limit_backoff() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::sync::oneshot;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_started_tx, request_started_rx) = oneshot::channel();
        let (send_response_tx, send_response_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let _request_len = stream.read(&mut request).await.unwrap();
            request_started_tx.send(()).unwrap();
            send_response_rx.await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n[]",
                )
                .await
                .unwrap();
        });

        let successful_client = build_github_client(None).unwrap();
        let successful_request = tokio::spawn(async move {
            github_get(
                &successful_client,
                &format!("http://{addr}"),
                "success",
                &[],
                "contaminating success",
            )
            .await
        });
        request_started_rx.await.unwrap();

        let backed_off_client = build_github_client(None).unwrap();
        backed_off_client.backoff.lock().unwrap().until =
            Some(std::time::Instant::now() + Duration::from_secs(60));
        send_response_tx.send(()).unwrap();
        successful_request.await.unwrap().unwrap();
        server.await.unwrap();

        let error = github_get(
            &backed_off_client,
            "http://127.0.0.1:1",
            "rate-limit",
            &[],
            "test",
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("rate limit backoff is active"));
    }
    use crate::config::{DefaultsConfig, RouteRule};
    use crate::events::MessageFormat;
    use crate::router::Router;
    use serde_json::json;

    #[tokio::test]
    async fn new_issue_events_apply_route_channel_and_mention_over_repo_monitor_channel() {
        let repo = GitRepoMonitor {
            path: "/tmp/clawhip".into(),
            name: Some("clawhip".into()),
            channel: Some("dev-channel".into()),
            ..GitRepoMonitor::default()
        };
        let previous = HashMap::new();
        let current = [(
            2_u64,
            IssueSnapshot {
                title: "live issue".into(),
                state: "open".into(),
                comments: 0,
            },
        )]
        .into_iter()
        .collect();
        let events = collect_issue_events(&repo, "clawhip", &previous, &current);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.issue-opened");
        assert_eq!(events[0].payload["repo"], "clawhip");

        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("fallback".into()),
                channel_name: None,
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "github.*".into(),
                sink: "discord".into(),
                filter: [("repo".to_string(), "clawhip".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("route-channel".into()),
                thread: None,
                channel_name: None,
                webhook: None,
                slack_webhook: None,
                local_path: None,
                mention: Some("<@1465264645320474637>".into()),
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Alert),
                template: None,
                gajae: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let (channel, _, content) = router.preview(&events[0]).await.unwrap();
        assert_eq!(channel, "route-channel");
        assert!(content.starts_with("<@1465264645320474637> "));
        assert!(content.contains("live issue"));
    }

    #[test]
    fn issue_comment_and_close_events_are_emitted() {
        let repo = GitRepoMonitor {
            path: "/tmp/clawhip".into(),
            name: Some("clawhip".into()),
            ..GitRepoMonitor::default()
        };
        let previous = [(
            2_u64,
            IssueSnapshot {
                title: "live issue".into(),
                state: "open".into(),
                comments: 0,
            },
        )]
        .into_iter()
        .collect();
        let current = [(
            2_u64,
            IssueSnapshot {
                title: "live issue".into(),
                state: "closed".into(),
                comments: 1,
            },
        )]
        .into_iter()
        .collect();
        let events = collect_issue_events(&repo, "clawhip", &previous, &current);
        assert!(
            events
                .iter()
                .any(|event| event.canonical_kind() == "github.issue-commented")
        );
        assert!(
            events
                .iter()
                .any(|event| event.canonical_kind() == "github.issue-closed")
        );
    }

    fn ci_snapshot(
        pr_number: u64,
        workflow: &str,
        status: &str,
        conclusion: Option<&str>,
    ) -> GitHubCISnapshot {
        GitHubCISnapshot {
            pr_number: Some(pr_number),
            workflow: workflow.into(),
            status: status.into(),
            conclusion: conclusion.map(ToString::to_string),
            sha: "abcdef1234567890".into(),
            url: "https://github.com/Yeachan-Heo/clawhip/actions/runs/1".into(),
            branch: Some("feat/github-ci-events".into()),
            run_id: Some("1".into()),
            run_attempt: 1,
            check_run_id: None,
            created_at: None,
            run_job_count: 1,
            run_all_terminal: status == "completed",
        }
    }

    fn run_snapshot(
        run_id: u64,
        workflow: &str,
        conclusion: Option<&str>,
        branch: &str,
    ) -> GitHubCISnapshot {
        GitHubCISnapshot {
            pr_number: None,
            workflow: workflow.into(),
            status: "completed".into(),
            conclusion: conclusion.map(ToString::to_string),
            sha: format!("sha-{run_id}"),
            url: format!("https://github.com/org/repo/actions/runs/{run_id}"),
            branch: Some(branch.into()),
            run_id: Some(run_id.to_string()),
            run_attempt: 1,
            check_run_id: None,
            created_at: None,
            run_job_count: 1,
            run_all_terminal: true,
        }
    }

    fn run_map(runs: &[GitHubCISnapshot]) -> HashMap<String, GitHubCISnapshot> {
        runs.iter()
            .map(|run| (run.dedupe_key(), run.clone()))
            .collect()
    }

    /// Simulates one monitor poll: fetch -> diff against in-process previous
    /// state -> record terminal identities into the shared baseline.
    async fn poll_once_with_baseline(
        config: &AppConfig,
        client: &GitHubClient,
        state: &mut HashMap<String, GitHubRepoState>,
        ci_baseline: &mut CIBaseline,
        ci_baseline_path: Option<&Path>,
        tx: &mpsc::Sender<IncomingEvent>,
    ) -> Result<()> {
        poll_github(
            config,
            Some(client),
            tx,
            state,
            ci_baseline,
            ci_baseline_path,
        )
        .await
    }

    #[test]
    fn issue_317_bounded_baseline_suppresses_historical_terminal_runs_after_monitor_restart() {
        // Deterministic replay of the #317 burst: historical success/failure/
        // cancelled runs already observed by a previous daemon process.
        let historical_runs = [
            run_snapshot(27620587146, "CI", Some("success"), "main"),
            run_snapshot(24244183277, "CI", Some("failure"), "main"),
            run_snapshot(29691404856, "CI", Some("cancelled"), "main"),
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");

        // --- First daemon process: observe and persist the baseline.
        let mut ci_baseline = load_ci_baseline(Some(&path));
        assert!(ci_baseline.record_terminal_runs("/repo", "/repo", &run_map(&historical_runs)));
        save_ci_baseline(&ci_baseline, Some(&path)).unwrap();

        // --- Daemon restart: state HashMap is empty, baseline reloads from disk.
        let restarted = load_ci_baseline(Some(&path));
        let repo_baseline = restarted.repos.get("/repo").expect("persisted repo");

        // Bounded: only terminal identities, at most MAX_BASELINE_RUNS_PER_REPO.
        assert_eq!(repo_baseline.terminal_runs.len(), 3);
        for run in &historical_runs {
            assert!(
                repo_ci_baseline_is_suppressed(restarted.repos.get("/repo"), run),
                "historical terminal run {} must be suppressed after restart",
                run.run_id.as_deref().unwrap_or("?")
            );
        }

        // --- Re-observation with an empty in-process previous (pagination
        // drift / cursor loss / re-enrollment) emits nothing.
        let repo = GitRepoMonitor::default();
        let events = collect_ci_events(
            &repo,
            "org/repo",
            true, // established monitor — old code emitted here (#317)
            &HashMap::new(),
            &run_map(&historical_runs),
            restarted.repos.get("/repo"),
        );
        assert!(
            events.is_empty(),
            "historical terminal runs must not replay after restart; got {} events",
            events.len()
        );

        // --- Genuinely new terminal runs are still delivered.
        let new_run = run_snapshot(32205447861, "CI", Some("success"), "main");
        let events = collect_ci_events(
            &repo,
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&new_run)),
            restarted.repos.get("/repo"),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.ci-passed");
    }

    #[test]
    fn issue_317_baseline_is_bounded_and_atomic_persistence_is_fail_safe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");

        // Bounded growth: recording far more terminal runs than the cap keeps
        // at most MAX_BASELINE_RUNS_PER_REPO identities (newest retained).
        let mut ci_baseline = CIBaseline::default();
        for id in 0..(MAX_BASELINE_RUNS_PER_REPO + 64) {
            let runs = [run_snapshot(id as u64, "CI", Some("success"), "main")];
            ci_baseline.record_terminal_runs("/repo", "/repo", &run_map(&runs));
        }
        let repo_baseline = ci_baseline.repos.get("/repo").unwrap();
        assert_eq!(
            repo_baseline.terminal_runs.len(),
            MAX_BASELINE_RUNS_PER_REPO
        );
        assert!(
            repo_baseline.contains_identity(&format!("run:{}", MAX_BASELINE_RUNS_PER_REPO + 63))
        );
        assert!(!repo_baseline.contains_identity("run:0"));

        // Atomic persistence round-trips through a temp file + rename.
        save_ci_baseline(&ci_baseline, Some(&path)).unwrap();
        let reloaded = load_ci_baseline(Some(&path));
        assert_eq!(
            reloaded.repos.get("/repo").unwrap().terminal_runs.len(),
            MAX_BASELINE_RUNS_PER_REPO
        );
        assert!(!dir.path().join("github-ci-baseline.json.tmp").exists());

        // Corrupt persisted state is a fail-safe fresh baseline, not a crash
        // and not a silent skip: replay is still suppressed by first-poll
        // baseline priming, and no unwinding panic escapes the source.
        std::fs::write(&path, "{ not valid json").unwrap();
        let corrupted = load_ci_baseline(Some(&path));
        assert!(corrupted.repos.is_empty());

        // Cross-repo identity: another repo's baseline never suppresses this
        // repo's runs.
        let mut mixed = CIBaseline::default();
        let other = [run_snapshot(999, "CI", Some("success"), "main")];
        mixed.record_terminal_runs("/other-repo", "/other-repo", &run_map(&other));
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(&other),
            mixed.repos.get("/repo"),
        );
        assert_eq!(
            events.len(),
            1,
            "a different repo's baseline must not suppress this repo's runs"
        );
    }

    #[test]
    fn issue_317_in_progress_runs_and_changed_conclusions_still_emit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut ci_baseline = load_ci_baseline(Some(&path));

        // A run observed in-progress is not recorded as terminal...
        let in_progress = GitHubCISnapshot {
            status: "in_progress".into(),
            conclusion: None,
            run_all_terminal: false,
            ..run_snapshot(4242424242, "CI", Some("success"), "main")
        };
        ci_baseline.record_terminal_runs(
            "/repo",
            "/repo",
            &run_map(std::slice::from_ref(&in_progress)),
        );
        assert!(ci_baseline.repos["/repo"].terminal_runs.is_empty());

        // ...so its genuinely new started event is emitted even though the
        // monitor is established.
        let repo = GitRepoMonitor::default();
        let events = collect_ci_events(
            &repo,
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&in_progress)),
            ci_baseline.repos.get("/repo"),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.ci-started");

        // Once it terminates, the completion transition emits (present in
        // previous) and is then recorded/suppressed forever after.
        let completed = run_snapshot(4242424242, "CI", Some("failure"), "main");
        let events = collect_ci_events(
            &repo,
            "org/repo",
            true,
            &run_map(std::slice::from_ref(&in_progress)),
            &run_map(std::slice::from_ref(&completed)),
            ci_baseline.repos.get("/repo"),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.ci-failed");

        ci_baseline.record_terminal_runs(
            "/repo",
            "/repo",
            &run_map(std::slice::from_ref(&completed)),
        );
        save_ci_baseline(&ci_baseline, Some(&path)).unwrap();
        let restarted = load_ci_baseline(Some(&path));
        let events = collect_ci_events(
            &repo,
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&completed)),
            restarted.repos.get("/repo"),
        );
        assert!(
            events.is_empty(),
            "terminal outcome delivered once must never replay"
        );
    }

    fn fallback_snapshot(pr_number: u64, sha: &str, workflow: &str) -> GitHubCISnapshot {
        GitHubCISnapshot {
            pr_number: Some(pr_number),
            workflow: workflow.into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
            sha: sha.into(),
            url: "https://github.com/org/repo/pull/58".into(),
            branch: Some("feat/x".into()),
            run_id: None,
            run_attempt: 1,
            check_run_id: None,
            created_at: None,
            run_job_count: 1,
            run_all_terminal: true,
        }
    }

    #[test]
    fn issue_317_representation_drift_fallback_then_run_id_is_suppressed() {
        let fallback = fallback_snapshot(58, "abcdef1234567890", "CI");
        let with_run_id = GitHubCISnapshot {
            run_id: Some("4242".into()),
            url: "https://github.com/org/repo/actions/runs/4242".into(),
            ..fallback.clone()
        };

        let mut ci_baseline = CIBaseline::default();
        assert!(ci_baseline.record_terminal_runs("/repo", "/repo", &run_map(&[fallback])));

        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&with_run_id)),
            ci_baseline.repos.get("/repo"),
        );
        assert!(
            !events.is_empty(),
            "ambiguous fallback-to-run-id drift must fail open rather than suppress"
        );

        assert!(ci_baseline.record_terminal_runs("/repo", "/repo", &run_map(&[with_run_id])));
        assert!(
            ci_baseline.repos["/repo"].contains_identity("run:4242"),
            "discovering a run id must merge onto the existing fallback record"
        );
    }

    #[test]
    fn issue_317_fallback_collisions_do_not_drop_distinct_reruns() {
        let first = GitHubCISnapshot {
            check_run_id: Some("111".into()),
            ..fallback_snapshot(58, "abcdef1234567890", "CI")
        };
        let second = GitHubCISnapshot {
            check_run_id: Some("222".into()),
            conclusion: Some("failure".into()),
            ..fallback_snapshot(58, "abcdef1234567890", "CI")
        };
        let current = run_map(&[first.clone(), second.clone()]);
        assert_eq!(
            current.len(),
            2,
            "distinct check-run ids must not collapse in the poll map"
        );

        let mut ci_baseline = CIBaseline::default();
        ci_baseline.record_terminal_runs("/repo", "/repo", &current);
        assert_eq!(ci_baseline.repos["/repo"].terminal_runs.len(), 2);
        assert!(ci_baseline.repos["/repo"].contains_identity("check:111"));
        assert!(ci_baseline.repos["/repo"].contains_identity("check:222"));

        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &current,
            ci_baseline.repos.get("/repo"),
        );
        assert!(events.is_empty());
    }

    #[test]
    fn issue_317_partial_multi_job_workflow_is_not_terminal_until_all_jobs_complete() {
        let partial = GitHubCISnapshot {
            status: "completed".into(),
            conclusion: Some("success".into()),
            run_all_terminal: false,
            run_job_count: 2,
            ..run_snapshot(77, "CI", Some("success"), "main")
        };
        let complete = GitHubCISnapshot {
            run_all_terminal: true,
            run_job_count: 2,
            ..run_snapshot(77, "CI", Some("success"), "main")
        };

        let mut ci_baseline = CIBaseline::default();
        ci_baseline.record_terminal_runs(
            "/repo",
            "/repo",
            &run_map(std::slice::from_ref(&partial)),
        );
        assert!(
            ci_baseline
                .repos
                .get("/repo")
                .is_none_or(|repo| repo.terminal_runs.is_empty()),
            "a completed job in a still-running workflow must not be persisted"
        );

        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&complete)),
            ci_baseline.repos.get("/repo"),
        );
        assert_eq!(
            events.len(),
            1,
            "eventual multi-job completion after restart must still emit"
        );
        assert_eq!(events[0].canonical_kind(), "github.ci-passed");

        ci_baseline.record_terminal_runs(
            "/repo",
            "/repo",
            &run_map(std::slice::from_ref(&complete)),
        );
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(&[complete]),
            ci_baseline.repos.get("/repo"),
        );
        assert!(events.is_empty());
    }

    #[test]
    fn issue_317_persist_failure_does_not_mutate_in_memory_suppression() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"file").unwrap();
        let path = blocker.join("github-ci-baseline.json");

        let original = CIBaseline::default();
        let mut next = original.clone();
        let runs = run_map(&[run_snapshot(9, "CI", Some("success"), "main")]);
        assert!(next.record_terminal_runs("repo:org/repo", "/repo", &runs));
        assert!(commit_ci_baseline(Some(&path), &next).is_err());
        assert!(
            original.repos.is_empty(),
            "persist failure must leave the live baseline unmutated"
        );
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn issue_317_persist_before_publish_drops_events_when_receipt_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, b"file").unwrap();
        let baseline_path = blocker.join("github-ci-baseline.json");

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0_u8; 4096];
                let _ = stream.read(&mut buf).await;
                let body = r#"{"workflow_runs":[{"id": 9001, "name": "CI", "status": "completed", "conclusion": "success", "head_branch": "main", "head_sha": "abc", "html_url": "https://github.com/org/repo/actions/runs/9001", "pull_requests": [], "created_at": "2026-08-19T00:00:00Z"}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let mut config = AppConfig::default();
        config.monitors.github_api_base = format!("http://{addr}");
        config.monitors.git.repos = vec![GitRepoMonitor {
            path: "/tmp/persist-boundary".into(),
            name: Some("repo".into()),
            github_repo: Some("org/repo".into()),
            emit_pr_status: true,
            emit_issue_opened: false,
            emit_commits: false,
            emit_branch_changes: false,
            ..GitRepoMonitor::default()
        }];
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let mut state = HashMap::new();
        state.insert(
            "/tmp/persist-boundary".into(),
            GitHubRepoState {
                activity: PRActivityState::default(),
                issues: HashMap::new(),
                prs: HashMap::new(),
                ci: HashMap::new(),
                ci_baseline_established: true,
            },
        );
        let mut baseline = CIBaseline::default();
        poll_once_with_baseline(
            &config,
            &client,
            &mut state,
            &mut baseline,
            Some(&baseline_path),
            &tx,
        )
        .await
        .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "persist failure must not publish a terminal event"
        );
        assert!(!baseline_path.exists());
        server.abort();
    }

    #[test]
    fn issue_317_baseline_is_keyed_by_canonical_github_repo_not_checkout_path() {
        let run = run_snapshot(55, "CI", Some("success"), "main");
        let mut ci_baseline = CIBaseline::default();
        ci_baseline.record_terminal_runs(
            "repo:org/repo",
            "/old/path",
            &run_map(std::slice::from_ref(&run)),
        );

        let renamed = GitRepoMonitor {
            path: "/new/path".into(),
            github_repo: Some("org/repo".into()),
            ..GitRepoMonitor::default()
        };
        let snapshot = GitSnapshot {
            repo_name: "repo".into(),
            repo_path: "/new/path".into(),
            worktree_path: "/new/path".into(),
            branch: "main".into(),
            head: "sha-55".into(),
            commits: Vec::new(),
            github_repo: Some("org/repo".into()),
        };
        let key = ci_baseline_repo_key(&snapshot, &renamed);
        assert_eq!(key, "repo:org/repo");
        assert!(repo_ci_baseline_is_suppressed(
            ci_baseline.repos.get(&key),
            &run
        ));

        let other_remote = GitHubCISnapshot {
            sha: "other-sha".into(),
            ..run_snapshot(55, "CI", Some("success"), "main")
        };
        let other_key = "repo:other/remote";
        assert!(!repo_ci_baseline_is_suppressed(
            ci_baseline.repos.get(other_key),
            &other_remote
        ));
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "other/remote",
            true,
            &HashMap::new(),
            &run_map(&[other_remote]),
            ci_baseline.repos.get(other_key),
        );
        assert_eq!(events.len(), 1, "a different remote must not be suppressed");
    }

    #[test]
    fn issue_317_later_poll_evicts_oldest_unprotected_github_runs() {
        let mut first = Vec::new();
        for id in 0..MAX_BASELINE_RUNS_PER_REPO {
            let mut run = run_snapshot(id as u64, "CI", Some("success"), "main");
            run.created_at = Some(format!("2026-01-01T00:{:02}:{:02}Z", id / 60, id % 60));
            first.push(run);
        }
        let mut ci_baseline = CIBaseline::default();
        ci_baseline.record_terminal_runs("/repo", "/repo", &run_map(&first));
        let mut later = Vec::new();
        for id in MAX_BASELINE_RUNS_PER_REPO..(MAX_BASELINE_RUNS_PER_REPO + 20) {
            let mut run = run_snapshot(id as u64, "CI", Some("success"), "main");
            run.created_at = Some(format!("2026-01-01T01:{:02}:{:02}Z", id / 60, id % 60));
            later.push(run);
        }
        ci_baseline.record_terminal_runs("/repo", "/repo", &run_map(&later));
        let repo = &ci_baseline.repos["/repo"];
        assert_eq!(repo.terminal_runs.len(), MAX_BASELINE_RUNS_PER_REPO);
        assert!(!repo.contains_identity("run:0"));
        assert!(!repo.contains_identity("run:19"));
        assert!(repo.contains_identity("run:20"));
        assert!(repo.contains_identity(&format!("run:{}", MAX_BASELINE_RUNS_PER_REPO + 19)));
    }

    #[test]
    fn issue_317_current_window_over_cap_survives_reload() {
        let mut runs = Vec::new();
        for id in 0..(MAX_BASELINE_RUNS_PER_REPO + 20) {
            let mut run = run_snapshot(id as u64, "CI", Some("success"), "main");
            run.created_at = Some(format!("2026-01-01T00:{:02}:{:02}Z", id / 60, id % 60));
            runs.push(run);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut baseline = CIBaseline::default();
        baseline.record_terminal_runs("/repo", "/repo", &run_map(&runs));
        save_ci_baseline(&baseline, Some(&path)).unwrap();
        let loaded = load_ci_baseline(Some(&path));
        let repo = &loaded.repos["/repo"];
        assert_eq!(repo.terminal_runs.len(), MAX_BASELINE_RUNS_PER_REPO + 20);
        assert!(repo.contains_identity("run:0"));
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(&runs),
            loaded.repos.get("/repo"),
        );
        assert!(
            events.is_empty(),
            "restart against the same oversized window must not replay"
        );
    }

    #[test]
    fn issue_317_stale_writer_cannot_evict_oversized_current_window() {
        let mut current = Vec::new();
        for id in 0..(MAX_BASELINE_RUNS_PER_REPO + 20) as u64 {
            let mut run = run_snapshot(id, "CI", Some("success"), "main");
            run.created_at = Some(format!("2026-01-01T00:{:02}:{:02}Z", id / 60, id % 60));
            current.push(run);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut live = CIBaseline::default();
        live.record_terminal_runs("/repo", "/repo", &run_map(&current));
        live.repos.get_mut("/repo").unwrap().min_writer_epoch = 1;
        save_ci_baseline(&live, Some(&path)).unwrap();
        let committed_epoch = load_ci_baseline(Some(&path)).repos["/repo"].epoch;

        let mut stale = Vec::new();
        for id in 10_000..(10_000 + MAX_BASELINE_RUNS_PER_REPO as u64) {
            let mut run = run_snapshot(id, "CI", Some("success"), "main");
            run.created_at = Some("2025-01-01T00:00:00Z".into());
            stale.push(run);
        }
        let mut incoming = CIBaseline::default();
        incoming.record_terminal_runs("/repo", "/repo", &run_map(&stale));
        incoming.repos.get_mut("/repo").unwrap().epoch = 0;
        commit_ci_baseline(Some(&path), &incoming).unwrap();
        let loaded = load_ci_baseline(Some(&path));
        let repo = &loaded.repos["/repo"];
        assert!(repo.contains_identity("run:0"));
        assert!(
            repo.current_window_ids.contains(&"run:0".to_string()),
            "stale writer must not replace the authoritative current window"
        );
        assert!(
            !repo.current_window_ids.iter().any(|id| id == "run:10000"),
            "stale writer identities must not become window authority"
        );
        assert_eq!(repo.epoch, committed_epoch + 1);
        assert_eq!(
            collect_ci_events(
                &GitRepoMonitor::default(),
                "org/repo",
                true,
                &HashMap::new(),
                &run_map(&current),
                loaded.repos.get("/repo"),
            )
            .len(),
            0
        );
    }

    #[test]
    fn issue_317_current_window_ids_are_canonicalized_and_bounded() {
        // One run contributing multiple durable aliases (attempt-qualified
        // run id plus check-run id) and repeated snapshots must store each
        // identity once, and an oversized window must stop at the explicit
        // bound instead of growing unboundedly.
        let mut aliased = GitHubCISnapshot {
            check_run_id: Some("555".into()),
            run_attempt: 2,
            ..run_snapshot(77, "CI", Some("success"), "main")
        };
        aliased.created_at = Some("2026-01-01T00:00:00Z".into());
        let mut baseline = CIBaseline::default();
        let map = run_map(&[aliased.clone(), aliased.clone()]);
        baseline.record_terminal_runs("/repo", "/repo", &map);
        let repo = &baseline.repos["/repo"];
        assert_eq!(repo.current_window_ids, vec!["run:77:2", "check:555"]);

        let many = (0..(MAX_CURRENT_WINDOW_IDS_PER_SCOPE as u64 + 60))
            .map(|id| format!("run:{id}"))
            .collect::<Vec<_>>();
        let bounded = canonicalize_current_window_ids_from(&many);
        assert_eq!(bounded.len(), MAX_CURRENT_WINDOW_IDS_PER_SCOPE);
        assert!(
            bounded.iter().all(|id| id.starts_with("run:")),
            "fallback identities must never be pinned"
        );
        let pinned_newest = format!("run:{}", MAX_CURRENT_WINDOW_IDS_PER_SCOPE as u64 + 59);
        assert!(
            bounded.contains(&pinned_newest),
            "truncation must keep the newest window identities"
        );
    }

    #[test]
    fn issue_317_loaded_window_ids_are_rebounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut ids: Vec<String> = (0..(MAX_CURRENT_WINDOW_IDS_PER_SCOPE + 25) as u64)
            .map(|id| format!("run:{id}"))
            .collect();
        ids.push(ids[0].clone());
        let payload = serde_json::to_string(&HashMap::from([(
            "/repo",
            RepoCIBaseline {
                current_window_ids: ids,
                ..RepoCIBaseline::default()
            },
        )]))
        .unwrap();
        std::fs::write(&path, payload).unwrap();
        let loaded = load_ci_baseline(Some(&path));
        let repo = &loaded.repos["/repo"];
        assert_eq!(
            repo.current_window_ids.len(),
            MAX_CURRENT_WINDOW_IDS_PER_SCOPE,
            "load must re-bound an over-long pinned window"
        );
        let mut unique = std::collections::HashSet::new();
        assert!(
            repo.current_window_ids
                .iter()
                .all(|id| unique.insert(id.clone())),
            "duplicates must collapse on load"
        );
        assert_eq!(
            repo.current_window_ids.last().unwrap(),
            &format!("run:{}", MAX_CURRENT_WINDOW_IDS_PER_SCOPE as u64 + 24),
            "the load bound must keep the newest pinned identities"
        );
    }

    #[test]
    fn issue_317_large_sibling_scopes_keep_their_full_protected_union() {
        let old_global_cap = CI_PAGE_SIZE * MAX_CI_PAGES * (CI_PAGE_SIZE + 1) * 2;
        let scope_size = old_global_cap + 1;
        let mut repo = RepoCIBaseline::default();
        repo.current_window_ids_by_scope.insert(
            "branch-a".into(),
            (0..scope_size).map(|id| format!("run:a-{id}")).collect(),
        );
        repo.current_window_ids_by_scope.insert(
            "branch-b".into(),
            (0..scope_size).map(|id| format!("run:b-{id}")).collect(),
        );
        canonicalize_loaded_window(&mut repo);
        assert_eq!(repo.current_window_ids.len(), scope_size * 2);
        assert!(repo.current_window_ids.contains(&"run:a-0".to_string()));
        assert!(repo.current_window_ids.contains(&"run:b-0".to_string()));
    }

    #[test]
    fn issue_317_partial_restart_poll_preserves_authoritative_scope() {
        let mut baseline = CIBaseline::default();
        let first = run_snapshot(1, "CI", Some("success"), "main");
        let second = run_snapshot(2, "CI", Some("failure"), "main");
        baseline.record_terminal_runs("/repo", "/repo", &run_map(std::slice::from_ref(&first)));
        let generation = baseline.repos["/repo"].window_generation + 1;
        baseline.record_terminal_runs_filtered(
            "/repo",
            "/repo",
            "/repo",
            &run_map(&[second]),
            (generation, false),
            |_| true,
        );
        let ids = &baseline.repos["/repo"].current_window_ids_by_scope["/repo"];
        assert!(ids.contains(&"run:1".to_string()));
        assert!(!ids.contains(&"run:2".to_string()));
    }

    #[test]
    fn issue_317_partial_window_keeps_more_than_history_cap_of_new_terminals() {
        let mut baseline = CIBaseline::default();
        let runs: Vec<_> = (0..(MAX_BASELINE_RUNS_PER_REPO + 100) as u64)
            .map(|id| run_snapshot(id, "CI", Some("failure"), "main"))
            .collect();
        let current = run_map(&runs);
        baseline.record_terminal_runs_filtered(
            "/repo",
            "/repo",
            "/repo",
            &current,
            (10, false),
            |_| true,
        );
        let repo = &baseline.repos["/repo"];
        assert_eq!(repo.terminal_runs.len(), MAX_BASELINE_RUNS_PER_REPO + 100);
        assert!(repo.contains_identity(&format!("run:{}", MAX_BASELINE_RUNS_PER_REPO + 99)));
    }

    #[test]
    fn issue_317_out_of_order_window_response_cannot_overwrite_newer_generation() {
        let mut baseline = CIBaseline::default();
        let fresh = run_snapshot(20, "CI", Some("success"), "main");
        let stale = run_snapshot(10, "CI", Some("success"), "main");
        baseline.record_terminal_runs_filtered(
            "/repo",
            "/repo",
            "/repo",
            &run_map(&[fresh]),
            (20, true),
            |_| true,
        );
        baseline.record_terminal_runs_filtered(
            "/repo",
            "/repo",
            "/repo",
            &run_map(&[stale]),
            (10, true),
            |_| true,
        );
        let ids = &baseline.repos["/repo"].current_window_ids_by_scope["/repo"];
        assert!(ids.contains(&"run:20".to_string()));
        assert!(!ids.contains(&"run:10".to_string()));
    }

    #[test]
    fn issue_317_empty_scope_tombstone_blocks_stale_nonempty_resurrection() {
        let mut live = RepoCIBaseline::default();
        live.current_window_ids_by_scope
            .insert("/repo".into(), vec!["run:old".into()]);
        live.current_window_generations.insert("/repo".into(), 10);
        live.window_generation = 10;
        canonicalize_loaded_window(&mut live);

        let mut empty = RepoCIBaseline::default();
        empty
            .current_window_ids_by_scope
            .insert("/repo".into(), vec![]);
        empty.current_window_generations.insert("/repo".into(), 20);
        empty.window_generation = 20;
        merge_repo_baseline(&mut live, empty);
        assert!(live.current_window_ids_by_scope["/repo"].is_empty());

        let mut stale = RepoCIBaseline::default();
        stale
            .current_window_ids_by_scope
            .insert("/repo".into(), vec!["run:old".into()]);
        stale.current_window_generations.insert("/repo".into(), 10);
        stale.window_generation = 10;
        merge_repo_baseline(&mut live, stale);
        assert!(live.current_window_ids_by_scope["/repo"].is_empty());
    }

    #[test]
    fn issue_317_generation_reservation_is_unique_under_concurrency() {
        let threads: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| next_window_generation(0)))
            .collect();
        let mut generations: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        generations.sort_unstable();
        generations.dedup();
        assert_eq!(generations.len(), 8);
    }

    #[test]
    fn issue_317_same_attempt_conclusion_change_is_not_suppressed_after_merge() {
        let success = check_job(42, 7, "CI", "success");
        let mut failure = check_job(42, 7, "CI", "failure");
        failure.run_attempt = success.run_attempt;
        let success_record = TerminalRunRecord::from_snapshot(&success);
        let failure_record = TerminalRunRecord::from_snapshot(&failure);
        assert!(!success_record.matches_snapshot(&failure));
        assert!(!terminal_records_same_run(&success_record, &failure_record));
    }

    #[test]
    fn issue_317_fenced_writer_cannot_clear_current_window() {
        // min_writer_epoch fencing: an epoch-0 writer landing after ack
        // compaction raised the fence must not replace (or clear) the
        // pinned current window even though its terminal receipts merge.
        let mut live = RepoCIBaseline {
            epoch: 10,
            min_writer_epoch: 10,
            ..RepoCIBaseline::default()
        };
        live.current_window_ids = vec!["run:4242".into()];
        live.terminal_runs
            .push_back(TerminalRunRecord::from_snapshot(&run_snapshot(
                4242,
                "CI",
                Some("success"),
                "main",
            )));
        let mut fenced = RepoCIBaseline {
            epoch: 0,
            ..RepoCIBaseline::default()
        };
        fenced.current_window_ids = vec!["run:9".into()];
        fenced
            .terminal_runs
            .push_back(TerminalRunRecord::from_snapshot(&run_snapshot(
                9,
                "CI",
                Some("success"),
                "main",
            )));
        merge_repo_baseline(&mut live, fenced);
        assert_eq!(
            live.current_window_ids,
            vec!["run:4242".to_string()],
            "fenced writer must not become window authority"
        );
        assert!(live.contains_identity("run:4242"));
    }

    #[test]
    fn issue_317_mixed_legacy_eviction_keeps_timestamped_and_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut records = Vec::new();
        for id in 0..MAX_BASELINE_RUNS_PER_REPO {
            records.push(format!(r#"{{"identities":["run:{id}"],"run_id":"{id}"}}"#));
        }
        let payload = format!(r#"{{"/repo":{{"terminal_runs":[{}]}}}}"#, records.join(","));
        std::fs::write(&path, payload).unwrap();

        let mut incoming = CIBaseline::default();
        let mut fresh = run_snapshot(9_999, "CI", Some("success"), "main");
        fresh.created_at = Some("2026-08-19T12:00:00Z".into());
        incoming.record_terminal_runs("/repo", "/repo", &run_map(std::slice::from_ref(&fresh)));
        commit_ci_baseline(Some(&path), &incoming).unwrap();

        let loaded = load_ci_baseline(Some(&path));
        let repo = &loaded.repos["/repo"];
        assert_eq!(repo.terminal_runs.len(), MAX_BASELINE_RUNS_PER_REPO);
        assert!(
            repo.contains_identity("run:9999"),
            "timestamped receipt must outrank unknown-age legacy records"
        );
        assert!(!repo.contains_identity("run:0"));

        let restarted = load_ci_baseline(Some(&path));
        assert!(restarted.repos["/repo"].contains_identity("run:9999"));
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&fresh)),
            restarted.repos.get("/repo"),
        );
        assert!(
            events.is_empty(),
            "restart must still suppress the retained timestamped receipt"
        );
    }

    #[tokio::test]
    async fn issue_317_pr_check_rerun_attempt_is_distinct_on_production_path() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut check_polls = 0_u32;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0_u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let body = if request.contains("/pulls") {
                    r#"[{"number":42,"title":"PR","state":"open","html_url":"https://github.com/org/repo/pull/42","head":{"ref":"feat/pr","sha":"prsha"}}]"#.to_string()
                } else if request.contains("/check-runs") {
                    check_polls += 1;
                    if check_polls <= 2 {
                        r#"{"check_runs":[{"id":11,"name":"CI","status":"completed","conclusion":"success","details_url":"https://github.com/org/repo/actions/runs/4242/jobs/11","head_sha":"prsha"}]}"#.to_string()
                    } else {
                        r#"{"check_runs":[{"id":22,"name":"CI","status":"completed","conclusion":"failure","details_url":"https://github.com/org/repo/actions/runs/4242/jobs/22","head_sha":"prsha"}]}"#.to_string()
                    }
                } else if request.contains("/actions/runs") {
                    if request.contains("head_sha=prsha") {
                        if check_polls <= 2 {
                            r#"{"workflow_runs":[{"id":4242,"name":"CI","status":"completed","conclusion":"success","head_branch":"feat/pr","head_sha":"prsha","html_url":"https://github.com/org/repo/actions/runs/4242","run_attempt":1,"pull_requests":[{"number":42}]}]}"#.to_string()
                        } else {
                            r#"{"workflow_runs":[{"id":4242,"name":"CI","status":"completed","conclusion":"failure","head_branch":"feat/pr","head_sha":"prsha","html_url":"https://github.com/org/repo/actions/runs/4242","run_attempt":2,"pull_requests":[{"number":42}]}]}"#.to_string()
                        }
                    } else {
                        r#"{"workflow_runs":[]}"#.to_string()
                    }
                } else {
                    "[]".to_string()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let baseline_path = dir.path().join("github-ci-baseline.json");
        let mut config = AppConfig::default();
        config.monitors.github_api_base = format!("http://{addr}");
        config.monitors.git.repos = vec![GitRepoMonitor {
            path: "/tmp/pr-rerun".into(),
            name: Some("repo".into()),
            github_repo: Some("org/repo".into()),
            emit_pr_status: true,
            emit_issue_opened: false,
            emit_commits: false,
            emit_branch_changes: false,
            ..GitRepoMonitor::default()
        }];
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = HashMap::new();
        let mut baseline = CIBaseline::default();
        poll_once_with_baseline(
            &config,
            &client,
            &mut state,
            &mut baseline,
            Some(&baseline_path),
            &tx,
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err(), "first poll primes attempt 1");

        let mut state = HashMap::new();
        let mut baseline = load_ci_baseline(Some(&baseline_path));
        poll_once_with_baseline(
            &config,
            &client,
            &mut state,
            &mut baseline,
            Some(&baseline_path),
            &tx,
        )
        .await
        .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "restart prime must not replay attempt 1"
        );
        poll_once_with_baseline(
            &config,
            &client,
            &mut state,
            &mut baseline,
            Some(&baseline_path),
            &tx,
        )
        .await
        .unwrap();
        let mut delivered = Vec::new();
        while let Ok(event) = rx.try_recv() {
            delivered.push(event);
        }
        assert_eq!(delivered.len(), 1, "attempt 2 must deliver exactly once");
        assert_eq!(delivered[0].canonical_kind(), "github.ci-failed");
        assert_eq!(delivered[0].payload["run_id"], json!("4242"));
        assert_eq!(delivered[0].payload["run_attempt"], json!(2));
        let stored = load_ci_baseline(Some(&baseline_path));
        let repo = stored
            .repos
            .values()
            .find(|repo| repo.contains_identity("run:4242") || repo.contains_identity("run:4242:2"))
            .expect("repo baseline");
        assert!(repo.contains_identity("run:4242"));
        assert!(repo.contains_identity("run:4242:2"));
        server.abort();
    }

    #[test]
    fn issue_317_oversized_loaded_state_is_preserved_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut records = Vec::new();
        for id in 0..(MAX_BASELINE_RUNS_PER_REPO + 12) {
            records.push(format!(
                r#"{{"identities":["run:{id}"],"run_id":"{id}","created_at":"2026-01-01T00:{:02}:{:02}Z"}}"#,
                id / 60,
                id % 60
            ));
        }
        let payload = format!(r#"{{"/repo":{{"terminal_runs":[{}]}}}}"#, records.join(","));
        std::fs::write(&path, payload).unwrap();
        let loaded = load_ci_baseline(Some(&path));
        let repo = loaded.repos.get("/repo").unwrap();
        assert_eq!(
            repo.terminal_runs.len(),
            MAX_BASELINE_RUNS_PER_REPO + 12,
            "load must not evict a stored current window"
        );
        assert!(repo.contains_identity("run:0"));
        assert!(repo.contains_identity(&format!("run:{}", MAX_BASELINE_RUNS_PER_REPO + 11)));
    }

    #[test]
    fn issue_317_incomplete_pagination_window_is_detected() {}

    #[test]
    fn issue_317_workflow_rerun_attempt_is_distinct_from_legacy_run_id() {
        let attempt1 = GitHubCISnapshot {
            run_attempt: 1,
            ..run_snapshot(4242, "CI", Some("success"), "main")
        };
        let attempt2 = GitHubCISnapshot {
            run_attempt: 2,
            conclusion: Some("failure".into()),
            ..run_snapshot(4242, "CI", Some("failure"), "main")
        };
        assert_ne!(attempt1.dedupe_key(), attempt2.dedupe_key());
        assert_eq!(run_map(&[attempt1.clone(), attempt2.clone()]).len(), 2);

        let mut ci_baseline = CIBaseline::default();
        ci_baseline.record_terminal_runs(
            "/repo",
            "/repo",
            &run_map(std::slice::from_ref(&attempt1)),
        );
        assert!(repo_ci_baseline_is_suppressed(
            ci_baseline.repos.get("/repo"),
            &attempt1
        ));
        assert!(
            !repo_ci_baseline_is_suppressed(ci_baseline.repos.get("/repo"), &attempt2),
            "attempt 2 must not be swallowed by a legacy run:<id> receipt for attempt 1"
        );

        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&attempt2)),
            ci_baseline.repos.get("/repo"),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.ci-failed");
    }

    #[test]
    fn issue_317_pagination_cycle_keys_normalize_variants_and_self_cycles() {
        let first = "https://API.GITHUB.com:443/repos/org/repo/actions/runs/?b=2&a=1#ignored";
        let equivalent = "https://api.github.com/repos/org/repo/actions/runs?a=1&b=2";
        assert_eq!(canonical_next_url(first), canonical_next_url(equivalent));

        let current = "https://api.github.com/repos/org/repo/actions/runs?page=1";
        let self_cycle = "https://API.GITHUB.com:443/repos/org/repo/actions/runs/?page=1#next";
        let mut seen = HashSet::new();
        seen.insert(pagination_page_key(current));
        assert!(
            !seen.insert(pagination_page_key(self_cycle)),
            "a next link back to the current page must terminate immediately"
        );
    }

    #[test]
    fn issue_317_pending_outbox_retries_undelivered_terminal_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let run = run_snapshot(77, "CI", Some("success"), "main");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&run)),
            None,
        );
        assert_eq!(events.len(), 1);

        let mut ci_baseline = CIBaseline::default();
        ci_baseline.record_terminal_runs("/repo", "/repo", &run_map(std::slice::from_ref(&run)));
        ci_baseline.enqueue_pending(
            "/repo",
            "/repo",
            &events,
            &run_map(std::slice::from_ref(&run)),
        );
        save_ci_baseline(&ci_baseline, Some(&path)).unwrap();

        let restarted = load_ci_baseline(Some(&path));
        assert_eq!(restarted.repos["/repo"].pending.len(), 1);
        let pending = restarted.repos["/repo"].pending.clone();
        assert_eq!(pending[0].kind, "github.ci-passed");
        let mut acked = restarted;
        ack_pending_deliveries(&mut acked, Some(&path), &pending).unwrap();
        let reloaded = load_ci_baseline(Some(&path));
        assert!(reloaded.repos["/repo"].pending.is_empty());
        assert!(repo_ci_baseline_is_suppressed(
            reloaded.repos.get("/repo"),
            &run
        ));
    }

    #[test]
    fn issue_317_ack_save_failure_retains_live_pending_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let run = run_snapshot(77, "CI", Some("success"), "main");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&run)),
            None,
        );
        let mut ci_baseline = CIBaseline::default();
        ci_baseline.record_terminal_runs("/repo", "/repo", &run_map(std::slice::from_ref(&run)));
        ci_baseline.enqueue_pending(
            "/repo",
            "/repo",
            &events,
            &run_map(std::slice::from_ref(&run)),
        );
        save_ci_baseline(&ci_baseline, Some(&path)).unwrap();
        let pending = ci_baseline.repos["/repo"].pending.clone();
        assert_eq!(pending.len(), 1);

        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, b"file").unwrap();
        let bad_path = blocker.join("github-ci-baseline.json");
        assert!(ack_pending_deliveries(&mut ci_baseline, Some(&bad_path), &pending).is_err());
        assert_eq!(
            ci_baseline.repos["/repo"].pending.len(),
            1,
            "live pending must survive an ACK persist failure so restart can retry"
        );
        assert_eq!(
            load_ci_baseline(Some(&path)).repos["/repo"].pending.len(),
            1
        );

        ack_pending_deliveries(&mut ci_baseline, Some(&path), &pending).unwrap();
        assert!(ci_baseline.repos["/repo"].pending.is_empty());
        assert!(
            load_ci_baseline(Some(&path)).repos["/repo"]
                .pending
                .is_empty()
        );
    }

    #[test]
    fn issue_317_pending_equivalence_keeps_distinct_run_attempts() {
        let attempt1 = GitHubCISnapshot {
            run_attempt: 1,
            ..run_snapshot(4242, "CI", Some("success"), "main")
        };
        let attempt2 = GitHubCISnapshot {
            run_attempt: 2,
            conclusion: Some("failure".into()),
            ..run_snapshot(4242, "CI", Some("failure"), "main")
        };
        let events1 = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&attempt1)),
            None,
        );
        let events2 = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&attempt2)),
            None,
        );
        let pending1 = PendingCiDelivery::from_event(&events1[0], attempt1.identities());
        let pending2 = PendingCiDelivery::from_event(&events2[0], attempt2.identities());
        assert!(
            !pending1.same_event(&pending2),
            "attempt 2 must not coalesce with attempt 1 in outbox equivalence"
        );

        let mut dest = RepoCIBaseline::default();
        dest.pending.push(pending1.clone());
        merge_repo_baseline(
            &mut dest,
            RepoCIBaseline {
                pending: vec![pending2.clone()],
                ..RepoCIBaseline::default()
            },
        );
        assert_eq!(dest.pending.len(), 2);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut left = CIBaseline::default();
        left.enqueue_pending(
            "/repo",
            "/repo",
            &events1,
            &run_map(std::slice::from_ref(&attempt1)),
        );
        commit_ci_baseline(Some(&path), &left).unwrap();
        let mut right = CIBaseline::default();
        right.enqueue_pending(
            "/repo",
            "/repo",
            &events2,
            &run_map(std::slice::from_ref(&attempt2)),
        );
        commit_ci_baseline(Some(&path), &right).unwrap();
        let loaded = load_ci_baseline(Some(&path));
        assert_eq!(loaded.repos["/repo"].pending.len(), 2);
    }

    fn check_job(run_id: u64, check_id: u64, workflow: &str, conclusion: &str) -> GitHubCISnapshot {
        GitHubCISnapshot {
            check_run_id: Some(check_id.to_string()),
            workflow: workflow.into(),
            conclusion: Some(conclusion.into()),
            status: "completed".into(),
            run_all_terminal: true,
            run_job_count: 2,
            ..run_snapshot(run_id, workflow, Some(conclusion), "main")
        }
    }

    #[test]
    fn issue_317_pending_keeps_distinct_check_jobs_in_one_run() {
        let lint = check_job(4242, 11, "lint", "success");
        let test = check_job(4242, 22, "test", "failure");
        let current = run_map(&[lint.clone(), test.clone()]);
        assert_eq!(current.len(), 2, "check jobs must not share a HashMap key");

        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &current,
            None,
        );
        assert_eq!(events.len(), 2, "both terminal jobs must publish");

        let mut baseline = CIBaseline::default();
        baseline.enqueue_pending("/repo", "/repo", &events, &current);
        assert_eq!(baseline.repos["/repo"].pending.len(), 2);

        let success = events
            .iter()
            .find(|event| event.canonical_kind() == "github.ci-passed")
            .unwrap();
        let failure = events
            .iter()
            .find(|event| event.canonical_kind() == "github.ci-failed")
            .unwrap();
        let pending_ok = PendingCiDelivery::from_event(success, lint.identities());
        let pending_fail = PendingCiDelivery::from_event(failure, test.identities());
        assert!(!pending_ok.same_event(&pending_fail));

        let stale_ok = pending_ok.clone();
        let mut flipped = check_job(4242, 11, "lint", "failure");
        flipped.sha = lint.sha.clone();
        let flip_events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &run_map(std::slice::from_ref(&lint)),
            &run_map(std::slice::from_ref(&flipped)),
            None,
        );
        assert_eq!(flip_events.len(), 1);
        let pending_flip = PendingCiDelivery::from_event(&flip_events[0], flipped.identities());
        assert!(!stale_ok.same_event(&pending_flip));
        let mut dest = RepoCIBaseline::default();
        dest.pending.push(stale_ok.clone());
        merge_repo_baseline(
            &mut dest,
            RepoCIBaseline {
                pending: vec![pending_flip.clone()],
                ..RepoCIBaseline::default()
            },
        );
        assert_eq!(dest.pending.len(), 2);

        let mut ci_baseline = CIBaseline::default();
        ci_baseline.enqueue_pending("/repo", "/repo", &events, &current);
        bump_pending_send_attempts(
            &mut ci_baseline,
            std::slice::from_ref(&pending_ok),
            unix_now(),
        );
        let pending = &ci_baseline.repos["/repo"].pending;
        let bumped = pending
            .iter()
            .find(|item| item.same_event(&pending_ok))
            .unwrap();
        let untouched = pending
            .iter()
            .find(|item| item.same_event(&pending_fail))
            .unwrap();
        assert_eq!(bumped.send_attempts, 1);
        assert_eq!(untouched.send_attempts, 0);

        ack_pending_deliveries(&mut ci_baseline, None, std::slice::from_ref(&pending_ok)).unwrap();
        assert_eq!(ci_baseline.repos["/repo"].pending.len(), 1);
        assert!(ci_baseline.repos["/repo"].pending[0].same_event(&pending_fail));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut persist = CIBaseline::default();
        persist.enqueue_pending("/repo", "/repo", &events, &current);
        persist.enqueue_pending(
            "/repo",
            "/repo",
            &flip_events,
            &run_map(std::slice::from_ref(&flipped)),
        );
        save_ci_baseline(&persist, Some(&path)).unwrap();
        let restarted = load_ci_baseline(Some(&path));
        assert_eq!(
            restarted.repos["/repo"].pending.len(),
            3,
            "restart must keep both jobs and the changed conclusion"
        );
    }

    #[test]
    fn issue_317_concurrent_acks_do_not_resurrect_acked_deliveries() {
        let lint = check_job(4242, 11, "lint", "success");
        let test = check_job(4242, 22, "test", "failure");
        let current = run_map(&[lint.clone(), test.clone()]);
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &current,
            None,
        );
        let pending_a = PendingCiDelivery::from_event(
            events
                .iter()
                .find(|event| event.canonical_kind() == "github.ci-passed")
                .unwrap(),
            lint.identities(),
        );
        let pending_b = PendingCiDelivery::from_event(
            events
                .iter()
                .find(|event| event.canonical_kind() == "github.ci-failed")
                .unwrap(),
            test.identities(),
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut durable = CIBaseline::default();
        durable.enqueue_pending("/repo", "/repo", &events, &current);
        save_ci_baseline(&durable, Some(&path)).unwrap();

        let start = Arc::new(Barrier::new(2));
        thread::scope(|scope| {
            let path_a = path.clone();
            let pending_one = pending_a.clone();
            let start_a = start.clone();
            scope.spawn(move || {
                let mut proc1 = load_ci_baseline(Some(&path_a));
                start_a.wait();
                ack_pending_deliveries(
                    &mut proc1,
                    Some(&path_a),
                    std::slice::from_ref(&pending_one),
                )
                .unwrap();
            });
            let path_b = path.clone();
            let pending_two = pending_b.clone();
            scope.spawn(move || {
                let mut proc2 = load_ci_baseline(Some(&path_b));
                start.wait();
                ack_pending_deliveries(
                    &mut proc2,
                    Some(&path_b),
                    std::slice::from_ref(&pending_two),
                )
                .unwrap();
            });
        });

        let restarted = load_ci_baseline(Some(&path));
        assert!(
            restarted.repos["/repo"].pending.is_empty(),
            "stale concurrent ACK must not restore an already ACKed delivery"
        );

        let (tx, mut rx) = mpsc::channel(8);
        let mut drain_state = restarted.clone();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            drain_pending_outbox(&mut drain_state, Some(&path), &tx)
                .await
                .unwrap();
        });
        assert!(
            rx.try_recv().is_err(),
            "restart drain must not redeliver an ACKed event"
        );
    }

    #[test]
    fn issue_317_stale_lock_reclaim_does_not_delete_live_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let lock_path = path.with_extension("json.lock");
        fs::create_dir_all(&lock_path).unwrap();
        fs::write(lock_path.join("owner"), b"live-legacy-owner").unwrap();

        let err = acquire_baseline_lock(&path).unwrap_err();
        assert!(
            err.to_string().contains("leftover directory lock"),
            "new acquire must fail closed on a live legacy directory lock: {err}"
        );
        assert!(
            lock_path.is_dir(),
            "new acquire must not delete a live legacy directory lock"
        );

        fs::remove_dir_all(&lock_path).unwrap();
        let overlapping = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let start = Arc::new(Barrier::new(2));

        thread::scope(|scope| {
            for _ in 0..2 {
                let overlapping = overlapping.clone();
                let current = current.clone();
                let errors = errors.clone();
                let path = path.clone();
                let start = start.clone();
                scope.spawn(move || {
                    start.wait();
                    match acquire_baseline_lock(&path) {
                        Ok(lock) => {
                            current.fetch_add(1, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(30));
                            if current.load(Ordering::SeqCst) > 1 {
                                overlapping.fetch_add(1, Ordering::SeqCst);
                            }
                            current.fetch_sub(1, Ordering::SeqCst);
                            drop(lock);
                        }
                        Err(error) => errors.lock().unwrap().push(error.to_string()),
                    }
                });
            }
        });

        assert!(
            errors.lock().unwrap().is_empty(),
            "lock acquire failed: {:?}",
            errors.lock().unwrap()
        );
        assert_eq!(
            overlapping.load(Ordering::SeqCst),
            0,
            "advisory lock must serialize critical sections"
        );
    }

    #[test]
    fn issue_317_ack_covers_check_to_run_and_fallback_drift() {
        let check = check_job(4242, 11, "CI", "success");
        let current = run_map(std::slice::from_ref(&check));
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &current,
            None,
        );
        let mut repo = RepoCIBaseline::default();
        let pending_check = PendingCiDelivery::from_event(&events[0], check.identities());
        record_acked(&mut repo, &pending_check);

        let mut run_only = pending_check.clone();
        run_only
            .identities
            .retain(|identity| identity.starts_with("run:"));
        assert!(
            pending_is_acked(&repo, &run_only),
            "check-form ACK must cover later run-only representation"
        );

        let mut fallback_only = pending_check.clone();
        fallback_only
            .identities
            .retain(|identity| !identity.starts_with("run:") && !identity.starts_with("check:"));
        fallback_only.run_id = None;
        assert!(
            pending_is_acked(&repo, &fallback_only),
            "check-form ACK must cover later fallback representation"
        );

        let mut dest = CIBaseline::default();
        dest.repos.insert("/repo".into(), repo);
        dest.enqueue_pending("/repo", "/repo", &events, &current);
        assert!(
            dest.repos["/repo"].pending.is_empty(),
            "acked check delivery must not re-enqueue as run/fallback"
        );
    }

    #[test]
    fn issue_317_ack_does_not_cover_other_run_attempt() {
        let attempt1 = check_job(4242, 11, "CI", "success");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&attempt1)),
            None,
        );
        let mut repo = RepoCIBaseline::default();
        record_acked(
            &mut repo,
            &PendingCiDelivery::from_event(&events[0], attempt1.identities()),
        );
        let attempt2 = GitHubCISnapshot {
            run_attempt: 2,
            check_run_id: Some("99".into()),
            ..check_job(4242, 99, "CI", "success")
        };
        let events2 = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&attempt2)),
            None,
        );
        let pending2 = PendingCiDelivery::from_event(&events2[0], attempt2.identities());
        assert!(!pending_is_acked(&repo, &pending2));
    }

    #[test]
    fn issue_317_shared_check_id_does_not_collapse_run_attempts() {
        let attempt1 = check_job(4242, 11, "CI", "success");
        let events1 = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&attempt1)),
            None,
        );
        let pending1 = PendingCiDelivery::from_event(&events1[0], attempt1.identities());
        let attempt2 = GitHubCISnapshot {
            run_attempt: 2,
            check_run_id: Some("11".into()),
            ..check_job(4242, 11, "CI", "success")
        };
        let events2 = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&attempt2)),
            None,
        );
        let pending2 = PendingCiDelivery::from_event(&events2[0], attempt2.identities());
        assert!(!pending1.same_event(&pending2));
        let mut repo = RepoCIBaseline::default();
        record_acked(&mut repo, &pending1);
        assert!(!pending_is_acked(&repo, &pending2));
    }

    #[test]
    fn issue_317_check_only_ack_does_not_conflate_run_attempts() {
        let check = check_job(4242, 11, "CI", "success");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&check)),
            None,
        );
        let mut check_only = PendingCiDelivery::from_event(&events[0], check.identities());
        check_only.run_id = None;
        check_only.run_attempt = 1;
        assert!(!check_only.same_event(&PendingCiDelivery {
            run_attempt: 2,
            ..check_only.clone()
        }));
        let mut repo = RepoCIBaseline::default();
        record_acked(&mut repo, &check_only);
        let mut attempt2 = PendingCiDelivery::from_event(&events[0], check.identities());
        attempt2.run_attempt = 2;
        assert!(!AckedDelivery::from_pending(&check_only, 0).matches_pending(&attempt2));
        record_acked(&mut repo, &attempt2);
        assert_eq!(repo.acked.len(), 2);
        assert_eq!(repo.acked[0].run_attempt, 1);
        assert_eq!(repo.acked[1].run_attempt, 2);
        assert!(pending_is_acked(&repo, &attempt2));
    }

    #[tokio::test]
    async fn issue_317_check_ack_prevents_run_only_drain() {
        let check = check_job(4242, 11, "CI", "success");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&check)),
            None,
        );
        let mut run_only = PendingCiDelivery::from_event(&events[0], check.identities());
        run_only
            .identities
            .retain(|identity| identity.starts_with("run:"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut baseline = CIBaseline::default();
        baseline
            .repos
            .insert("/repo".into(), RepoCIBaseline::default());
        baseline
            .repos
            .get_mut("/repo")
            .unwrap()
            .pending
            .push(run_only.clone());
        save_ci_baseline(&baseline, Some(&path)).unwrap();
        let mut live = load_ci_baseline(Some(&path));
        ack_pending_deliveries(
            &mut live,
            Some(&path),
            std::slice::from_ref(&PendingCiDelivery::from_event(
                &events[0],
                check.identities(),
            )),
        )
        .unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        drain_pending_outbox(&mut live, Some(&path), &tx)
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
        assert!(
            load_ci_baseline(Some(&path)).repos["/repo"]
                .pending
                .is_empty()
        );
    }

    #[tokio::test]
    async fn issue_317_partial_send_acks_only_successful_prefix() {
        let lint = check_job(4242, 11, "lint", "success");
        let test = check_job(4242, 22, "test", "failure");
        let current = run_map(&[lint.clone(), test.clone()]);
        let mut events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &current,
            None,
        );
        events.sort_by(|left, right| {
            left.payload["workflow"]
                .as_str()
                .cmp(&right.payload["workflow"].as_str())
        });
        let delivered: Vec<_> = events
            .iter()
            .map(|event| {
                let identities = previous_snapshot_for_event(&current, event)
                    .map(GitHubCISnapshot::identities)
                    .unwrap_or_default();
                PendingCiDelivery::from_event(event, identities)
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut baseline = CIBaseline::default();
        baseline.enqueue_pending("/repo", "/repo", &events, &current);
        save_ci_baseline(&baseline, Some(&path)).unwrap();

        let (tx, mut rx) = mpsc::channel(1);
        let recv = tokio::spawn(async move {
            let first = rx.recv().await;
            drop(rx);
            first
        });
        let mut live = load_ci_baseline(Some(&path));
        let result = send_then_ack_prefix(&tx, &mut live, Some(&path), events, &delivered).await;
        assert!(
            result.is_ok(),
            "closed receiver must not abort later repository polling"
        );
        let _ = recv.await;
        let restarted = load_ci_baseline(Some(&path));
        let pending = &restarted.repos["/repo"].pending;
        assert_eq!(pending.len(), 1, "unsent B must remain pending");
        assert!(pending_is_acked(&restarted.repos["/repo"], &delivered[0]));
        assert!(!pending_is_acked(&restarted.repos["/repo"], &delivered[1]));
    }

    #[test]
    fn issue_317_exhausted_pending_is_dead_lettered_and_bounded() {
        let mut repo = RepoCIBaseline::default();
        for id in 0..(MAX_DEAD_LETTER + 8) {
            let job = check_job(9, id as u64, "later", "failure");
            let mut pending = PendingCiDelivery::from_event(
                &collect_ci_events(
                    &GitRepoMonitor::default(),
                    "org/repo",
                    true,
                    &HashMap::new(),
                    &run_map(std::slice::from_ref(&job)),
                    None,
                )[0],
                job.identities(),
            );
            pending.send_attempts = MAX_PENDING_SEND_ATTEMPTS;
            repo.pending.push(pending);
        }
        retire_exhausted_pending(&mut repo);
        assert!(repo.pending.is_empty());
        assert_eq!(repo.dead_letter.len(), MAX_DEAD_LETTER);
    }

    #[test]
    fn issue_317_ack_compaction_retires_stale_writers() {
        let job = check_job(7, 1, "CI", "success");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&job)),
            None,
        );
        let pending = PendingCiDelivery::from_event(&events[0], job.identities());
        let mut dest = RepoCIBaseline {
            epoch: 10,
            ..RepoCIBaseline::default()
        };
        record_acked(&mut dest, &pending);
        dest.acked[0].acked_unix = 0;
        compact_acked(&mut dest, ACK_RETENTION_SECS + 1);
        assert!(dest.acked.is_empty());
        assert_eq!(dest.min_writer_epoch, 10);

        let mut stale = RepoCIBaseline {
            epoch: 3,
            ..RepoCIBaseline::default()
        };
        stale.pending.push(pending.clone());
        merge_repo_baseline(&mut dest, stale);
        assert!(
            dest.pending.is_empty(),
            "stale writer below compacted epoch floor must not restore pending"
        );
    }

    #[test]
    fn issue_317_baseline_write_flushes_file_data() {
        SYNC_FILE_CALLS.with(|count| count.set(0));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut baseline = CIBaseline::default();
        baseline
            .repos
            .insert("/repo".into(), RepoCIBaseline::default());
        write_ci_baseline_atomic(&baseline, &path).unwrap();
        let calls = SYNC_FILE_CALLS.with(|count| count.get());
        assert!(
            calls >= 2,
            "temp file and destination file must be fsync'd, got {calls}"
        );
        let loaded = load_ci_baseline(Some(&path));
        assert!(loaded.repos.contains_key("/repo"));
    }

    #[test]
    fn issue_317_poll_merges_legacy_path_alias_before_suppression_lookup() {
        let run = run_snapshot(4242, "CI", Some("success"), "main");
        let mut baseline = CIBaseline::default();
        baseline.record_terminal_runs(
            "/old-path",
            "/old-path",
            &run_map(std::slice::from_ref(&run)),
        );
        let monitor = GitRepoMonitor {
            path: "/old-path".into(),
            github_repo: Some("Org/Repo".into()),
            ..GitRepoMonitor::default()
        };
        let snapshot = crate::source::git::GitSnapshot {
            repo_name: "repo".into(),
            repo_path: "/old-path".into(),
            worktree_path: "/old-path".into(),
            branch: "main".into(),
            head: "abc".into(),
            commits: Vec::new(),
            github_repo: Some("Org/Repo".into()),
        };
        let repo_key = ci_baseline_repo_key(&snapshot, &monitor);
        let _ = baseline.repo_baseline_mut(&repo_key, &monitor.path);
        let events = collect_ci_events(
            &monitor,
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&run)),
            baseline.repos.get(&repo_key),
        );
        assert!(
            events.is_empty(),
            "legacy path alias must suppress historical terminal runs after re-enrollment"
        );
    }

    #[test]
    fn issue_317_sibling_jobs_do_not_suppress_mixed_outcomes() {
        let lint = check_job(4242, 11, "lint", "success");
        let test = check_job(4242, 22, "test", "failure");
        let mut baseline = CIBaseline::default();
        baseline.record_terminal_runs("/repo", "/repo", &run_map(std::slice::from_ref(&lint)));
        assert!(repo_ci_baseline_is_suppressed(
            baseline.repos.get("/repo"),
            &lint
        ));
        assert!(
            !repo_ci_baseline_is_suppressed(baseline.repos.get("/repo"), &test),
            "failed sibling must not be suppressed by a successful sibling in the same run"
        );
    }

    #[test]
    fn issue_317_run_only_receipt_does_not_suppress_check_jobs() {
        let aggregate = run_snapshot(4242, "CI", Some("success"), "main");
        let lint = check_job(4242, 11, "lint", "success");
        let test = check_job(4242, 22, "test", "failure");
        let mut baseline = CIBaseline::default();
        baseline.record_terminal_runs("/repo", "/repo", &run_map(std::slice::from_ref(&aggregate)));
        assert!(!repo_ci_baseline_is_suppressed(
            baseline.repos.get("/repo"),
            &lint
        ));
        assert!(
            !repo_ci_baseline_is_suppressed(baseline.repos.get("/repo"), &test),
            "run-only historical receipt must not suppress mixed check jobs"
        );
    }

    #[test]
    fn issue_317_first_poll_after_restart_emits_unrecorded_terminal() {
        let historical = run_snapshot(1, "CI", Some("success"), "main");
        let fresh = run_snapshot(2, "CI", Some("failure"), "main");
        let mut baseline = CIBaseline::default();
        baseline.record_terminal_runs(
            "/repo",
            "/repo",
            &run_map(std::slice::from_ref(&historical)),
        );
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(&[historical.clone(), fresh.clone()]),
            baseline.repos.get("/repo"),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.ci-failed");
    }

    #[test]
    fn issue_317_empty_established_baseline_restart_emits_first_terminal() {
        let fresh = run_snapshot(2, "CI", Some("failure"), "main");
        let mut baseline = CIBaseline::default();
        baseline.repo_baseline_mut("/repo", "/repo").ci_established = true;
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&fresh)),
            baseline.repos.get("/repo"),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.ci-failed");
    }

    #[test]
    fn issue_317_same_check_id_same_conclusion_rerun_still_emits() {
        let attempt1 = check_job(4242, 11, "CI", "success");
        let attempt2 = GitHubCISnapshot {
            run_attempt: 2,
            check_run_id: Some("11".into()),
            ..check_job(4242, 11, "CI", "success")
        };
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &run_map(std::slice::from_ref(&attempt1)),
            &run_map(std::slice::from_ref(&attempt2)),
            None,
        );
        assert_eq!(
            events.len(),
            1,
            "same check id and conclusion on a new attempt must emit"
        );
    }

    #[test]
    fn issue_317_fallback_kind_change_is_not_suppressed() {
        let success = check_job(7, 1, "CI", "success");
        let mut fallback = PendingCiDelivery::from_event(
            &collect_ci_events(
                &GitRepoMonitor::default(),
                "org/repo",
                true,
                &HashMap::new(),
                &run_map(std::slice::from_ref(&success)),
                None,
            )[0],
            success.identities(),
        );
        fallback
            .identities
            .retain(|identity| !identity.starts_with("run:") && !identity.starts_with("check:"));
        fallback.run_id = None;
        let mut failed = fallback.clone();
        failed.kind = "github.ci-failed".into();
        failed.conclusion = Some("failure".into());
        assert!(!fallback.same_event(&failed));
        let mut repo = RepoCIBaseline::default();
        record_acked(&mut repo, &fallback);
        assert!(!pending_is_acked(&repo, &failed));
    }

    #[test]
    fn issue_317_restart_preserves_nondefault_format_and_template() {
        let run = run_snapshot(88, "CI", Some("success"), "main");
        let mut events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&run)),
            None,
        );
        events[0].format = Some(crate::events::MessageFormat::Alert);
        events[0].template = Some("ci-alert".into());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut baseline = CIBaseline::default();
        baseline.enqueue_pending(
            "/repo",
            "/repo",
            &events,
            &run_map(std::slice::from_ref(&run)),
        );
        save_ci_baseline(&baseline, Some(&path)).unwrap();
        let restarted = load_ci_baseline(Some(&path));
        let restored = restarted.repos["/repo"].pending[0].clone().into_event();
        assert_eq!(restored.format, Some(crate::events::MessageFormat::Alert));
        assert_eq!(restored.template.as_deref(), Some("ci-alert"));
    }

    #[test]
    fn issue_317_dead_letter_tombstone_survives_stale_pending_merge() {
        let job = check_job(3, 9, "CI", "failure");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&job)),
            None,
        );
        let mut pending = PendingCiDelivery::from_event(&events[0], job.identities());
        pending.send_attempts = MAX_PENDING_SEND_ATTEMPTS;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut live = CIBaseline::default();
        live.repos.insert("/repo".into(), RepoCIBaseline::default());
        live.repos
            .get_mut("/repo")
            .unwrap()
            .pending
            .push(pending.clone());
        save_ci_baseline(&live, Some(&path)).unwrap();
        dead_letter_unsent_final_attempts(&mut live, std::slice::from_ref(&pending));
        let _ = commit_ci_baseline(Some(&path), &live).unwrap();
        let restarted = load_ci_baseline(Some(&path));
        assert!(restarted.repos["/repo"].pending.is_empty());
        assert_eq!(restarted.repos["/repo"].dead_letter.len(), 1);
    }

    #[test]
    fn issue_317_stale_writer_below_epoch_floor_cannot_restore_pending() {
        let start = std::sync::Arc::new(std::sync::Barrier::new(1));
        let job = check_job(4, 1, "CI", "success");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&job)),
            None,
        );
        let pending = PendingCiDelivery::from_event(&events[0], job.identities());
        let mut dest = RepoCIBaseline {
            epoch: 10,
            min_writer_epoch: 10,
            ..RepoCIBaseline::default()
        };
        let mut stale = RepoCIBaseline {
            epoch: 3,
            ..RepoCIBaseline::default()
        };
        stale.pending.push(pending);
        start.wait();
        merge_repo_baseline(&mut dest, stale);
        assert!(
            dest.pending.is_empty(),
            "stale writer below epoch floor must not restore pending"
        );
    }

    #[test]
    fn issue_317_acked_tombstones_survive_many_later_acks() {
        let first = check_job(1, 1, "first", "success");
        let extra = check_job(1, 2, "extra", "success");
        let current = run_map(&[first.clone(), extra.clone()]);
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &current,
            None,
        );
        let pending_a = PendingCiDelivery::from_event(
            events
                .iter()
                .find(|event| event.payload["workflow"] == "first")
                .unwrap(),
            first.identities(),
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut durable = CIBaseline::default();
        durable.enqueue_pending("/repo", "/repo", &events, &current);
        save_ci_baseline(&durable, Some(&path)).unwrap();
        let mut proc1 = load_ci_baseline(Some(&path));
        ack_pending_deliveries(&mut proc1, Some(&path), std::slice::from_ref(&pending_a)).unwrap();

        let mut later = load_ci_baseline(Some(&path));
        for id in 10..530 {
            let job = check_job(9, id, "later", "success");
            let later_events = collect_ci_events(
                &GitRepoMonitor::default(),
                "org/repo",
                true,
                &HashMap::new(),
                &run_map(std::slice::from_ref(&job)),
                None,
            );
            later.enqueue_pending(
                "/repo",
                "/repo",
                &later_events,
                &run_map(std::slice::from_ref(&job)),
            );
            let pending = PendingCiDelivery::from_event(&later_events[0], job.identities());
            ack_pending_deliveries(&mut later, Some(&path), std::slice::from_ref(&pending))
                .unwrap();
        }

        let mut stale = CIBaseline::default();
        stale.enqueue_pending("/repo", "/repo", &events, &current);
        commit_ci_baseline(Some(&path), &stale).unwrap();
        let restarted = load_ci_baseline(Some(&path));
        assert!(
            !restarted.repos["/repo"]
                .pending
                .iter()
                .any(|pending| pending.same_event(&pending_a)),
            "tombstone must survive hundreds of later ACKs and a stale merge"
        );
    }

    #[test]
    fn issue_317_pending_recovery_does_not_match_across_run_attempts() {
        let attempt1 = GitHubCISnapshot {
            run_attempt: 1,
            ..run_snapshot(4242, "CI", Some("success"), "main")
        };
        let attempt2 = GitHubCISnapshot {
            run_attempt: 2,
            conclusion: Some("failure".into()),
            ..run_snapshot(4242, "CI", Some("failure"), "main")
        };
        let current = run_map(&[attempt1.clone(), attempt2.clone()]);
        let events1 = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&attempt1)),
            None,
        );
        let events2 = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&attempt2)),
            None,
        );
        assert_eq!(
            previous_snapshot_for_event(&current, &events1[0])
                .unwrap()
                .run_attempt(),
            1
        );
        assert_eq!(
            previous_snapshot_for_event(&current, &events2[0])
                .unwrap()
                .run_attempt(),
            2
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut ci_baseline = CIBaseline::default();
        ci_baseline.enqueue_pending("/repo", "/repo", &events1, &current);
        ci_baseline.enqueue_pending("/repo", "/repo", &events2, &current);
        save_ci_baseline(&ci_baseline, Some(&path)).unwrap();

        let restarted = load_ci_baseline(Some(&path));
        assert_eq!(restarted.repos["/repo"].pending.len(), 2);
        for pending in &restarted.repos["/repo"].pending {
            let recovered = previous_snapshot_for_event(&current, &pending.clone().into_event())
                .expect("pending must recover against its own attempt");
            assert_eq!(
                recovered.run_attempt(),
                run_attempt_or_one(pending.run_attempt)
            );
            assert!(
                recovered
                    .unique_identities()
                    .iter()
                    .any(|identity| pending.identities.contains(identity)),
                "recovered identities must stay on the same attempt"
            );
        }
    }

    #[tokio::test]
    async fn issue_317_incomplete_window_does_not_suppress_new_terminal_as_prime() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut poll = 0_u32;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0_u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                if !request.contains("/actions/runs") {
                    let body = "[]";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    continue;
                }
                let page = request
                    .split(['?', '&'])
                    .find_map(|part| part.strip_prefix("page="))
                    .and_then(|value| {
                        value
                            .chars()
                            .take_while(|ch| ch.is_ascii_digit())
                            .collect::<String>()
                            .parse::<usize>()
                            .ok()
                    })
                    .unwrap_or(1);
                if page == 1 {
                    poll += 1;
                }
                let start = (page - 1) * CI_PAGE_SIZE;
                let mut ids: Vec<u64> =
                    (start as u64 + 1..=start as u64 + CI_PAGE_SIZE as u64).collect();
                if poll >= 2 && page == 1 {
                    ids[0] = 9_001;
                }
                let runs: Vec<String> = ids
                    .iter()
                    .map(|id| {
                        let conclusion = if *id == 9_001 { "failure" } else { "success" };
                        format!(
                            r#"{{"id":{id},"name":"CI","status":"completed","conclusion":"{conclusion}","head_branch":"main","head_sha":"sha-{id}","html_url":"https://github.com/org/repo/actions/runs/{id}","pull_requests":[],"created_at":"2026-01-01T00:00:00Z","run_attempt":1}}"#
                        )
                    })
                    .collect();
                let body = format!(r#"{{"workflow_runs":[{}]}}"#, runs.join(","));
                let next = page + 1;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nlink: <http://{addr}/repos/org/repo/actions/runs?per_page=100&page={next}>; rel=\"next\"\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                if poll >= 4 && page == 1 {
                    break;
                }
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let baseline_path = dir.path().join("github-ci-baseline.json");
        let mut config = AppConfig::default();
        config.monitors.github_api_base = format!("http://{addr}");
        config.monitors.git.repos = vec![GitRepoMonitor {
            path: "/tmp/clawhip-incomplete-window".into(),
            name: Some("repo".into()),
            github_repo: Some("org/repo".into()),
            emit_pr_status: true,
            emit_issue_opened: false,
            emit_commits: false,
            emit_branch_changes: false,
            ..GitRepoMonitor::default()
        }];
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let mut state = HashMap::new();
        let mut baseline = CIBaseline::default();
        for _ in 0..3 {
            poll_once_with_baseline(
                &config,
                &client,
                &mut state,
                &mut baseline,
                Some(&baseline_path),
                &tx,
            )
            .await
            .unwrap();
        }
        let mut delivered = Vec::new();
        while let Ok(event) = rx.try_recv() {
            delivered.push(event);
        }
        assert_eq!(
            delivered.len(),
            1,
            "new terminal failure must emit once, not be primed away; got {delivered:?}"
        );
        assert_eq!(delivered[0].payload["run_id"], json!("9001"));
        assert_eq!(delivered[0].canonical_kind(), "github.ci-failed");
        assert!(state["/tmp/clawhip-incomplete-window"].ci_baseline_established);
        server.abort();
    }

    #[tokio::test]
    async fn issue_317_persistent_ack_failure_bounds_resends_and_keeps_polling() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_server = hits.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0_u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                if request.contains("/issues")
                    || request.contains("/pulls")
                    || request.contains("/actions/runs")
                {
                    hits_server.fetch_add(1, Ordering::SeqCst);
                }
                let body = if request.contains("/actions/runs") {
                    r#"{"workflow_runs":[]}"#
                } else {
                    "[]"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let good_path = dir.path().join("github-ci-baseline.json");
        let run = run_snapshot(77, "CI", Some("success"), "main");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&run)),
            None,
        );
        let mut seeded = CIBaseline::default();
        seeded.enqueue_pending(
            "repo:org/repo",
            "/tmp/ack-a",
            &events,
            &run_map(std::slice::from_ref(&run)),
        );
        save_ci_baseline(&seeded, Some(&good_path)).unwrap();

        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, b"file").unwrap();
        let bad_path = blocker.join("github-ci-baseline.json");

        let mut config = AppConfig::default();
        config.monitors.github_api_base = format!("http://{addr}");
        config.monitors.git.repos = vec![
            GitRepoMonitor {
                path: "/tmp/ack-a".into(),
                name: Some("a".into()),
                github_repo: Some("org/repo".into()),
                emit_pr_status: true,
                emit_issue_opened: true,
                emit_commits: false,
                emit_branch_changes: false,
                ..GitRepoMonitor::default()
            },
            GitRepoMonitor {
                path: "/tmp/ack-b".into(),
                name: Some("b".into()),
                github_repo: Some("org/other".into()),
                emit_pr_status: true,
                emit_issue_opened: true,
                emit_commits: false,
                emit_branch_changes: false,
                ..GitRepoMonitor::default()
            },
        ];
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(32);
        let mut state = HashMap::new();
        let mut baseline = load_ci_baseline(Some(&good_path));
        for _ in 0..3 {
            poll_once_with_baseline(
                &config,
                &client,
                &mut state,
                &mut baseline,
                Some(&bad_path),
                &tx,
            )
            .await
            .unwrap();
        }
        let mut sent = 0_usize;
        while rx.try_recv().is_ok() {
            sent += 1;
        }
        assert_eq!(
            sent, 0,
            "pre-send persist failure must not publish pending outbox events"
        );
        assert!(
            hits.load(Ordering::SeqCst) >= 6,
            "ACK failure must not abort polling of remaining repositories"
        );
        server.abort();
    }

    #[test]
    fn issue_317_merge_pending_keeps_monotonic_retry_state() {
        let run = run_snapshot(77, "CI", Some("success"), "main");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&run)),
            None,
        );
        let mut older = PendingCiDelivery::from_event(&events[0], run.identities());
        older.send_attempts = 0;
        older.last_sent_unix = Some(10);
        let mut newer = older.clone();
        newer.send_attempts = 2;
        newer.last_sent_unix = Some(50);
        let mut dest = RepoCIBaseline::default();
        dest.pending.push(older);
        merge_repo_baseline(
            &mut dest,
            RepoCIBaseline {
                pending: vec![newer],
                ..RepoCIBaseline::default()
            },
        );
        assert_eq!(dest.pending.len(), 1);
        assert_eq!(dest.pending[0].send_attempts, 2);
        assert_eq!(dest.pending[0].last_sent_unix, Some(50));
    }

    #[test]
    fn issue_317_restart_after_ack_failure_honors_persisted_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let run = run_snapshot(77, "CI", Some("success"), "main");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&run)),
            None,
        );
        let mut ci_baseline = CIBaseline::default();
        ci_baseline.enqueue_pending(
            "/repo",
            "/repo",
            &events,
            &run_map(std::slice::from_ref(&run)),
        );
        save_ci_baseline(&ci_baseline, Some(&path)).unwrap();
        let pending = ci_baseline.repos["/repo"].pending.clone();
        bump_pending_send_attempts(&mut ci_baseline, &pending, unix_now());
        commit_ci_baseline(Some(&path), &ci_baseline).unwrap();

        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, b"file").unwrap();
        let bad_path = blocker.join("github-ci-baseline.json");
        assert!(ack_pending_deliveries(&mut ci_baseline, Some(&bad_path), &pending).is_err());

        let restarted = load_ci_baseline(Some(&path));
        let stored = &restarted.repos["/repo"].pending;
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].send_attempts, 1);
        assert!(stored[0].last_sent_unix.is_some());
        assert!(
            !pending_due_for_send(&stored[0], unix_now()),
            "restart must honor persisted backoff after ACK removal fails"
        );
    }

    #[test]
    fn issue_317_persist_retry_before_send_leaves_nonzero_attempt_without_publish() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let run = run_snapshot(77, "CI", Some("success"), "main");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&run)),
            None,
        );
        let mut ci_baseline = CIBaseline::default();
        ci_baseline.enqueue_pending(
            "/repo",
            "/repo",
            &events,
            &run_map(std::slice::from_ref(&run)),
        );
        save_ci_baseline(&ci_baseline, Some(&path)).unwrap();
        let pending = ci_baseline.repos["/repo"].pending.clone();
        persist_pending_send_attempts(&mut ci_baseline, Some(&path), &pending, unix_now()).unwrap();
        let stored = &load_ci_baseline(Some(&path)).repos["/repo"].pending;
        assert_eq!(stored.len(), 1);
        assert!(
            stored[0].send_attempts >= 1,
            "durable attempt must be recorded before any send"
        );
    }

    #[test]
    fn issue_317_failed_pre_send_persist_does_not_consume_retry_budget() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, b"file").unwrap();
        let bad_path = blocker.join("github-ci-baseline.json");
        let run = run_snapshot(77, "CI", Some("success"), "main");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&run)),
            None,
        );
        let mut ci_baseline = CIBaseline::default();
        ci_baseline.enqueue_pending(
            "/repo",
            "/repo",
            &events,
            &run_map(std::slice::from_ref(&run)),
        );
        let pending = ci_baseline.repos["/repo"].pending.clone();
        assert!(
            persist_pending_send_attempts(&mut ci_baseline, Some(&bad_path), &pending, unix_now())
                .is_err()
        );
        assert_eq!(ci_baseline.repos["/repo"].pending[0].send_attempts, 0);
        assert!(pending_due_for_send(
            &ci_baseline.repos["/repo"].pending[0],
            unix_now()
        ));
    }

    #[test]
    fn issue_317_in_progress_to_partial_complete_does_not_emit_until_all_jobs() {
        let started = GitHubCISnapshot {
            status: "in_progress".into(),
            conclusion: None,
            run_all_terminal: false,
            run_job_count: 2,
            ..run_snapshot(77, "CI", Some("success"), "main")
        };
        let partial = GitHubCISnapshot {
            status: "completed".into(),
            conclusion: Some("success".into()),
            run_all_terminal: false,
            run_job_count: 2,
            ..run_snapshot(77, "CI", Some("success"), "main")
        };
        let complete = GitHubCISnapshot {
            run_all_terminal: true,
            run_job_count: 2,
            ..run_snapshot(77, "CI", Some("success"), "main")
        };
        let repo = GitRepoMonitor::default();
        let events = collect_ci_events(
            &repo,
            "org/repo",
            true,
            &run_map(std::slice::from_ref(&started)),
            &run_map(std::slice::from_ref(&partial)),
            None,
        );
        assert!(
            events.is_empty(),
            "in-progress to completed-but-not-all-jobs must not publish"
        );
        let events = collect_ci_events(
            &repo,
            "org/repo",
            true,
            &run_map(std::slice::from_ref(&partial)),
            &run_map(std::slice::from_ref(&complete)),
            None,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.ci-passed");
    }

    #[test]
    fn issue_317_pre_send_persist_failure_does_not_publish() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, b"file").unwrap();
        let bad_path = blocker.join("github-ci-baseline.json");
        let run = run_snapshot(77, "CI", Some("success"), "main");
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&run)),
            None,
        );
        let mut ci_baseline = CIBaseline::default();
        ci_baseline.enqueue_pending(
            "/repo",
            "/repo",
            &events,
            &run_map(std::slice::from_ref(&run)),
        );
        assert!(
            persist_pending_send_attempts(
                &mut ci_baseline,
                Some(&bad_path),
                &events
                    .iter()
                    .map(|event| PendingCiDelivery::from_event(event, run.identities()))
                    .collect::<Vec<_>>(),
                unix_now()
            )
            .is_err()
        );
    }

    #[test]
    fn issue_317_merge_does_not_collapse_distinct_unique_fallback_overlap() {
        let first = TerminalRunRecord::from_snapshot(&GitHubCISnapshot {
            check_run_id: Some("111".into()),
            run_id: None,
            ..fallback_snapshot(58, "abcdef1234567890", "CI")
        });
        let second = TerminalRunRecord::from_snapshot(&GitHubCISnapshot {
            check_run_id: Some("222".into()),
            run_id: None,
            conclusion: Some("failure".into()),
            ..fallback_snapshot(58, "abcdef1234567890", "CI")
        });
        assert!(!terminal_records_same_run(&first, &second));
        let mut dest = RepoCIBaseline::default();
        dest.terminal_runs.push_back(first);
        merge_repo_baseline(
            &mut dest,
            RepoCIBaseline {
                terminal_runs: {
                    let mut q = VecDeque::new();
                    q.push_back(second);
                    q
                },
                ..RepoCIBaseline::default()
            },
        );
        assert_eq!(dest.terminal_runs.len(), 2);
    }

    #[test]
    fn issue_317_disk_merge_evicts_oldest_beyond_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let mut disk = CIBaseline::default();
        let mut on_disk = Vec::new();
        for id in 0..200 {
            let mut run = run_snapshot(id, "CI", Some("success"), "main");
            run.created_at = Some(format!("2026-01-01T00:{:02}:{:02}Z", id / 60, id % 60));
            on_disk.push(run);
        }
        disk.record_terminal_runs("/repo", "/repo", &run_map(&on_disk));
        save_ci_baseline(&disk, Some(&path)).unwrap();

        let mut incoming = CIBaseline::default();
        let mut extra = Vec::new();
        for id in 200..(MAX_BASELINE_RUNS_PER_REPO + 20) {
            let mut run = run_snapshot(id as u64, "CI", Some("success"), "main");
            run.created_at = Some(format!(
                "2026-01-01T01:{:02}:{:02}Z",
                (id - 200) / 60,
                (id - 200) % 60
            ));
            extra.push(run);
        }
        incoming.record_terminal_runs("/repo", "/repo", &run_map(&extra));
        commit_ci_baseline(Some(&path), &incoming).unwrap();
        let loaded = load_ci_baseline(Some(&path));
        let repo = &loaded.repos["/repo"];
        assert_eq!(repo.terminal_runs.len(), MAX_BASELINE_RUNS_PER_REPO);
        assert!(!repo.contains_identity("run:0"));
        assert!(repo.contains_identity(&format!("run:{}", MAX_BASELINE_RUNS_PER_REPO + 19)));
    }

    #[test]
    fn issue_317_incomplete_multi_job_does_not_emit_until_all_terminal() {
        let partial = GitHubCISnapshot {
            status: "completed".into(),
            conclusion: Some("success".into()),
            run_all_terminal: false,
            run_job_count: 2,
            ..run_snapshot(77, "CI", Some("success"), "main")
        };
        let complete = GitHubCISnapshot {
            run_all_terminal: true,
            run_job_count: 2,
            ..run_snapshot(77, "CI", Some("success"), "main")
        };
        let repo = GitRepoMonitor::default();
        let events = collect_ci_events(
            &repo,
            "org/repo",
            true,
            &HashMap::new(),
            &run_map(std::slice::from_ref(&partial)),
            None,
        );
        assert!(
            events.is_empty(),
            "completed jobs in an incomplete workflow must not publish"
        );
        let events = collect_ci_events(
            &repo,
            "org/repo",
            true,
            &run_map(std::slice::from_ref(&partial)),
            &run_map(std::slice::from_ref(&complete)),
            None,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.ci-passed");
    }

    #[tokio::test]
    async fn issue_317_incomplete_check_run_pages_are_not_all_terminal() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut page_hits = 0_usize;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = vec![0_u8; 4096];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                if !request.contains("/check-runs") {
                    let body = if request.contains("/actions/runs") {
                        r#"{"workflow_runs":[]}"#
                    } else {
                        "[]"
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    continue;
                }
                page_hits += 1;
                let start = (page_hits - 1) * CI_PAGE_SIZE;
                let runs: Vec<String> = (start..start + CI_PAGE_SIZE)
                    .map(|id| {
                        format!(
                            r#"{{"id":{},"name":"CI","status":"completed","conclusion":"success","details_url":"https://github.com/org/repo/actions/runs/4242/jobs/{id}","head_sha":"prsha"}}"#,
                            id + 1
                        )
                    })
                    .collect();
                let body = format!(r#"{{"check_runs":[{}]}}"#, runs.join(","));
                let next = page_hits + 1;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nlink: <http://{addr}/repos/org/repo/commits/prsha/check-runs?page={next}>; rel=\"next\"\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                if page_hits >= MAX_CI_PAGES {
                    break;
                }
            }
        });

        let pr = PullRequestSnapshot {
            title: "PR".into(),
            status: "open".into(),
            url: "https://github.com/org/repo/pull/42".into(),
            head_branch: "feat/pr".into(),
            head_sha: "prsha".into(),
            author: String::new(),
            updated_at: String::new(),
        };
        let client = build_github_client(None).unwrap();
        let (runs, complete) =
            fetch_check_runs(&client, &format!("http://{addr}"), "org/repo", 42, &pr)
                .await
                .unwrap();
        assert!(!complete);
        assert!(
            runs.iter()
                .all(|run| !run.run_all_terminal && !run.is_terminal()),
            "incomplete check-run pagination must not infer run_all_terminal"
        );
        server.abort();
    }

    #[test]
    fn issue_317_incomplete_window_keeps_current_rerun_on_check_key_collision() {
        let attempt1 = check_job(4242, 11, "CI", "success");
        let attempt2 = GitHubCISnapshot {
            run_attempt: 2,
            check_run_id: Some("11".into()),
            ..check_job(4242, 11, "CI", "success")
        };
        let previous = run_map(std::slice::from_ref(&attempt1));
        let current = run_map(std::slice::from_ref(&attempt2));
        let merged = merge_incomplete_ci(&previous, current);
        let kept = merged.get(&attempt2.dedupe_key()).unwrap();
        assert_eq!(kept.run_attempt(), 2);
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &previous,
            &merged,
            None,
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn issue_317_incomplete_window_keeps_omitted_in_progress_and_later_completion() {
        let in_progress = GitHubCISnapshot {
            status: "in_progress".into(),
            conclusion: None,
            run_all_terminal: false,
            ..run_snapshot(88, "CI", Some("success"), "main")
        };
        let completed = run_snapshot(88, "CI", Some("success"), "main");
        let visible = run_snapshot(99, "CI", Some("success"), "main");

        let previous = run_map(&[in_progress.clone(), visible.clone()]);
        let incomplete = run_map(std::slice::from_ref(&visible));
        let merged = merge_incomplete_ci(&previous, incomplete);
        assert!(
            merged
                .values()
                .any(|ci| snapshots_same_run(ci, &in_progress)),
            "omitted in-progress run must be retained across an incomplete page"
        );

        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &previous,
            &merged,
            None,
        );
        assert!(
            events.is_empty(),
            "reappearing omitted in-progress run must not duplicate github.ci-started"
        );

        let completed_window = run_map(&[completed.clone(), visible]);
        let events = collect_ci_events(
            &GitRepoMonitor::default(),
            "org/repo",
            true,
            &merged,
            &completed_window,
            None,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.ci-passed");
    }

    #[test]
    fn issue_317_repo_key_case_fold_merges_variants_without_cross_remote() {
        let run = run_snapshot(55, "CI", Some("success"), "main");
        let mut ci_baseline = CIBaseline::default();
        ci_baseline.record_terminal_runs(
            "repo:org/repo",
            "/old",
            &run_map(std::slice::from_ref(&run)),
        );

        let mixed = GitRepoMonitor {
            path: "/new".into(),
            github_repo: Some("Org/Repo".into()),
            ..GitRepoMonitor::default()
        };
        let snapshot = GitSnapshot {
            repo_name: "repo".into(),
            repo_path: "/new".into(),
            worktree_path: "/new".into(),
            branch: "main".into(),
            head: "sha-55".into(),
            commits: Vec::new(),
            github_repo: Some(" Org/Repo ".into()),
        };
        let key = ci_baseline_repo_key(&snapshot, &mixed);
        assert_eq!(key, "repo:org/repo");
        let _ = ci_baseline.repo_baseline_mut(&key, "/new");
        assert!(repo_ci_baseline_is_suppressed(
            ci_baseline.repos.get(&key),
            &run
        ));

        let other = GitHubCISnapshot {
            sha: "other".into(),
            ..run_snapshot(55, "CI", Some("success"), "main")
        };
        assert!(!repo_ci_baseline_is_suppressed(
            ci_baseline.repos.get("repo:other/remote"),
            &other
        ));
    }

    #[test]
    fn issue_317_locked_reload_merge_write_preserves_concurrent_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        save_ci_baseline(&CIBaseline::default(), Some(&path)).unwrap();

        let start = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            let start_left = start.clone();
            let path_left = path.clone();
            scope.spawn(move || {
                let mut left = CIBaseline::default();
                left.record_terminal_runs(
                    "repo:org/repo",
                    "/a",
                    &run_map(&[run_snapshot(1, "CI", Some("success"), "main")]),
                );
                start_left.wait();
                commit_ci_baseline(Some(&path_left), &left).unwrap();
            });
            let path_right = path.clone();
            scope.spawn(move || {
                let mut right = CIBaseline::default();
                right.record_terminal_runs(
                    "repo:org/repo",
                    "/a",
                    &run_map(&[run_snapshot(2, "CI", Some("success"), "main")]),
                );
                start.wait();
                commit_ci_baseline(Some(&path_right), &right).unwrap();
            });
        });

        let loaded = load_ci_baseline(Some(&path));
        let repo = loaded.repos.get("repo:org/repo").unwrap();
        assert!(repo.contains_identity("run:1"));
        assert!(repo.contains_identity("run:2"));
    }

    #[test]
    fn issue_317_issue_359_baseline_lock_waits_for_slow_windows_file_flush() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("github-ci-baseline.json");
        let held_lock = acquire_baseline_lock(&path).unwrap();
        let (contended_tx, contended_rx) = std::sync::mpsc::sync_channel(0);

        std::thread::scope(|scope| {
            let waiter_path = path.clone();
            let waiter = scope.spawn(move || {
                let lock_path = waiter_path.with_extension("json.lock");
                let probe = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(lock_path)
                    .unwrap();
                assert!(
                    !try_lock_baseline_file(&probe),
                    "waiter probe must be excluded by the live lock owner"
                );
                contended_tx.send(()).unwrap();
                drop(probe);
                acquire_baseline_lock(&waiter_path)
            });

            contended_rx.recv().unwrap();
            std::thread::sleep(Duration::from_secs(1));
            drop(held_lock);
            assert!(
                waiter.join().unwrap().is_ok(),
                "baseline lock waiter must tolerate a slow Windows replace-and-flush"
            );
        });
    }

    #[tokio::test]
    async fn issue_317_restart_and_pagination_cursor_loss_do_not_replay_historical_runs() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Fake GitHub Actions API page: one current run plus three historical
        // terminal runs (success / failure / cancelled) that predate the
        // monitor but re-enter the listing when pagination drifts (#317).
        let workflow_runs_body = |run_ids: &[u64]| {
            let runs: Vec<String> = run_ids
                .iter()
                .map(|id| {
                    let conclusion = match id % 3 {
                        0 => "success",
                        1 => "failure",
                        _ => "cancelled",
                    };
                    format!(
                        r#"{{"id": {id}, "name": "CI", "status": "completed", "conclusion": "{conclusion}", "head_branch": "main", "head_sha": "sha-{id}", "html_url": "https://github.com/org/repo/actions/runs/{id}", "pull_requests": []}}"#
                    )
                })
                .collect();
            format!("{{\"workflow_runs\": [{}]}}", runs.join(","))
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Poll sequence the fake server serves:
        //   1. first poll of process A: current + historical runs (primes
        //      baseline, no events),
        //   2. second poll of process A: unchanged (no events),
        //   3. first poll of restarted process B with the same page
        //      (pagination/cursor loss re-serving history): no events,
        //   4. poll after a genuinely new push run: exactly one event.
        let pages = [
            workflow_runs_body(&[32205447861, 27620587146, 24244183277, 29691404856]),
            workflow_runs_body(&[32205447861, 27620587146, 24244183277, 29691404856]),
            workflow_runs_body(&[32205447861, 27620587146, 24244183277, 29691404856]),
            workflow_runs_body(&[
                32205447861,
                27620587146,
                24244183277,
                29691404856,
                32206000001,
            ]),
        ];
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_for_server = request_count.clone();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            let mut next_page = 0_usize;
            while next_page < pages.len() {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0_u8; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                requests.push(request.clone());
                request_count_for_server.fetch_add(1, Ordering::SeqCst);

                // Serve the next workflow-run page only for actions/runs
                // requests; every other endpoint gets an empty listing so the
                // CI poll sequence stays deterministic.
                let body = if request.contains("/actions/runs") {
                    let page = &pages[next_page];
                    next_page += 1;
                    page.clone()
                } else {
                    "[]".to_string()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });

        let dir = tempfile::tempdir().unwrap();
        let baseline_path = dir.path().join("github-ci-baseline.json");

        let mut config = AppConfig::default();
        config.monitors.github_api_base = format!("http://{addr}");
        config.monitors.git.repos = vec![GitRepoMonitor {
            path: "/tmp/clawhip-replay-repo".into(),
            name: Some("repo".into()),
            github_repo: Some("org/repo".into()),
            emit_pr_status: true,
            emit_issue_opened: true,
            emit_commits: false,
            emit_branch_changes: false,
            ..GitRepoMonitor::default()
        }];
        let client = build_github_client(None).unwrap();

        // ---- Process A, poll 1: primes in-process baseline; nothing emits.
        let (tx, mut rx) = mpsc::channel(16);
        let mut state_a = HashMap::new();
        let mut baseline_a = load_ci_baseline(Some(&baseline_path));
        poll_once_with_baseline(
            &config,
            &client,
            &mut state_a,
            &mut baseline_a,
            Some(&baseline_path),
            &tx,
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err(), "first poll must not emit history");
        assert!(
            baseline_path.exists(),
            "terminal identities must be persisted"
        );

        // ---- Process A, poll 2: unchanged page, still silent.
        poll_once_with_baseline(
            &config,
            &client,
            &mut state_a,
            &mut baseline_a,
            Some(&baseline_path),
            &tx,
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());

        // ---- Process B (restart): fresh in-process state, baseline reloaded
        // from disk; the fake server re-serves the same page exactly as a
        // pagination drift / cursor loss would. Nothing may replay.
        let mut state_b = HashMap::new();
        let mut baseline_b = load_ci_baseline(Some(&baseline_path));
        poll_once_with_baseline(
            &config,
            &client,
            &mut state_b,
            &mut baseline_b,
            Some(&baseline_path),
            &tx,
        )
        .await
        .unwrap();
        assert!(
            rx.try_recv().is_err(),
            "restart must not replay historical terminal runs (#317)"
        );

        // ---- A genuinely new run lands: exactly one event for it.
        poll_once_with_baseline(
            &config,
            &client,
            &mut state_b,
            &mut baseline_b,
            Some(&baseline_path),
            &tx,
        )
        .await
        .unwrap();
        let mut delivered = Vec::new();
        while let Ok(event) = rx.try_recv() {
            delivered.push(event);
        }
        assert_eq!(
            delivered.len(),
            1,
            "exactly the new run must be delivered, got {delivered:?}"
        );
        assert_eq!(delivered[0].payload["run_id"], json!("32206000001"));

        // Persisted growth is bounded by the number of terminal runs observed
        // in the window — no history scan, no ledger growth.
        let persisted: HashMap<String, RepoCIBaseline> =
            serde_json::from_str(&std::fs::read_to_string(&baseline_path).unwrap()).unwrap();
        assert!(persisted["repo:org/repo"].terminal_runs.len() <= MAX_BASELINE_RUNS_PER_REPO);

        let requests = server.await.unwrap();
        // Four CI polls; the issues/pulls endpoints add two requests per poll.
        assert_eq!(requests.len(), 12);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.contains("GET /repos/org/repo/actions/runs?"))
                .count(),
            4,
            "exactly four CI polls must hit the workflow-runs endpoint"
        );
    }

    #[tokio::test]
    async fn direct_branch_workflow_run_without_open_pr_emits_ci_failed_event() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = json!({
                "workflow_runs": [{
                    "id": 24007460067_u64,
                    "name": "Rust CI",
                    "status": "completed",
                    "conclusion": "failure",
                    "head_branch": "main",
                    "head_sha": "deadbeef",
                    "html_url": "https://github.com/ultraworkers/claw-code/actions/runs/24007460067",
                    "pull_requests": []
                }]
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            req
        });

        let mut config = AppConfig::default();
        config.monitors.github_api_base = format!("http://{addr}");
        let repo = GitRepoMonitor {
            path: "/tmp/claw-code".into(),
            emit_pr_status: true,
            ..GitRepoMonitor::default()
        };
        let snapshot = GitSnapshot {
            repo_name: "claw-code".into(),
            repo_path: "/tmp/claw-code".into(),
            worktree_path: "/tmp/claw-code".into(),
            branch: "main".into(),
            head: "deadbeef".into(),
            commits: Vec::new(),
            github_repo: Some("ultraworkers/claw-code".into()),
        };
        let client = build_github_client(None).unwrap();
        let prs = HashMap::new();

        let (ci, ci_baseline_established, _window_complete) =
            poll_ci_statuses(&config, Some(&client), &repo, &snapshot, None, &prs, true)
                .await
                .unwrap();

        assert_eq!(ci.len(), 1);
        assert!(ci_baseline_established);

        let req = server.await.unwrap();
        assert!(req.contains("GET /repos/ultraworkers/claw-code/actions/runs?"));
        assert!(req.contains("branch=main"));
        assert!(req.contains("event=push"));
        assert!(req.contains("per_page=100"));
    }

    #[tokio::test]
    async fn direct_workflow_runs_skip_run_ids_already_seen_from_pr_checks() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            let responses = [
                json!({
                    "check_runs": [{
                        "name": "test",
                        "status": "completed",
                        "conclusion": "failure",
                        "details_url": "https://github.com/org/repo/actions/runs/123/jobs/1",
                        "head_sha": "prsha"
                    }]
                })
                .to_string(),
                json!({
                    "workflow_runs": [{
                        "id": 123_u64,
                        "name": "CI",
                        "status": "completed",
                        "conclusion": "failure",
                        "head_branch": "feat/pr",
                        "head_sha": "prsha",
                        "html_url": "https://github.com/org/repo/actions/runs/123",
                        "run_attempt": 1,
                        "pull_requests": [{"number": 42}]
                    }]
                })
                .to_string(),
                json!({
                    "workflow_runs": [{
                        "id": 456_u64,
                        "name": "Rust CI",
                        "status": "completed",
                        "conclusion": "failure",
                        "head_branch": "main",
                        "head_sha": "mainsha",
                        "html_url": "https://github.com/org/repo/actions/runs/456",
                        "pull_requests": []
                    }]
                })
                .to_string(),
            ];

            for body in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0_u8; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                requests.push(String::from_utf8_lossy(&buf[..n]).to_string());
                // connection: close prevents reqwest from reusing the TCP
                // stream — the mock server calls accept() per request, so
                // keep-alive pooling causes the 2nd request to go to a dead
                // connection under load (flake root-cause, see #194).
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }

            requests
        });

        let snapshot = GitSnapshot {
            repo_name: "repo".into(),
            repo_path: "/tmp/repo".into(),
            worktree_path: "/tmp/repo".into(),
            branch: "main".into(),
            head: "mainsha".into(),
            commits: Vec::new(),
            github_repo: Some("org/repo".into()),
        };
        let client = build_github_client(None).unwrap();
        let pr = PullRequestSnapshot {
            title: "PR".into(),
            status: "open".into(),
            url: "https://github.com/org/repo/pull/42".into(),
            head_branch: "feat/pr".into(),
            head_sha: "prsha".into(),
            author: String::new(),
            updated_at: String::new(),
        };
        let open_prs = vec![(42_u64, &pr)];

        let (ci, window_complete) = fetch_ci_statuses(
            &client,
            &format!("http://{addr}"),
            &GitRepoMonitor::default(),
            &snapshot,
            &open_prs,
        )
        .await
        .unwrap();

        assert_eq!(ci.len(), 2);
        assert!(window_complete);
        assert_eq!(
            ci.values()
                .filter(|snapshot| snapshot.run_id.as_deref() == Some("123"))
                .count(),
            1
        );
        let direct = ci
            .values()
            .find(|snapshot| snapshot.run_id.as_deref() == Some("456"))
            .unwrap();
        assert_eq!(direct.pr_number, None);
        assert_eq!(direct.branch.as_deref(), Some("main"));

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[0].contains("GET /repos/org/repo/commits/prsha/check-runs?"));
        assert!(requests[1].contains("GET /repos/org/repo/actions/runs?"));
        assert!(requests[1].contains("head_sha=prsha"));
        assert!(requests[2].contains("GET /repos/org/repo/actions/runs?"));
        assert!(requests[2].contains("branch=main"));
        assert!(requests[2].contains("event=push"));
    }

    #[test]
    fn initial_ci_detection_emits_started_event_with_route_metadata() {
        let repo = GitRepoMonitor {
            path: "/tmp/clawhip".into(),
            name: Some("clawhip".into()),
            channel: Some("dev-channel".into()),
            mention: Some("<@123>".into()),
            format: Some(MessageFormat::Alert),
            ..GitRepoMonitor::default()
        };
        let previous = HashMap::new();
        let current_ci = ci_snapshot(58, "CI / test", "in_progress", None);
        let current = [(current_ci.dedupe_key(), current_ci)]
            .into_iter()
            .collect();

        let events = collect_ci_events(&repo, "clawhip", false, &previous, &current, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.ci-started");
        assert_eq!(events[0].channel.as_deref(), Some("dev-channel"));
        assert_eq!(events[0].mention.as_deref(), Some("<@123>"));
        assert_eq!(events[0].format, Some(MessageFormat::Alert));
        assert_eq!(events[0].payload["repo"], json!("clawhip"));
        assert_eq!(events[0].payload["number"], json!(58));
        assert_eq!(events[0].payload["workflow"], json!("CI / test"));
        assert_eq!(events[0].payload["status"], json!("in_progress"));
        assert_eq!(events[0].payload["sha"], json!("abcdef1234567890"));
        assert_eq!(
            events[0].payload["url"],
            json!("https://github.com/Yeachan-Heo/clawhip/actions/runs/1")
        );
    }

    #[test]
    fn initial_terminal_ci_detection_is_suppressed_as_baseline() {
        let repo = GitRepoMonitor {
            path: "/tmp/clawhip".into(),
            ..GitRepoMonitor::default()
        };
        for conclusion in ["success", "failure", "cancelled"] {
            let previous = HashMap::new();
            let current_ci = ci_snapshot(58, "CI / test", "completed", Some(conclusion));
            let current = [(current_ci.dedupe_key(), current_ci)]
                .into_iter()
                .collect();

            let events = collect_ci_events(&repo, "clawhip", false, &previous, &current, None);
            assert!(
                events.is_empty(),
                "initial completed CI with conclusion {conclusion} should only seed the baseline"
            );
        }
    }

    #[test]
    fn absent_terminal_ci_after_baseline_emits_completion_events() {
        let repo = GitRepoMonitor {
            path: "/tmp/clawhip".into(),
            ..GitRepoMonitor::default()
        };

        for (conclusion, expected_kind) in [
            ("failure", "github.ci-failed"),
            ("success", "github.ci-passed"),
            ("cancelled", "github.ci-cancelled"),
        ] {
            let previous = HashMap::new();
            let current_ci = ci_snapshot(58, "CI / test", "completed", Some(conclusion));
            let current = [(current_ci.dedupe_key(), current_ci)]
                .into_iter()
                .collect();

            let events = collect_ci_events(&repo, "clawhip", true, &previous, &current, None);
            assert_eq!(
                events.len(),
                1,
                "completed CI with conclusion {conclusion} should emit after baseline"
            );
            assert_eq!(events[0].canonical_kind(), expected_kind);
            assert_eq!(events[0].payload["status"], json!("completed"));
            assert_eq!(events[0].payload["conclusion"], json!(conclusion));
        }
    }

    #[test]
    fn unchanged_ci_state_is_suppressed() {
        let repo = GitRepoMonitor {
            path: "/tmp/clawhip".into(),
            ..GitRepoMonitor::default()
        };
        let ci = ci_snapshot(58, "CI / test", "in_progress", None);
        let previous = [(ci.dedupe_key(), ci.clone())].into_iter().collect();
        let current = [(ci.dedupe_key(), ci)].into_iter().collect();

        let events = collect_ci_events(&repo, "clawhip", true, &previous, &current, None);
        assert!(events.is_empty());
    }

    #[test]
    fn ci_state_transition_to_failed_emits_failed_event() {
        let repo = GitRepoMonitor {
            path: "/tmp/clawhip".into(),
            ..GitRepoMonitor::default()
        };
        let previous_ci = ci_snapshot(58, "CI / test", "in_progress", None);
        let current_ci = ci_snapshot(58, "CI / test", "completed", Some("failure"));
        let previous = [(previous_ci.dedupe_key(), previous_ci)]
            .into_iter()
            .collect();
        let current = [(current_ci.dedupe_key(), current_ci)]
            .into_iter()
            .collect();

        let events = collect_ci_events(&repo, "clawhip", true, &previous, &current, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.ci-failed");
        assert_eq!(events[0].payload["workflow"], json!("CI / test"));
        assert_eq!(events[0].payload["status"], json!("completed"));
        assert_eq!(events[0].payload["conclusion"], json!("failure"));
    }

    #[test]
    fn ci_state_transition_to_passed_emits_passed_event() {
        let repo = GitRepoMonitor {
            path: "/tmp/clawhip".into(),
            ..GitRepoMonitor::default()
        };
        let previous_ci = ci_snapshot(58, "CI / test", "in_progress", None);
        let current_ci = ci_snapshot(58, "CI / test", "completed", Some("success"));
        let previous = [(previous_ci.dedupe_key(), previous_ci)]
            .into_iter()
            .collect();
        let current = [(current_ci.dedupe_key(), current_ci)]
            .into_iter()
            .collect();

        let events = collect_ci_events(&repo, "clawhip", true, &previous, &current, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.ci-passed");
    }

    #[test]
    fn ci_state_transition_to_cancelled_emits_cancelled_event() {
        let repo = GitRepoMonitor {
            path: "/tmp/clawhip".into(),
            ..GitRepoMonitor::default()
        };
        let previous_ci = ci_snapshot(58, "CI / test", "in_progress", None);
        let current_ci = ci_snapshot(58, "CI / test", "completed", Some("cancelled"));
        let previous = [(previous_ci.dedupe_key(), previous_ci)]
            .into_iter()
            .collect();
        let current = [(current_ci.dedupe_key(), current_ci)]
            .into_iter()
            .collect();

        let events = collect_ci_events(&repo, "clawhip", true, &previous, &current, None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.ci-cancelled");
    }

    #[tokio::test]
    async fn github_client_includes_bearer_auth_when_token_configured() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n[]")
                .await
                .unwrap();
            req
        });

        let client = build_github_client(Some("secret-token".into())).unwrap();
        let _ = client
            .http
            .get(format!("http://{}/repos/x/y/pulls", addr))
            .send()
            .await
            .unwrap();
        let req = server.await.unwrap();
        assert!(
            req.contains("Authorization: Bearer secret-token")
                || req.contains("authorization: Bearer secret-token")
        );
    }

    #[tokio::test]
    async fn snapshot_falls_back_to_configured_github_repo_without_local_clone() {
        let repo = GitRepoMonitor {
            path: "/tmp/clawhip-test-private-repo-missing".into(),
            name: Some("private-repo".into()),
            github_repo: Some("owner/private-repo".into()),
            ..GitRepoMonitor::default()
        };

        let snapshot = snapshot_github_repo(&repo).await.unwrap();

        assert_eq!(snapshot.repo_name, "private-repo");
        assert_eq!(snapshot.github_repo.as_deref(), Some("owner/private-repo"));
    }

    #[tokio::test]
    async fn source_loop_survives_transient_github_api_errors() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_for_server = request_count.clone();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            let responses = [
                "HTTP/1.1 500 Internal Server Error\r\ncontent-type: text/plain\r\ncontent-length: 4\r\n\r\nboom",
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n[]",
            ];

            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0_u8; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                requests.push(String::from_utf8_lossy(&buf[..n]).to_string());
                request_count_for_server.fetch_add(1, Ordering::SeqCst);
                stream.write_all(response.as_bytes()).await.unwrap();
            }

            requests
        });

        let mut config = AppConfig::default();
        config.monitors.poll_interval_secs = 1;
        config.monitors.github_api_base = format!("http://{addr}");
        config.monitors.git.repos = vec![GitRepoMonitor {
            path: "/tmp/clawhip-test-private-repo-missing".into(),
            name: Some("private-repo".into()),
            github_repo: Some("owner/private-repo".into()),
            emit_commits: false,
            emit_branch_changes: false,
            emit_issue_opened: true,
            emit_pr_status: false,
            ..GitRepoMonitor::default()
        }];

        let baseline_dir = tempfile::tempdir().unwrap();
        let source = GitHubSource::with_ci_baseline_path(
            Arc::new(config),
            baseline_dir.path().join("github-ci-baseline.json"),
        );
        let (tx, _rx) = mpsc::channel(4);
        let source_task = tokio::spawn(async move { source.run(tx).await });

        tokio::time::timeout(Duration::from_secs(5), async {
            while request_count.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert!(
            !source_task.is_finished(),
            "GitHub source loop exited after a transient API failure"
        );

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|request| {
            request.contains("GET /repos/owner/private-repo/issues?")
                || request.contains("GET /repos/owner/private-repo/issues ")
        }));

        source_task.abort();
        let _ = source_task.await;
    }
    include!("github_pr_activity_metadata_tests.rs");

    type ActivityResponse = (
        String,
        serde_json::Value,
        u16,
        Option<String>,
        Option<String>,
    );

    fn activity_response(path: &str, body: serde_json::Value) -> ActivityResponse {
        (path.to_string(), body, 200, None, None)
    }

    async fn activity_server(
        responses: Vec<ActivityResponse>,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let server_base = base.clone();
        let server = tokio::spawn(async move {
            for (path, body, status, next, date) in responses {
                let (mut stream, _) =
                    tokio::time::timeout(Duration::from_secs(5), listener.accept())
                        .await
                        .expect("expected request was not made")
                        .unwrap();
                let mut buffer = vec![0; 8192];
                let n = stream.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..n]).to_string();
                let target = request.split_whitespace().nth(1).unwrap();
                assert_eq!(target.split('?').next().unwrap(), path, "{request}");
                captured.lock().unwrap().push(target.to_string());
                let body = body.to_string();
                let link = next
                    .map(|next| format!("link: <{server_base}{next}>; rel=\"next\"\r\n"))
                    .unwrap_or_default();
                let date = date
                    .map(|date| format!("date: {date}\r\n"))
                    .unwrap_or_default();
                let response = format!(
                    "HTTP/1.1 {status} Mock\r\ncontent-type: application/json\r\nconnection: close\r\n{link}{date}content-length: {}\r\n\r\n{body}",
                    body.len(),
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "unexpected extra GitHub API request"
            );
        });
        (base, requests, server)
    }

    fn activity_pr(number: u64, updated_at: &str) -> serde_json::Value {
        json!({"number": number, "title": "A PR", "state": "open", "html_url": "https://github.com/org/repo/pull/1",
            "merged_at": null, "head": {"ref": "feature", "sha": "abc"}, "updated_at": updated_at,
            "user": {"login": "pr-owner"}})
    }

    fn activity_comment(
        id: u64,
        number: u64,
        created_at: &str,
        updated_at: &str,
    ) -> serde_json::Value {
        json!({"id": id, "issue_url": format!("https://api.github.com/repos/org/repo/issues/{number}"),
            "pull_request_url": format!("https://api.github.com/repos/org/repo/pulls/{number}"),
            "html_url": format!("https://github.com/org/repo/pull/{number}#comment-{id}"),
            "created_at": created_at, "updated_at": updated_at, "user": {"login": "commenter"},
            "path": "src/main.rs", "body": "private comment text"})
    }

    fn activity_issue_comment(
        id: u64,
        issue_number: u64,
        created_at: &str,
        updated_at: &str,
    ) -> serde_json::Value {
        json!({"id": id, "issue_url": format!("https://api.github.com/repos/org/repo/issues/{issue_number}"),
            "pull_request_url": format!("https://api.github.com/repos/org/repo/pulls/{issue_number}"),
            "html_url": format!("https://github.com/org/repo/issues/{issue_number}#issuecomment-{id}"),
            "created_at": created_at, "updated_at": updated_at, "user": {"login": "commenter"},
            "path": "src/main.rs", "body": "private comment text"})
    }

    fn activity_review(id: u64, state: &str, submitted_at: &str) -> serde_json::Value {
        json!({"id": id, "state": state, "submitted_at": submitted_at,
            "user": {"login": "reviewer"}, "html_url": format!("https://github.com/org/repo/pull/1#review-{id}"),
            "body": "private review text"})
    }

    fn activity_poll_responses(
        conversations: serde_json::Value,
        prs: serde_json::Value,
        inline: serde_json::Value,
    ) -> Vec<ActivityResponse> {
        vec![
            activity_response("/repos/org/repo/pulls", prs),
            activity_response("/repos/org/repo/issues", json!([])),
            activity_response("/repos/org/repo/issues/comments", conversations),
            activity_response("/repos/org/repo/pulls/comments", inline),
        ]
    }

    // Fresh comments resolve only after both feeds have joined the pending FIFO.
    fn activity_lookup_after_feed(
        responses: &mut Vec<ActivityResponse>,
        feed: &str,
        occurrence: usize,
        numbers: &[u64],
    ) {
        let mut index = responses
            .iter()
            .enumerate()
            .filter(|(_, response)| response.0 == feed)
            .nth(occurrence)
            .unwrap()
            .0
            + 1;
        if feed == "/repos/org/repo/issues/comments" {
            index += responses[index..]
                .iter()
                .position(|response| response.0 == "/repos/org/repo/pulls/comments")
                .expect("inline feed follows conversation feed");
            index += 1;
        }
        while responses.get(index).is_some_and(|response| {
            response.0 == "/repos/org/repo/pulls/comments"
                || (response.0.starts_with("/repos/org/repo/pulls/")
                    && !response.0.ends_with("/reviews"))
        }) {
            index += 1;
        }
        for number in numbers {
            responses.insert(
                index,
                activity_response(
                    &format!("/repos/org/repo/pulls/{number}"),
                    activity_pr(*number, ACTIVITY_T0),
                ),
            );
            index += 1;
        }
    }

    // Existing pending work resolves before the comment feed requests.
    fn activity_lookup_before_feeds(
        responses: &mut Vec<ActivityResponse>,
        occurrence: usize,
        numbers: &[u64],
    ) {
        let index = responses
            .iter()
            .enumerate()
            .filter(|(_, response)| response.0 == "/repos/org/repo/issues/comments")
            .nth(occurrence)
            .unwrap()
            .0;
        for number in numbers.iter().rev() {
            responses.insert(
                index,
                activity_response(
                    &format!("/repos/org/repo/pulls/{number}"),
                    activity_pr(*number, ACTIVITY_T0),
                ),
            );
        }
    }

    struct ActivityHarness {
        config: AppConfig,
        client: GitHubClient,
        state: HashMap<String, GitHubRepoState>,
        baseline: CIBaseline,
        baseline_path: Option<PathBuf>,
        tx: mpsc::Sender<IncomingEvent>,
        rx: mpsc::Receiver<IncomingEvent>,
    }

    impl ActivityHarness {
        fn new(base: String) -> Self {
            telemetry::take_test_records();
            let mut config = AppConfig::default();
            config.monitors.github_api_base = base;
            config.monitors.git.repos = vec![GitRepoMonitor {
                path: concat!(env!("CARGO_MANIFEST_DIR"), "/missing-pr-activity-repo").into(),
                name: Some("repo".into()),
                github_repo: Some("org/repo".into()),
                emit_pr_activity: true,
                emit_commits: false,
                emit_branch_changes: false,
                ..GitRepoMonitor::default()
            }];
            let (tx, rx) = mpsc::channel(3000);
            Self {
                config,
                client: build_github_client(None).unwrap(),
                state: HashMap::new(),
                baseline: CIBaseline::default(),
                baseline_path: None,
                tx,
                rx,
            }
        }

        async fn poll(&mut self) -> Vec<IncomingEvent> {
            poll_github(
                &self.config,
                Some(&self.client),
                &self.tx,
                &mut self.state,
                &mut self.baseline,
                self.baseline_path.as_deref(),
            )
            .await
            .unwrap();
            let mut events = Vec::new();
            while let Ok(event) = self.rx.try_recv() {
                events.push(event);
            }
            events
        }

        fn repo_state(&self) -> &GitHubRepoState {
            &self.state[&self.config.monitors.git.repos[0].path]
        }

        fn assert_degraded(&self, error_fragment: &str) {
            let records = telemetry::take_test_records();
            assert!(
                records.iter().any(|record| {
                    record["telemetry_event"] == "source_degraded"
                        && record["reason_code"] == "source_poll_failed"
                        && record["repo_path"] == self.config.monitors.git.repos[0].path
                        && record["error"]
                            .as_str()
                            .is_some_and(|error| error.contains(error_fragment))
                }),
                "missing degradation {error_fragment}: {records:?}"
            );
        }
    }

    const ACTIVITY_T0: &str = "2030-01-01T00:00:00Z";
    const ACTIVITY_T1: &str = "2030-01-01T00:01:00Z";
    const ACTIVITY_T2: &str = "2030-01-01T00:02:00Z";

    fn activity_scan_responses() -> Vec<ActivityResponse> {
        vec![
            activity_response("/repos/org/repo/issues", json!([])),
            activity_response("/repos/org/repo/issues/comments", json!([])),
            activity_response("/repos/org/repo/pulls/comments", json!([])),
        ]
    }

    fn activity_review_baseline_responses() -> Vec<ActivityResponse> {
        let mut responses = activity_poll_responses(
            json!([]),
            json!([activity_pr(1, ACTIVITY_T0)]),
            json!([activity_comment(0, 1, ACTIVITY_T0, ACTIVITY_T0)]),
        );
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([]),
        ));
        responses
    }

    fn activity_second(offset: u64) -> String {
        format!("2030-01-01T00:{:02}:{:02}Z", offset / 60, offset % 60)
    }

    #[tokio::test]
    async fn pr_activity_flag_off_keeps_requests_and_drops_pr_conversations() {
        let mut responses = Vec::new();
        for comments in [1, 2] {
            responses.push(activity_response(
                "/repos/org/repo/issues",
                json!([
                    {"number": 10, "title": "An issue", "state": "open", "comments": comments},
                    {"number": 1, "title": "A PR", "state": "open", "comments": comments,
                     "pull_request": {"url": "https://api.github.com/repos/org/repo/pulls/1"}}
                ]),
            ));
        }
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        harness.config.monitors.git.repos[0].emit_pr_activity = false;
        assert!(harness.poll().await.is_empty());
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.issue-commented");
        assert_eq!(events[0].payload["number"], 10);
        assert_eq!(
            *requests.lock().unwrap(),
            ["/repos/org/repo/issues?state=all&per_page=100"; 2],
            "flag-off must preserve dev's exact request paths and count"
        );
        assert!(harness.repo_state().activity.review_watermark.is_none());
        assert!(harness.repo_state().activity.pr_comment_cursor.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_conversation_emitted_once_with_pr_author() {
        let prs = json!([activity_pr(1, ACTIVITY_T0)]);
        let mut responses = activity_poll_responses(
            json!([activity_comment(1, 1, ACTIVITY_T0, ACTIVITY_T0)]),
            prs.clone(),
            json!([]),
        );
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([]),
        ));
        for _ in 0..2 {
            responses.extend(activity_poll_responses(
                json!([activity_comment(2, 2, ACTIVITY_T1, ACTIVITY_T1)]),
                json!([activity_pr(2, ACTIVITY_T1)]),
                json!([]),
            ));
            responses.push(activity_response(
                "/repos/org/repo/pulls/2/reviews",
                json!([]),
            ));
        }
        responses.retain(|response| response.0 != "/repos/org/repo/issues");
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        harness.config.monitors.git.repos[0].emit_issue_opened = false;
        assert!(harness.poll().await.is_empty());
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.pr-commented");
        assert_eq!(
            events[0].payload,
            json!({"repo": "repo", "number": 2, "title": "A PR",
            "author": "commenter", "pr_author": "pr-owner", "comment_id": 2,
            "url": "https://github.com/org/repo/pull/2#comment-2"})
        );
        let router = Router::new(Arc::new(AppConfig {
            defaults: DefaultsConfig {
                channel: Some("fallback".into()),
                ..DefaultsConfig::default()
            },
            routes: vec![RouteRule {
                event: "github.*".into(),
                channel: Some("my-prs".into()),
                filter: [("pr_author".to_string(), "pr-owner".to_string())]
                    .into_iter()
                    .collect(),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        }));
        assert_eq!(router.preview(&events[0]).await.unwrap().0, "my-prs");
        let mut other_pr = events[0].clone();
        other_pr.payload["pr_author"] = json!("another-owner");
        assert_eq!(router.preview(&other_pr).await.unwrap().0, "fallback");
        assert!(harness.poll().await.is_empty());
        server.await.unwrap();
    }

    fn activity_comment_poll(inline: bool, comments: serde_json::Value) -> Vec<ActivityResponse> {
        if inline {
            activity_poll_responses(json!([]), json!([]), comments)
        } else {
            activity_poll_responses(comments, json!([]), json!([]))
        }
    }

    fn activity_comment_feed(inline: bool) -> &'static str {
        if inline {
            "/repos/org/repo/pulls/comments"
        } else {
            "/repos/org/repo/issues/comments"
        }
    }

    async fn assert_pr_activity_old_pr_metadata_cached(inline: bool) {
        let mut responses = activity_comment_poll(
            inline,
            json!([activity_comment(1, 6763, ACTIVITY_T0, ACTIVITY_T0)]),
        );
        for comments in [
            json!([activity_comment(2, 6763, ACTIVITY_T1, ACTIVITY_T1)]),
            json!([
                activity_comment(2, 6763, ACTIVITY_T1, ACTIVITY_T1),
                activity_comment(3, 6763, ACTIVITY_T2, ACTIVITY_T2)
            ]),
            json!([activity_comment(3, 6763, ACTIVITY_T2, ACTIVITY_T2)]),
        ] {
            responses.extend(activity_comment_poll(inline, comments));
        }
        activity_lookup_after_feed(&mut responses, activity_comment_feed(inline), 1, &[6763]);
        let lookup = responses
            .iter_mut()
            .find(|response| response.0 == "/repos/org/repo/pulls/6763")
            .unwrap();
        lookup.1["user"]["login"] = json!("najy97");
        lookup.1["title"] = json!("Old merged PR");
        lookup.1["merged_at"] = json!(ACTIVITY_T0);
        lookup.1["state"] = json!("closed");
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            requests.lock().unwrap().len(),
            4,
            "baseline must not look up seeded comments"
        );
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(
            event.canonical_kind(),
            if inline {
                "github.pr-review-commented"
            } else {
                "github.pr-commented"
            }
        );
        assert_eq!(event.payload["number"], 6763);
        assert_eq!(event.payload["title"], "Old merged PR");
        assert_eq!(event.payload["pr_author"], "najy97");
        assert_eq!(event.payload["author"], "commenter");
        if inline {
            assert_eq!(event.payload["path"], "src/main.rs");
        }
        let router = Router::new(Arc::new(AppConfig {
            routes: vec![RouteRule {
                event: "github.*".into(),
                channel: Some("my-prs".into()),
                filter: [("pr_author".to_string(), "najy97".to_string())]
                    .into_iter()
                    .collect(),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        }));
        assert_eq!(router.preview(event).await.unwrap().0, "my-prs");
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["comment_id"], 3);
        assert_eq!(events[0].payload["title"], "Old merged PR");
        assert_eq!(events[0].payload["pr_author"], "najy97");
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|path| path.as_str() == "/repos/org/repo/pulls/6763")
                .count(),
            1
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_old_conversation_metadata_cached_and_routes_by_pr_author() {
        assert_pr_activity_old_pr_metadata_cached(false).await;
    }

    #[tokio::test]
    async fn pr_activity_old_inline_metadata_cached_and_routes_by_pr_author() {
        assert_pr_activity_old_pr_metadata_cached(true).await;
    }

    #[tokio::test]
    async fn pr_activity_metadata_failure_retries_without_duplicates() {
        for (inline, number) in [(false, 99), (true, 100)] {
            let endpoint = format!("/repos/org/repo/pulls/{number}");
            for missing_author in [false, true] {
                let mut responses = activity_comment_poll(
                    inline,
                    json!([activity_comment(1, number, ACTIVITY_T0, ACTIVITY_T0)]),
                );
                for _ in 0..3 {
                    responses.extend(activity_comment_poll(
                        inline,
                        json!([activity_comment(2, number, ACTIVITY_T1, ACTIVITY_T1)]),
                    ));
                }
                activity_lookup_before_feeds(&mut responses, 2, &[number]);
                activity_lookup_after_feed(
                    &mut responses,
                    activity_comment_feed(inline),
                    1,
                    &[number],
                );
                let failure = responses
                    .iter_mut()
                    .find(|response| response.0 == endpoint)
                    .unwrap();
                if missing_author {
                    failure.1["user"] = json!(null);
                } else {
                    failure.1 = json!({"message": "failed"});
                    failure.2 = 500;
                }
                let (base, requests, server) = activity_server(responses).await;
                let mut harness = ActivityHarness::new(base);
                assert!(harness.poll().await.is_empty());
                assert!(harness.poll().await.is_empty());
                harness
                    .assert_degraded(&format!("GitHub PR metadata lookup failed for #{number}:"));
                let activity = &harness.repo_state().activity;
                let (cursor, lower, ids) = if inline {
                    (
                        &activity.review_comment_cursor,
                        &activity.review_comment_lower,
                        &activity.emitted_review_comment_ids,
                    )
                } else {
                    (
                        &activity.pr_comment_cursor,
                        &activity.pr_comment_lower,
                        &activity.emitted_pr_comment_ids,
                    )
                };
                assert_eq!(cursor.as_deref(), Some(ACTIVITY_T1));
                assert_eq!(lower.as_deref(), Some(ACTIVITY_T1));
                assert!(ids.contains(&2));
                assert_eq!(activity.pending_comments.len(), 1);
                assert_eq!(activity.pending_comments[0].attempts, 1);
                let events = harness.poll().await;
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].payload["comment_id"], 2);
                assert_eq!(events[0].payload["title"], "A PR");
                assert_eq!(events[0].payload["pr_author"], "pr-owner");
                assert!(harness.poll().await.is_empty());
                assert_eq!(
                    requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|path| *path == &endpoint)
                        .count(),
                    2
                );
                server.await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn pr_activity_metadata_rate_limit_uses_shared_backoff_and_retains_comment() {
        for status in [403, 429] {
            let mut responses = activity_comment_poll(
                false,
                json!([activity_comment(1, 99, ACTIVITY_T0, ACTIVITY_T0)]),
            );
            let limited = activity_comment_poll(
                false,
                json!([activity_comment(2, 99, ACTIVITY_T1, ACTIVITY_T1)]),
            );
            // Both feeds advance before the lookup; subsequent polls back off.
            responses.extend(limited);
            let mut lookup =
                activity_response("/repos/org/repo/pulls/99", json!({"message": "limited"}));
            lookup.2 = status;
            responses.push(lookup);
            let mut recovered = activity_comment_poll(
                false,
                json!([activity_comment(2, 99, ACTIVITY_T1, ACTIVITY_T1)]),
            );
            activity_lookup_before_feeds(&mut recovered, 0, &[99]);
            responses.extend(recovered);
            responses.extend(activity_comment_poll(
                false,
                json!([activity_comment(2, 99, ACTIVITY_T1, ACTIVITY_T1)]),
            ));
            let (base, requests, server) = activity_server(responses).await;
            let mut harness = ActivityHarness::new(base);
            assert!(harness.poll().await.is_empty());
            assert!(harness.poll().await.is_empty());
            harness.assert_degraded("GitHub PR metadata lookup failed for #99:");
            assert!(harness.client.backoff.lock().unwrap().until.is_some());
            assert!(harness.poll().await.is_empty());
            assert_eq!(requests.lock().unwrap().len(), 9);
            let activity = &harness.repo_state().activity;
            assert_eq!(activity.pr_comment_cursor.as_deref(), Some(ACTIVITY_T1));
            assert_eq!(activity.pr_comment_lower.as_deref(), Some(ACTIVITY_T1));
            assert!(activity.emitted_pr_comment_ids.contains(&2));
            assert_eq!(activity.pending_comments.len(), 1);
            assert_eq!(activity.pending_comments[0].attempts, 0);
            // Advance only the backoff clock; recovery must use the real HTTP/poll path.
            harness.client.backoff.lock().unwrap().until = Some(std::time::Instant::now());
            let recovered = harness.poll().await;
            assert_eq!(recovered.len(), 1);
            assert_eq!(recovered[0].payload["comment_id"], 2);
            assert_eq!(recovered[0].payload["pr_author"], "pr-owner");
            assert!(harness.repo_state().activity.pending_comments.is_empty());
            assert!(harness.poll().await.is_empty());
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn pr_activity_metadata_budget_shared_across_feeds_retries_all_once() {
        let count = MAX_PR_METADATA_LOOKUPS + 5;
        let conversations: Vec<_> = (1..=15)
            .map(|id| activity_comment(id, id, ACTIVITY_T1, ACTIVITY_T1))
            .collect();
        let inline: Vec<_> = (16..=count as u64)
            .map(|id| activity_comment(id, id, ACTIVITY_T1, ACTIVITY_T1))
            .collect();
        let mut responses = activity_poll_responses(
            json!([activity_comment(0, 1, ACTIVITY_T0, ACTIVITY_T0)]),
            json!([]),
            json!([activity_comment(0, 1, ACTIVITY_T0, ACTIVITY_T0)]),
        );
        for _ in 0..3 {
            responses.extend(activity_poll_responses(
                json!(conversations),
                json!([]),
                json!(inline),
            ));
        }
        activity_lookup_after_feed(
            &mut responses,
            "/repos/org/repo/issues/comments",
            1,
            &(1..=15).collect::<Vec<_>>(),
        );
        activity_lookup_after_feed(
            &mut responses,
            "/repos/org/repo/pulls/comments",
            1,
            &(16..=MAX_PR_METADATA_LOOKUPS as u64).collect::<Vec<_>>(),
        );
        activity_lookup_before_feeds(
            &mut responses,
            2,
            &((MAX_PR_METADATA_LOOKUPS + 1) as u64..=count as u64).collect::<Vec<_>>(),
        );
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let mut events = harness.poll().await;
        assert_eq!(events.len(), MAX_PR_METADATA_LOOKUPS);
        harness.assert_degraded("GitHub PR metadata lookup budget exhausted");
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_cursor
                .as_deref(),
            Some(ACTIVITY_T1)
        );
        let first_poll_requests = requests.lock().unwrap().len();
        assert_eq!(first_poll_requests, 8 + MAX_PR_METADATA_LOOKUPS);
        events.extend(harness.poll().await);
        assert_eq!(events.len(), count);
        assert_eq!(
            events
                .iter()
                .map(|event| event.payload["comment_id"].as_u64().unwrap())
                .collect::<HashSet<_>>(),
            (1..=count as u64).collect()
        );
        for event in events {
            assert_eq!(event.payload["pr_author"], "pr-owner");
            assert_eq!(event.payload["title"], "A PR");
        }
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_cursor
                .as_deref(),
            Some(ACTIVITY_T1)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_metadata_404_emits_empty_once_and_caches_tombstone() {
        for inline in [false, true] {
            let mut responses = activity_comment_poll(
                inline,
                json!([activity_comment(1, 99, ACTIVITY_T0, ACTIVITY_T0)]),
            );
            for comments in [
                json!([activity_comment(2, 99, ACTIVITY_T1, ACTIVITY_T1)]),
                json!([activity_comment(2, 99, ACTIVITY_T1, ACTIVITY_T1)]),
                json!([activity_comment(3, 99, ACTIVITY_T2, ACTIVITY_T2)]),
            ] {
                responses.extend(activity_comment_poll(inline, comments));
            }
            activity_lookup_after_feed(&mut responses, activity_comment_feed(inline), 1, &[99]);
            let missing = responses
                .iter_mut()
                .find(|response| response.0 == "/repos/org/repo/pulls/99")
                .unwrap();
            missing.1 = json!({"message": "Not Found"});
            missing.2 = 404;
            let (base, requests, server) = activity_server(responses).await;
            let mut harness = ActivityHarness::new(base);
            assert!(harness.poll().await.is_empty());
            let events = harness.poll().await;
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].payload["comment_id"], 2);
            assert_eq!(events[0].payload["title"], "");
            assert_eq!(events[0].payload["pr_author"], "");
            assert!(harness.poll().await.is_empty());
            let events = harness.poll().await;
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].payload["comment_id"], 3);
            assert_eq!(events[0].payload["pr_author"], "");
            assert_eq!(
                requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|path| path.as_str() == "/repos/org/repo/pulls/99")
                    .count(),
                1
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn pr_activity_metadata_window_refreshes_cache_without_detail_lookup() {
        let mut responses = activity_comment_poll(
            false,
            json!([activity_comment(0, 7, ACTIVITY_T0, ACTIVITY_T0)]),
        );
        responses.extend(activity_comment_poll(
            false,
            json!([activity_comment(1, 7, ACTIVITY_T1, ACTIVITY_T1)]),
        ));
        let mut updated = activity_pr(7, ACTIVITY_T2);
        updated["title"] = json!("Renamed PR");
        responses.extend(activity_poll_responses(
            json!([]),
            json!([updated]),
            json!([]),
        ));
        responses.push(activity_response(
            "/repos/org/repo/pulls/7/reviews",
            json!([]),
        ));
        responses.extend(activity_comment_poll(
            false,
            json!([activity_comment(2, 7, ACTIVITY_T2, ACTIVITY_T2)]),
        ));
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/issues/comments", 1, &[7]);
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert_eq!(harness.poll().await[0].payload["title"], "A PR");
        assert!(harness.poll().await.is_empty());
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["title"], "Renamed PR");
        assert_eq!(events[0].payload["pr_author"], "pr-owner");
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|path| path.as_str() == "/repos/org/repo/pulls/7")
                .count(),
            1
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_metadata_cache_fifo_is_bounded_and_evicted_pr_refetches() {
        let count = MAX_PR_METADATA_CACHE + 1;
        let mut responses = activity_comment_poll(
            false,
            json!([activity_comment(0, 1, ACTIVITY_T0, ACTIVITY_T0)]),
        );
        let numbers: Vec<_> = (1..=count as u64).collect();
        for (poll, chunk) in numbers.chunks(MAX_PR_METADATA_LOOKUPS).enumerate() {
            let comments: Vec<_> = chunk
                .iter()
                .map(|number| activity_comment(*number, *number, ACTIVITY_T1, ACTIVITY_T1))
                .collect();
            responses.extend(activity_comment_poll(false, json!(comments)));
            activity_lookup_after_feed(
                &mut responses,
                "/repos/org/repo/issues/comments",
                poll + 1,
                chunk,
            );
        }
        responses.extend(activity_comment_poll(
            false,
            json!([activity_comment(
                count as u64 + 1,
                1,
                ACTIVITY_T2,
                ACTIVITY_T2
            )]),
        ));
        activity_lookup_after_feed(
            &mut responses,
            "/repos/org/repo/issues/comments",
            numbers.chunks(MAX_PR_METADATA_LOOKUPS).len() + 1,
            &[1],
        );
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        for chunk in numbers.chunks(MAX_PR_METADATA_LOOKUPS) {
            assert_eq!(harness.poll().await.len(), chunk.len());
            let activity = &harness.repo_state().activity;
            assert!(activity.metadata.len() <= MAX_PR_METADATA_CACHE);
            assert_eq!(activity.metadata.len(), activity.metadata_order.len());
        }
        assert!(!harness.repo_state().activity.metadata.contains_key(&1));
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["pr_author"], "pr-owner");
        assert_eq!(
            harness.repo_state().activity.metadata.len(),
            MAX_PR_METADATA_CACHE
        );
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|path| path.as_str() == "/repos/org/repo/pulls/1")
                .count(),
            2
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_metadata_uses_active_review_scan_before_lookup() {
        let prs: Vec<_> = (1..=21)
            .map(|number| activity_pr(number, ACTIVITY_T0))
            .collect();
        let mut responses = activity_poll_responses(
            json!([activity_comment(0, 21, ACTIVITY_T0, ACTIVITY_T0)]),
            json!(prs),
            json!([]),
        );
        for number in 1..=20 {
            responses.push(activity_response(
                &format!("/repos/org/repo/pulls/{number}/reviews"),
                json!([]),
            ));
        }
        let mut scan = activity_scan_responses();
        scan[1].1 = json!([activity_comment(1, 21, ACTIVITY_T1, ACTIVITY_T1)]);
        scan[2].1 = json!([activity_comment(2, 99, ACTIVITY_T1, ACTIVITY_T1)]);
        responses.extend(scan);
        responses.push(activity_response(
            "/repos/org/repo/pulls/21/reviews",
            json!([]),
        ));
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/pulls/comments", 1, &[99]);
        responses.extend(activity_poll_responses(
            json!([activity_comment(1, 21, ACTIVITY_T1, ACTIVITY_T1)]),
            json!([]),
            json!([activity_comment(2, 99, ACTIVITY_T1, ACTIVITY_T1)]),
        ));
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert!(harness.repo_state().activity.review_scan.is_some());
        let events = harness.poll().await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload["number"], 21);
        assert_eq!(events[1].payload["number"], 99);
        for event in events {
            assert_eq!(event.payload["pr_author"], "pr-owner");
        }
        assert!(harness.poll().await.is_empty());
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|path| path == "/repos/org/repo/pulls/21")
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_inline_baseline_new_dedup_and_old_edit_outside_pr_window() {
        let mut responses = activity_poll_responses(
            json!([]),
            json!([]),
            json!([activity_comment(1, 9999, ACTIVITY_T0, ACTIVITY_T0)]),
        );
        let comments = json!([
            activity_comment(2, 9999, ACTIVITY_T1, ACTIVITY_T1),
            activity_comment(3, 9999, "2029-01-01T00:00:00Z", ACTIVITY_T2)
        ]);
        for _ in 0..2 {
            responses.extend(activity_poll_responses(
                json!([]),
                json!([]),
                comments.clone(),
            ));
        }
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/pulls/comments", 1, &[9999]);
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.pr-review-commented");
        assert_eq!(events[0].payload["number"], 9999);
        assert_eq!(events[0].payload["comment_id"], 2);
        assert_eq!(events[0].payload["path"], "src/main.rs");
        assert_eq!(events[0].payload["title"], "A PR");
        assert_eq!(events[0].payload["pr_author"], "pr-owner");
        assert!(events[0].payload.get("body").is_none());
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_cursor
                .as_deref(),
            Some(ACTIVITY_T2)
        );
        let requests = requests.lock().unwrap().clone();
        let feed = &requests[7];
        assert!(
            feed.contains("sort=updated")
                && feed.contains("direction=asc")
                && feed.contains("per_page=100")
        );
        assert!(feed.contains("since=2029-12-31T23%3A59%3A59"), "{feed}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_conversation_failure_and_page_cap_preserve_lower_bound() {
        let mut responses = activity_poll_responses(
            json!([activity_comment(1, 1, ACTIVITY_T0, ACTIVITY_T0)]),
            json!([]),
            json!([]),
        );
        let mut failed =
            activity_poll_responses(json!({"message": "failed"}), json!([]), json!([]));
        failed[2].2 = 500;
        responses.extend(failed);
        responses.push(activity_response("/repos/org/repo/pulls", json!([])));
        responses.push(activity_response("/repos/org/repo/issues", json!([])));
        for page in 1..=MAX_COMMENT_PAGES {
            let mut response = activity_response(
                "/repos/org/repo/issues/comments",
                json!([activity_comment(2, 1, ACTIVITY_T1, ACTIVITY_T1)]),
            );
            response.3 = Some(format!("/repos/org/repo/issues/comments?page={}", page + 1));
            responses.push(response);
        }
        responses.push(activity_response(
            "/repos/org/repo/pulls/comments",
            json!([]),
        ));
        responses.extend(activity_poll_responses(
            json!([activity_comment(2, 1, ACTIVITY_T1, ACTIVITY_T1)]),
            json!([]),
            json!([]),
        ));
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/issues/comments", 2, &[1]);
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness.repo_state().activity.pr_comment_cursor.as_deref(),
            Some(ACTIVITY_T0)
        );
        assert_eq!(
            harness.repo_state().activity.pr_comment_lower.as_deref(),
            Some(ACTIVITY_T0)
        );
        harness.assert_degraded("GitHub PR conversation comments fetch failed:");
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.pr-commented");
        assert_eq!(
            harness.repo_state().activity.pr_comment_cursor.as_deref(),
            Some(ACTIVITY_T1)
        );
        assert_eq!(
            harness.repo_state().activity.pr_comment_lower.as_deref(),
            Some(ACTIVITY_T0)
        );
        harness.assert_degraded("GitHub PR conversation comment pagination cap reached");
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness.repo_state().activity.pr_comment_lower.as_deref(),
            Some(ACTIVITY_T1)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_inline_failure_preserves_cursor_and_page_cap_advances() {
        let mut responses = activity_poll_responses(
            json!([]),
            json!([]),
            json!([activity_comment(1, 1, ACTIVITY_T0, ACTIVITY_T0)]),
        );
        let mut failed =
            activity_poll_responses(json!([]), json!([]), json!({"message": "failure"}));
        failed.last_mut().unwrap().2 = 500;
        responses.extend(failed);
        let mut capped = activity_poll_responses(
            json!([]),
            json!([]),
            json!([activity_comment(2, 1, ACTIVITY_T1, ACTIVITY_T1)]),
        );
        capped.last_mut().unwrap().3 = Some("/repos/org/repo/pulls/comments?page=2".into());
        responses.extend(capped);
        for page in 2..=MAX_COMMENT_PAGES {
            let mut response = activity_response("/repos/org/repo/pulls/comments", json!([]));
            response.3 = Some(format!("/repos/org/repo/pulls/comments?page={}", page + 1));
            responses.push(response);
        }
        responses.extend(activity_poll_responses(
            json!([]),
            json!([]),
            json!([activity_comment(2, 1, ACTIVITY_T1, ACTIVITY_T1)]),
        ));
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/pulls/comments", 2, &[1]);
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_cursor
                .as_deref(),
            Some(ACTIVITY_T0)
        );
        assert_eq!(harness.poll().await.len(), 1);
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_cursor
                .as_deref(),
            Some(ACTIVITY_T1)
        );
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_cursor
                .as_deref(),
            Some(ACTIVITY_T1)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_reviews_updated_only_states_and_dedup() {
        let mut responses =
            activity_poll_responses(json!([]), json!([activity_pr(1, ACTIVITY_T0)]), json!([]));
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([activity_review(1, "APPROVED", ACTIVITY_T0)]),
        ));
        let reviews = json!([
            activity_review(2, "APPROVED", ACTIVITY_T1), activity_review(3, "CHANGES_REQUESTED", ACTIVITY_T1),
            activity_review(4, "COMMENTED", ACTIVITY_T1), activity_review(5, "PENDING", ACTIVITY_T1),
            activity_review(6, "DISMISSED", ACTIVITY_T1), activity_review(7, "APPROVED", "2029-01-01T00:00:00Z"),
            {"id": 8, "state": "COMMENTED", "submitted_at": null, "user": null, "html_url": ""}
        ]);
        for updated in [ACTIVITY_T1, ACTIVITY_T2] {
            responses.extend(activity_poll_responses(
                json!([]),
                json!([activity_pr(2, updated)]),
                json!([]),
            ));
            responses.push(activity_response(
                "/repos/org/repo/pulls/2/reviews",
                reviews.clone(),
            ));
        }
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let events = harness.poll().await;
        assert_eq!(events.len(), 3);
        for (event, state) in events
            .iter()
            .zip(["approved", "changes_requested", "commented"])
        {
            assert_eq!(event.canonical_kind(), "github.pr-reviewed");
            assert_eq!(event.payload["state"], state);
            assert_eq!(event.payload["pr_author"], "pr-owner");
            assert_eq!(event.payload["author"], "reviewer");
            assert!(event.payload.get("body").is_none());
        }
        assert!(harness.poll().await.is_empty());
        let review_paths: Vec<_> = requests
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.contains("/reviews?"))
            .cloned()
            .collect();
        assert_eq!(review_paths.len(), 3);
        assert!(review_paths[0].starts_with("/repos/org/repo/pulls/1/reviews?"));
        assert!(
            review_paths[1..]
                .iter()
                .all(|path| path.starts_with("/repos/org/repo/pulls/2/reviews?"))
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_review_failure_preserves_cursor_and_retries_without_duplicates() {
        let mut responses =
            activity_poll_responses(json!([]), json!([activity_pr(1, ACTIVITY_T0)]), json!([]));
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([]),
        ));
        let prs = json!([activity_pr(2, ACTIVITY_T1), activity_pr(3, ACTIVITY_T2)]);
        for fail in [true, false] {
            if fail {
                responses.extend(activity_poll_responses(json!([]), prs.clone(), json!([])));
                responses.push(activity_response(
                    "/repos/org/repo/pulls/2/reviews",
                    json!([activity_review(2, "APPROVED", ACTIVITY_T1)]),
                ));
            } else {
                responses.extend(activity_scan_responses());
            }
            let mut response = activity_response(
                "/repos/org/repo/pulls/3/reviews",
                if fail {
                    json!({"message":"failed"})
                } else {
                    json!([activity_review(3, "COMMENTED", ACTIVITY_T2)])
                },
            );
            if fail {
                response.2 = 500;
            }
            responses.push(response);
        }
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["review_id"], 2);
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T0)
        );
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_scan
                .as_ref()
                .unwrap()
                .pending,
            [3]
        );
        harness.assert_degraded("GitHub reviews fetch failed for #3:");
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["review_id"], 3);
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T2)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_updated_pr_page_cap_commits_bounded_window() {
        let mut responses =
            activity_poll_responses(json!([]), json!([activity_pr(1, ACTIVITY_T0)]), json!([]));
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([]),
        ));
        for page in 1..=MAX_CI_PAGES {
            let mut response = activity_response(
                "/repos/org/repo/pulls",
                json!([activity_pr(page as u64 + 1, ACTIVITY_T1)]),
            );
            response.3 = Some(format!("/repos/org/repo/pulls?page={}", page + 1));
            responses.push(response);
        }
        responses.push(activity_response("/repos/org/repo/issues", json!([])));
        responses.push(activity_response(
            "/repos/org/repo/issues/comments",
            json!([]),
        ));
        responses.push(activity_response(
            "/repos/org/repo/pulls/comments",
            json!([]),
        ));
        for number in 2..=6 {
            responses.push(activity_response(
                &format!("/repos/org/repo/pulls/{number}/reviews"),
                json!([activity_review(number, "APPROVED", ACTIVITY_T1)]),
            ));
        }
        let recovered: Vec<_> = (2..=6)
            .map(|number| activity_pr(number, ACTIVITY_T1))
            .collect();
        responses.extend(activity_poll_responses(
            json!([]),
            json!(recovered),
            json!([]),
        ));
        for number in 2..=6 {
            responses.push(activity_response(
                &format!("/repos/org/repo/pulls/{number}/reviews"),
                json!([activity_review(number, "APPROVED", ACTIVITY_T1)]),
            ));
        }
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert_eq!(harness.poll().await.len(), MAX_CI_PAGES);
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T1)
        );
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T1)
        );
        let requests = requests.lock().unwrap().clone();
        assert!(requests[5].contains("sort=updated") && requests[5].contains("direction=desc"));
        assert!(requests[9].contains("page=5"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_updated_pr_pagination_stops_at_cursor() {
        let mut responses =
            activity_poll_responses(json!([]), json!([activity_pr(1, ACTIVITY_T0)]), json!([]));
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([]),
        ));
        let mut first = activity_response(
            "/repos/org/repo/pulls",
            json!([activity_pr(2, ACTIVITY_T1)]),
        );
        first.3 = Some("/repos/org/repo/pulls?page=2".into());
        responses.push(first);
        let mut second = activity_response(
            "/repos/org/repo/pulls",
            json!([
                activity_pr(1, ACTIVITY_T0),
                activity_pr(3, "2029-01-01T00:00:00Z")
            ]),
        );
        second.3 = Some("/repos/org/repo/pulls?page=3".into());
        responses.push(second);
        responses.push(activity_response("/repos/org/repo/issues", json!([])));
        responses.push(activity_response(
            "/repos/org/repo/issues/comments",
            json!([]),
        ));
        responses.push(activity_response(
            "/repos/org/repo/pulls/comments",
            json!([]),
        ));
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([]),
        ));
        responses.push(activity_response(
            "/repos/org/repo/pulls/2/reviews",
            json!([activity_review(2, "APPROVED", ACTIVITY_T1)]),
        ));
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert_eq!(harness.poll().await.len(), 1);
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T1)
        );
        let requests = requests.lock().unwrap().clone();
        assert!(requests[5].contains("page=1") && requests[6].contains("page=2"));
        assert_eq!(requests.len(), 12);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_missing_updated_timestamp_preserves_review_cursor() {
        let mut responses =
            activity_poll_responses(json!([]), json!([activity_pr(1, ACTIVITY_T0)]), json!([]));
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([]),
        ));
        let mut partial = activity_pr(2, ACTIVITY_T1);
        partial.as_object_mut().unwrap().remove("updated_at");
        responses.extend(activity_poll_responses(
            json!([]),
            json!([partial]),
            json!([]),
        ));
        responses.extend(activity_poll_responses(
            json!([]),
            json!([activity_pr(2, ACTIVITY_T1)]),
            json!([]),
        ));
        responses.push(activity_response(
            "/repos/org/repo/pulls/2/reviews",
            json!([activity_review(2, "COMMENTED", ACTIVITY_T1)]),
        ));
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T0)
        );
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["review_id"], 2);
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T1)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_review_baseline_spans_cap_without_historical_notifications() {
        let prs: Vec<_> = (1..=21)
            .map(|number| activity_pr(number, ACTIVITY_T0))
            .collect();
        let mut responses = activity_poll_responses(json!([]), json!(prs), json!([]));
        for number in 1..=20 {
            responses.push(activity_response(
                &format!("/repos/org/repo/pulls/{number}/reviews"),
                json!([activity_review(number, "APPROVED", ACTIVITY_T0)]),
            ));
        }
        responses.extend(activity_scan_responses());
        responses.push(activity_response(
            "/repos/org/repo/pulls/21/reviews",
            json!([activity_review(21, "APPROVED", ACTIVITY_T0)]),
        ));
        responses.extend(activity_poll_responses(
            json!([]),
            json!([activity_pr(21, ACTIVITY_T1)]),
            json!([]),
        ));
        responses.push(activity_response(
            "/repos/org/repo/pulls/21/reviews",
            json!([
                activity_review(21, "APPROVED", ACTIVITY_T0),
                activity_review(22, "COMMENTED", ACTIVITY_T1)
            ]),
        ));
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert!(harness.repo_state().activity.review_watermark.is_none());
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T0)
        );
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["review_id"], 22);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_f2_review_scan_keeps_submission_lower_bound_across_21_prs() {
        let mut responses =
            activity_poll_responses(json!([]), json!([activity_pr(1, ACTIVITY_T0)]), json!([]));
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([]),
        ));
        let prs: Vec<_> = (2..=22)
            .rev()
            .map(|number| activity_pr(number, ACTIVITY_T2))
            .collect();
        responses.extend(activity_poll_responses(json!([]), json!(prs), json!([])));
        for number in 2..=21 {
            responses.push(activity_response(
                &format!("/repos/org/repo/pulls/{number}/reviews"),
                json!([activity_review(number, "APPROVED", ACTIVITY_T1)]),
            ));
        }
        responses.extend(activity_scan_responses());
        responses.push(activity_response(
            "/repos/org/repo/pulls/22/reviews",
            json!([activity_review(22, "APPROVED", ACTIVITY_T1)]),
        ));
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let mut events = harness.poll().await;
        assert_eq!(events.len(), 20);
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T0)
        );
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_scan
                .as_ref()
                .unwrap()
                .pending,
            [22]
        );
        events.extend(harness.poll().await);
        let ids: HashSet<_> = events
            .iter()
            .map(|event| event.payload["review_id"].as_u64().unwrap())
            .collect();
        assert_eq!(events.len(), 21);
        assert_eq!(ids, (2..=22).collect());
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T2)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_f1_same_second_second_review_is_emitted_once() {
        let mut responses = activity_review_baseline_responses();
        for ids in [vec![42], vec![42, 43], vec![42, 43]] {
            responses.extend(activity_poll_responses(
                json!([]),
                json!([activity_pr(42, ACTIVITY_T1)]),
                json!([]),
            ));
            let reviews: Vec<_> = ids
                .into_iter()
                .map(|id| activity_review(id, "APPROVED", ACTIVITY_T1))
                .collect();
            responses.push(activity_response(
                "/repos/org/repo/pulls/42/reviews",
                json!(reviews),
            ));
        }
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let first = harness.poll().await;
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].payload["review_id"], 42);
        let second = harness.poll().await;
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].payload["review_id"], 43);
        assert!(harness.poll().await.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_f3a_inline_cold_start_skips_1101_comment_history() {
        let history: Vec<_> = (1..=1101)
            .rev()
            .map(|id| {
                let timestamp = activity_second(id);
                activity_comment(id, 1, &timestamp, &timestamp)
            })
            .collect();
        let mut responses = activity_poll_responses(json!([]), json!([]), json!(&history[..100]));
        responses.last_mut().unwrap().3 = Some("/repos/org/repo/pulls/comments?page=2".into());
        let new_time = activity_second(1102);
        responses.extend(activity_poll_responses(
            json!([]),
            json!([]),
            json!([
                history[0].clone(),
                activity_comment(1102, 1, &new_time, &new_time)
            ]),
        ));
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/pulls/comments", 1, &[1]);
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_cursor
                .as_deref(),
            Some(activity_second(1101).as_str())
        );
        assert_eq!(
            harness.repo_state().activity.emitted_review_comment_ids,
            (1002..=1101).rev().collect::<VecDeque<_>>()
        );
        assert_eq!(requests.lock().unwrap().len(), 4);
        assert!(requests.lock().unwrap()[3].contains("direction=desc"));
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["comment_id"], 1102);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_f3b_review_cold_start_skips_501_pr_history() {
        let history: Vec<_> = (1..=501)
            .map(|number| {
                activity_pr(
                    number,
                    if number == 1 {
                        ACTIVITY_T1
                    } else {
                        ACTIVITY_T0
                    },
                )
            })
            .collect();
        let mut responses = activity_poll_responses(json!([]), json!(&history[..100]), json!([]));
        responses[0].3 = Some("/repos/org/repo/pulls?page=2".into());
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([activity_review(1, "APPROVED", ACTIVITY_T1)]),
        ));
        responses.extend(activity_poll_responses(
            json!([]),
            json!([activity_pr(501, ACTIVITY_T2)]),
            json!([]),
        ));
        responses.push(activity_response(
            "/repos/org/repo/pulls/501/reviews",
            json!([
                activity_review(501, "APPROVED", ACTIVITY_T0),
                activity_review(502, "APPROVED", ACTIVITY_T2)
            ]),
        ));
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T1)
        );
        assert!(harness.repo_state().activity.review_scan.is_none());
        assert_eq!(harness.repo_state().activity.emitted_review_ids, [1]);
        assert_eq!(requests.lock().unwrap().len(), 5);
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["review_id"], 502);
        assert!(
            !harness
                .repo_state()
                .activity
                .emitted_review_ids
                .contains(&501),
            "reviews below W are not remembered"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_f4_capped_501_pr_scan_drains_500_without_starvation() {
        let mut responses = activity_review_baseline_responses();
        let prs: Vec<_> = (2..=502)
            .map(|number| activity_pr(number, ACTIVITY_T2))
            .collect();
        for (page, chunk) in prs[..500].chunks(100).enumerate() {
            let mut response = activity_response("/repos/org/repo/pulls", json!(chunk));
            response.3 = Some(format!("/repos/org/repo/pulls?page={}", page + 2));
            responses.push(response);
        }
        for batch in 0..25 {
            responses.extend(activity_scan_responses());
            for number in (2 + batch * 20)..(22 + batch * 20) {
                responses.push(activity_response(
                    &format!("/repos/org/repo/pulls/{number}/reviews"),
                    json!([activity_review(number, "APPROVED", ACTIVITY_T1)]),
                ));
            }
        }
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let mut events = Vec::new();
        for batch in 0..25 {
            let next = harness.poll().await;
            assert_eq!(next.len(), 20);
            events.extend(next);
            if batch < 24 {
                assert_eq!(
                    harness.repo_state().activity.review_watermark.as_deref(),
                    Some(ACTIVITY_T0)
                );
                assert_eq!(
                    harness
                        .repo_state()
                        .activity
                        .review_scan
                        .as_ref()
                        .unwrap()
                        .pending
                        .len(),
                    480 - batch * 20
                );
            }
        }
        let ids: HashSet<_> = events
            .iter()
            .map(|event| event.payload["review_id"].as_u64().unwrap())
            .collect();
        assert_eq!(events.len(), 500);
        assert_eq!(ids, (2..=501).collect());
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T2)
        );
        assert!(harness.repo_state().activity.review_scan.is_none());
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.starts_with("/repos/org/repo/pulls?"))
                .count(),
            6
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_f5_issue_comments_never_affect_pr_dedup_or_emission() {
        let mut comments = vec![activity_comment(2, 1, ACTIVITY_T1, ACTIVITY_T1)];
        for id in 3..=1001 {
            let mut comment = activity_comment(id, 2, ACTIVITY_T1, ACTIVITY_T1);
            comment["html_url"] = json!(format!(
                "https://github.com/org/repo/issues/2#issuecomment-{id}"
            ));
            comments.push(comment);
        }
        let mut responses = activity_poll_responses(
            json!([activity_comment(1, 1, ACTIVITY_T0, ACTIVITY_T0)]),
            json!([]),
            json!([]),
        );
        for _ in 0..3 {
            let mut poll = activity_poll_responses(json!([]), json!([]), json!([]));
            poll[2].1 = json!(&comments[..100]);
            poll[2].3 = Some("/repos/org/repo/issues/comments?page=2".into());
            let inline = poll.pop().unwrap();
            responses.extend(poll);
            for (page, chunk) in comments[100..].chunks(100).enumerate() {
                let mut response =
                    activity_response("/repos/org/repo/issues/comments", json!(chunk));
                if page < 8 {
                    response.3 = Some(format!("/repos/org/repo/issues/comments?page={}", page + 3));
                }
                responses.push(response);
            }
            responses.push(inline);
        }
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/issues/comments", 1, &[1]);
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.pr-commented");
        assert_eq!(events[0].payload["comment_id"], 2);
        for _ in 0..2 {
            assert!(harness.poll().await.is_empty());
        }
        assert_eq!(harness.repo_state().activity.emitted_pr_comment_ids, [1, 2]);
        assert_eq!(
            harness.repo_state().activity.pr_comment_cursor.as_deref(),
            Some(ACTIVITY_T1)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_f6_ci_persist_failure_never_replays_delivered_conversation() {
        let mut responses = activity_poll_responses(
            json!([activity_comment(1, 1, ACTIVITY_T0, ACTIVITY_T0)]),
            json!([]),
            json!([]),
        );
        for _ in 0..2 {
            responses.extend(activity_poll_responses(
                json!([activity_comment(2, 1, ACTIVITY_T1, ACTIVITY_T1)]),
                json!([]),
                json!([]),
            ));
        }
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/issues/comments", 1, &[1]);
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"block").unwrap();
        harness.baseline_path = Some(blocker.join("baseline.json"));
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.pr-commented");
        assert_eq!(events[0].payload["comment_id"], 2);
        assert_eq!(
            harness.repo_state().activity.pr_comment_cursor.as_deref(),
            Some(ACTIVITY_T1),
            "real CI persist failure must retain delivered activity state"
        );
        assert!(
            telemetry::take_test_records()
                .iter()
                .any(|record| { record["reason_code"] == "ci_baseline_persist_failed" }),
            "must exercise the real CI persist failure branch"
        );
        assert!(
            harness
                .repo_state()
                .activity
                .emitted_pr_comment_ids
                .contains(&2)
        );
        assert!(harness.poll().await.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_inline_cap_resumes_1101_new_comments_without_duplicates() {
        let mut responses = activity_review_baseline_responses();
        responses.extend(activity_poll_responses(json!([]), json!([]), json!([])));
        responses.pop();
        for page in 0..10 {
            let comments: Vec<_> = ((page * 100 + 1)..=(page * 100 + 100))
                .map(|id| {
                    let timestamp = activity_second(id);
                    activity_comment(id, 1, &timestamp, &timestamp)
                })
                .collect();
            let mut response = activity_response("/repos/org/repo/pulls/comments", json!(comments));
            response.3 = Some(format!("/repos/org/repo/pulls/comments?page={}", page + 2));
            responses.push(response);
        }
        let tail: Vec<_> = (1000..=1101)
            .map(|id| {
                let timestamp = activity_second(id);
                activity_comment(id, 1, &timestamp, &timestamp)
            })
            .collect();
        for _ in 0..2 {
            responses.extend(activity_poll_responses(json!([]), json!([]), json!(tail)));
        }
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/pulls/comments", 1, &[1]);
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let mut events = harness.poll().await;
        assert_eq!(events.len(), 1000);
        harness.assert_degraded("GitHub inline review comment pagination cap reached");
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_cursor
                .as_deref(),
            Some(activity_second(1000).as_str())
        );
        events.extend(harness.poll().await);
        assert_eq!(events.len(), 1101);
        let ids: HashSet<_> = events
            .iter()
            .map(|event| event.payload["comment_id"].as_u64().unwrap())
            .collect();
        assert_eq!(ids, (1..=1101).collect());
        assert!(harness.poll().await.is_empty());
        {
            let requests = requests.lock().unwrap();
            let resumed = requests
                .iter()
                .filter(|request| request.starts_with("/repos/org/repo/pulls/comments?"))
                .nth(11)
                .unwrap();
            assert!(
                resumed.contains("since=2030-01-01T00%3A16%3A39"),
                "{resumed}"
            );
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_inline_partial_fetch_failure_preserves_cursor_and_retries() {
        let mut responses = activity_review_baseline_responses();
        let mut failed = activity_poll_responses(
            json!([]),
            json!([]),
            json!([activity_comment(1, 1, ACTIVITY_T1, ACTIVITY_T1)]),
        );
        failed.last_mut().unwrap().3 = Some("/repos/org/repo/pulls/comments?page=2".into());
        responses.extend(failed);
        let mut failure = activity_response(
            "/repos/org/repo/pulls/comments",
            json!({"message": "second page failed"}),
        );
        failure.2 = 500;
        responses.push(failure);
        responses.extend(activity_poll_responses(
            json!([]),
            json!([]),
            json!([
                activity_comment(1, 1, ACTIVITY_T1, ACTIVITY_T1),
                activity_comment(2, 1, ACTIVITY_T2, ACTIVITY_T2)
            ]),
        ));
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/pulls/comments", 3, &[1]);
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert!(harness.poll().await.is_empty());
        harness.assert_degraded("GitHub inline review comments fetch failed:");
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_lower
                .as_deref(),
            Some(ACTIVITY_T0)
        );
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_cursor
                .as_deref(),
            Some(ACTIVITY_T0)
        );
        assert!(
            !harness
                .repo_state()
                .activity
                .emitted_review_comment_ids
                .contains(&1)
        );
        let events = harness.poll().await;
        assert_eq!(events.len(), 2);
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_cursor
                .as_deref(),
            Some(ACTIVITY_T2)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_review_page_cap_requeues_without_replaying_sent_reviews() {
        let mut responses = activity_review_baseline_responses();
        responses.extend(activity_poll_responses(
            json!([]),
            json!([activity_pr(2, ACTIVITY_T1)]),
            json!([]),
        ));
        for page in 1..=MAX_COMMENT_PAGES {
            let mut response = activity_response(
                "/repos/org/repo/pulls/2/reviews",
                if page == 1 {
                    json!([activity_review(2, "APPROVED", ACTIVITY_T1)])
                } else {
                    json!([])
                },
            );
            response.3 = Some(format!("/repos/org/repo/pulls/2/reviews?page={}", page + 1));
            responses.push(response);
        }
        responses.extend(activity_scan_responses());
        responses.push(activity_response(
            "/repos/org/repo/pulls/2/reviews",
            json!([
                activity_review(2, "APPROVED", ACTIVITY_T1),
                activity_review(3, "APPROVED", ACTIVITY_T1)
            ]),
        ));
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["review_id"], 2);
        harness.assert_degraded("GitHub per-PR review pagination cap reached");
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T0)
        );
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_scan
                .as_ref()
                .unwrap()
                .pending,
            [2]
        );
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["review_id"], 3);
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T1)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_cold_empty_feed_uses_server_date_not_local_clock() {
        let mut responses = activity_poll_responses(json!([]), json!([]), json!([]));
        responses[0].4 = Some("Tue, 01 Jan 2030 00:00:00 GMT".into());
        responses[3].4 = Some("Tue, 01 Jan 2030 00:00:00 GMT".into());
        for _ in 0..2 {
            responses.extend(activity_poll_responses(
                json!([]),
                json!([activity_pr(1, ACTIVITY_T1)]),
                json!([activity_comment(1, 1, ACTIVITY_T1, ACTIVITY_T1)]),
            ));
            responses.push(activity_response(
                "/repos/org/repo/pulls/1/reviews",
                json!([activity_review(1, "APPROVED", ACTIVITY_T1)]),
            ));
        }
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_cursor
                .as_deref(),
            Some(ACTIVITY_T0)
        );
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_lower
                .as_deref(),
            Some(ACTIVITY_T0)
        );
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T0)
        );
        assert_eq!(harness.poll().await.len(), 2);
        assert!(harness.poll().await.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_cold_invalid_date_falls_back_to_observed_timestamps() {
        let mut responses = activity_poll_responses(
            json!([]),
            json!([activity_pr(1, ACTIVITY_T1)]),
            json!([activity_comment(1, 1, ACTIVITY_T1, ACTIVITY_T1)]),
        );
        responses[0].4 = Some("not a server date".into());
        responses[3].4 = Some("not a server date".into());
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([activity_review(1, "APPROVED", ACTIVITY_T1)]),
        ));
        for _ in 0..2 {
            responses.extend(activity_poll_responses(
                json!([]),
                json!([activity_pr(1, ACTIVITY_T2)]),
                json!([
                    activity_comment(2, 1, ACTIVITY_T0, ACTIVITY_T2),
                    activity_comment(3, 1, ACTIVITY_T2, ACTIVITY_T2)
                ]),
            ));
            responses.push(activity_response(
                "/repos/org/repo/pulls/1/reviews",
                json!([
                    activity_review(1, "APPROVED", ACTIVITY_T1),
                    activity_review(2, "COMMENTED", ACTIVITY_T2)
                ]),
            ));
        }
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_lower
                .as_deref(),
            Some(ACTIVITY_T1)
        );
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some(ACTIVITY_T1)
        );
        let events = harness.poll().await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload["comment_id"], 3);
        assert_eq!(events[1].payload["review_id"], 2);
        assert!(
            !harness
                .repo_state()
                .activity
                .emitted_review_comment_ids
                .contains(&2),
            "ineligible old edits must not enter the FIFO"
        );
        assert!(harness.poll().await.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_f8_inline_cold_partial_boundary_no_replay() {
        let history: Vec<_> = (1..=101)
            .map(|id| activity_comment(id, 1, ACTIVITY_T0, ACTIVITY_T0))
            .collect();
        let mut responses = activity_poll_responses(json!([]), json!([]), json!(&history[..100]));
        let cold = responses.last_mut().unwrap();
        cold.3 = Some("/repos/org/repo/pulls/comments?page=2".into());
        cold.4 = Some("Tue, 01 Jan 2030 00:00:01 GMT".into());
        let mut overlap = activity_poll_responses(json!([]), json!([]), json!(&history[..100]));
        overlap.last_mut().unwrap().3 = Some("/repos/org/repo/pulls/comments?page=2".into());
        responses.extend(overlap);
        responses.push(activity_response(
            "/repos/org/repo/pulls/comments",
            json!([history[100]]),
        ));
        for _ in 0..2 {
            responses.extend(activity_poll_responses(
                json!([]),
                json!([]),
                json!([
                    history[100],
                    activity_comment(102, 1, ACTIVITY_T1, ACTIVITY_T1)
                ]),
            ));
        }
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/pulls/comments", 3, &[1]);
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_lower
                .as_deref(),
            Some("2030-01-01T00:00:01Z")
        );
        assert_eq!(
            requests.lock().unwrap().len(),
            4,
            "cold fetch must stay bounded to one page"
        );
        assert!(
            harness.poll().await.is_empty(),
            "unseen historical id 101 must not replay"
        );
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["comment_id"], 102);
        assert!(harness.poll().await.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_f8_review_cold_partial_boundary_no_replay() {
        let history: Vec<_> = (1..=101)
            .map(|number| activity_pr(number, ACTIVITY_T0))
            .collect();
        let mut responses = activity_poll_responses(json!([]), json!(&history[..100]), json!([]));
        responses[0].3 = Some("/repos/org/repo/pulls?page=2".into());
        responses[0].4 = Some("Tue, 01 Jan 2030 00:00:01 GMT".into());
        for batch in 0..5 {
            if batch > 0 {
                responses.extend(activity_scan_responses());
            }
            for number in (batch * 20 + 1)..=(batch * 20 + 20) {
                responses.push(activity_response(
                    &format!("/repos/org/repo/pulls/{number}/reviews"),
                    json!([activity_review(number, "APPROVED", ACTIVITY_T0)]),
                ));
            }
        }
        // The next list includes the previously unseen PR at the old boundary.
        // Its update is below the Date-based baseline, so no historical reviews are fetched.
        let mut overlap = vec![history[100].clone()];
        overlap.extend_from_slice(&history[..99]);
        responses.extend(activity_poll_responses(
            json!([]),
            json!(overlap),
            json!([]),
        ));
        for _ in 0..2 {
            responses.extend(activity_poll_responses(
                json!([]),
                json!([activity_pr(101, ACTIVITY_T2)]),
                json!([]),
            ));
            responses.push(activity_response(
                "/repos/org/repo/pulls/101/reviews",
                json!([
                    activity_review(101, "APPROVED", ACTIVITY_T0),
                    activity_review(102, "COMMENTED", ACTIVITY_T1)
                ]),
            ));
        }
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        for _ in 0..5 {
            assert!(harness.poll().await.is_empty());
        }
        assert_eq!(
            harness.repo_state().activity.review_watermark.as_deref(),
            Some("2030-01-01T00:00:01Z")
        );
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|path| path.starts_with("/repos/org/repo/pulls?"))
                .count(),
            1
        );
        assert!(harness.poll().await.is_empty());
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["review_id"], 102);
        assert!(harness.poll().await.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_conversation_cold_partial_boundary_no_replay() {
        let history: Vec<_> = (1..=101)
            .map(|id| activity_comment(id, 1, ACTIVITY_T0, ACTIVITY_T0))
            .collect();
        let mut responses = activity_poll_responses(json!(&history[..100]), json!([]), json!([]));
        responses[2].3 = Some("/repos/org/repo/issues/comments?page=2".into());
        responses[2].4 = Some("Tue, 01 Jan 2030 00:00:01 GMT".into());
        let mut overlap = activity_poll_responses(json!(&history[..100]), json!([]), json!([]));
        overlap[2].3 = Some("/repos/org/repo/issues/comments?page=2".into());
        let inline = overlap.pop().unwrap();
        responses.extend(overlap);
        responses.push(activity_response(
            "/repos/org/repo/issues/comments",
            json!([history[100]]),
        ));
        responses.push(inline);
        for _ in 0..2 {
            responses.extend(activity_poll_responses(
                json!([
                    history[100],
                    activity_comment(102, 1, ACTIVITY_T1, ACTIVITY_T1)
                ]),
                json!([]),
                json!([]),
            ));
        }
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/issues/comments", 3, &[1]);
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert_eq!(
            harness.repo_state().activity.pr_comment_cursor.as_deref(),
            Some(ACTIVITY_T0)
        );
        assert_eq!(
            harness.repo_state().activity.pr_comment_lower.as_deref(),
            Some("2030-01-01T00:00:01Z")
        );
        assert_eq!(
            harness.repo_state().activity.emitted_pr_comment_ids.len(),
            100
        );
        assert_eq!(
            requests.lock().unwrap().len(),
            4,
            "cold start must read only one conversation page"
        );
        assert!(requests.lock().unwrap()[2].contains("direction=desc"));
        assert!(
            harness.poll().await.is_empty(),
            "unseen historical id 101 must not replay"
        );
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.pr-commented");
        assert_eq!(events[0].payload["comment_id"], 102);
        assert!(harness.poll().await.is_empty());
        let requests = requests.lock().unwrap().clone();
        let resumed = &requests[6];
        assert!(
            resumed.contains("sort=updated")
                && resumed.contains("direction=asc")
                && resumed.contains("per_page=100")
        );
        assert!(
            resumed.contains("since=2029-12-31T23%3A59%3A59"),
            "{resumed}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_conversation_capped_resume_keeps_edited_unseen_comment() {
        let mut responses = activity_poll_responses(
            json!([activity_comment(0, 1, ACTIVITY_T0, ACTIVITY_T0)]),
            json!([]),
            json!([]),
        );
        let mut capped = activity_poll_responses(json!([]), json!([]), json!([]));
        let inline = capped.pop().unwrap();
        capped.pop();
        responses.extend(capped);
        for page in 0..MAX_COMMENT_PAGES {
            let comments: Vec<_> = ((page * 100 + 1)..=(page * 100 + 100))
                .map(|id| {
                    let timestamp = activity_second(id as u64);
                    activity_comment(id as u64, 1, &timestamp, &timestamp)
                })
                .collect();
            let mut response =
                activity_response("/repos/org/repo/issues/comments", json!(comments));
            response.3 = Some(format!("/repos/org/repo/issues/comments?page={}", page + 2));
            responses.push(response);
        }
        responses.push(inline);
        let tail = json!([
            activity_comment(1000, 1, &activity_second(1000), &activity_second(1000)),
            activity_comment(1001, 1, &activity_second(1), &activity_second(1001))
        ]);
        for _ in 0..2 {
            responses.extend(activity_poll_responses(tail.clone(), json!([]), json!([])));
        }
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/issues/comments", 1, &[1]);
        let (base, requests, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let mut events = harness.poll().await;
        assert_eq!(events.len(), 1000);
        harness.assert_degraded("GitHub PR conversation comment pagination cap reached");
        assert_eq!(
            harness.repo_state().activity.pr_comment_cursor.as_deref(),
            Some(activity_second(1000).as_str())
        );
        assert_eq!(
            harness.repo_state().activity.pr_comment_lower.as_deref(),
            Some(ACTIVITY_T0)
        );
        let resumed = harness.poll().await;
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].payload["comment_id"], 1001);
        events.extend(resumed);
        let ids: HashSet<_> = events
            .iter()
            .map(|event| event.payload["comment_id"].as_u64().unwrap())
            .collect();
        assert_eq!(events.len(), 1001);
        assert_eq!(ids, (1..=1001).collect());
        assert_eq!(
            harness.repo_state().activity.pr_comment_lower.as_deref(),
            Some(activity_second(1001).as_str())
        );
        assert!(harness.poll().await.is_empty());
        let requests = requests.lock().unwrap().clone();
        let resumed = requests
            .iter()
            .filter(|path| path.starts_with("/repos/org/repo/issues/comments?"))
            .nth(11)
            .unwrap();
        assert!(
            resumed.contains("since=2030-01-01T00%3A16%3A39"),
            "{resumed}"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_conversation_partial_failure_preserves_position_lower_and_ids() {
        let mut responses = activity_poll_responses(
            json!([activity_comment(0, 1, ACTIVITY_T0, ACTIVITY_T0)]),
            json!([]),
            json!([]),
        );
        let mut failed = activity_poll_responses(
            json!([activity_comment(1, 1, ACTIVITY_T1, ACTIVITY_T1)]),
            json!([]),
            json!([]),
        );
        failed[2].3 = Some("/repos/org/repo/issues/comments?page=2".into());
        let inline = failed.pop().unwrap();
        responses.extend(failed);
        let mut failure = activity_response(
            "/repos/org/repo/issues/comments",
            json!({"message": "second page failed"}),
        );
        failure.2 = 500;
        responses.push(failure);
        responses.push(inline);
        responses.extend(activity_poll_responses(
            json!([
                activity_comment(1, 1, ACTIVITY_T1, ACTIVITY_T1),
                activity_comment(2, 1, ACTIVITY_T2, ACTIVITY_T2)
            ]),
            json!([]),
            json!([]),
        ));
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/issues/comments", 3, &[1]);
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        assert!(harness.poll().await.is_empty());
        harness.assert_degraded("GitHub PR conversation comments fetch failed:");
        assert_eq!(
            harness.repo_state().activity.pr_comment_cursor.as_deref(),
            Some(ACTIVITY_T0)
        );
        assert_eq!(
            harness.repo_state().activity.pr_comment_lower.as_deref(),
            Some(ACTIVITY_T0)
        );
        assert_eq!(harness.repo_state().activity.emitted_pr_comment_ids, [0]);
        let events = harness.poll().await;
        assert_eq!(events.len(), 2);
        assert_eq!(
            harness.repo_state().activity.pr_comment_cursor.as_deref(),
            Some(ACTIVITY_T2)
        );
        assert_eq!(
            harness.repo_state().activity.pr_comment_lower.as_deref(),
            Some(ACTIVITY_T2)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_classifier_github_corp_host() {
        let mut pr = activity_comment(1, 42, ACTIVITY_T1, ACTIVITY_T1);
        pr["html_url"] = json!("https://github.corp.net/org/repo/pull/42#issuecomment-1");
        let mut issue = activity_comment(2, 1, ACTIVITY_T1, ACTIVITY_T1);
        issue["html_url"] = json!("https://github.corp.net/org/repo/issues/1#issuecomment-2");
        issue["issue_url"] = json!("https://github.corp.net/api/v3/repos/org/repo/issues/1");
        let mut api_pr = activity_comment(3, 43, ACTIVITY_T1, ACTIVITY_T1);
        api_pr["html_url"] = json!("not a URL");
        api_pr["issue_url"] = json!("https://github.corp.net/api/v3/repos/org/repo/pulls/43");
        let mut responses = activity_poll_responses(json!([]), json!([]), json!([]));
        responses[2].4 = Some("Tue, 01 Jan 2030 00:00:00 GMT".into());
        for _ in 0..2 {
            responses.extend(activity_poll_responses(
                json!([pr, issue, api_pr]),
                json!([]),
                json!([]),
            ));
        }
        activity_lookup_after_feed(
            &mut responses,
            "/repos/org/repo/issues/comments",
            1,
            &[42, 43],
        );
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let events = harness.poll().await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload["number"], 42);
        assert_eq!(events[1].payload["number"], 43);
        assert_eq!(harness.repo_state().activity.emitted_pr_comment_ids, [1, 3]);
        assert!(harness.poll().await.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_classifier_repo_named_pull() {
        let mut issue = activity_comment(1, 42, ACTIVITY_T1, ACTIVITY_T1);
        issue["html_url"] = json!("https://github.com/org/pull/issues/42#issuecomment-1");
        issue["issue_url"] = json!("https://api.github.com/repos/org/pull/issues/42");
        let mut pr = activity_comment(2, 42, ACTIVITY_T1, ACTIVITY_T1);
        pr["html_url"] = json!("https://github.com/org/pull/pull/42#issuecomment-2");
        pr["issue_url"] = json!("https://api.github.com/repos/org/pull/pulls/42");
        let mut invalid = activity_comment(3, 42, ACTIVITY_T1, ACTIVITY_T1);
        invalid["html_url"] = json!("https://github.com/pull/pull/issues/42");
        invalid["issue_url"] = json!("not a URL");
        let mut responses = activity_poll_responses(json!([]), json!([]), json!([]));
        responses[2].4 = Some("Tue, 01 Jan 2030 00:00:00 GMT".into());
        responses.extend(activity_poll_responses(
            json!([issue, pr, invalid]),
            json!([]),
            json!([]),
        ));
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/issues/comments", 1, &[42]);
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["comment_id"], 2);
        assert_eq!(events[0].payload["number"], 42);
        assert_eq!(harness.repo_state().activity.emitted_pr_comment_ids, [2]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pr_activity_f9_capped_inline_resume_keeps_edited_unseen_comment() {
        let mut responses = activity_review_baseline_responses();
        responses.extend(activity_poll_responses(json!([]), json!([]), json!([])));
        responses.pop();
        for page in 0..10 {
            let comments: Vec<_> = ((page * 100 + 1)..=(page * 100 + 100))
                .map(|id| {
                    let timestamp = activity_second(id);
                    activity_comment(id, 1, &timestamp, &timestamp)
                })
                .collect();
            let mut response = activity_response("/repos/org/repo/pulls/comments", json!(comments));
            response.3 = Some(format!("/repos/org/repo/pulls/comments?page={}", page + 2));
            responses.push(response);
        }
        let tail = json!([
            activity_comment(1000, 1, &activity_second(1000), &activity_second(1000)),
            activity_comment(1001, 1, &activity_second(1), &activity_second(1001))
        ]);
        for _ in 0..2 {
            responses.extend(activity_poll_responses(json!([]), json!([]), tail.clone()));
        }
        activity_lookup_after_feed(&mut responses, "/repos/org/repo/pulls/comments", 1, &[1]);
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        assert!(harness.poll().await.is_empty());
        let mut events = harness.poll().await;
        assert_eq!(events.len(), 1000);
        harness.assert_degraded("GitHub inline review comment pagination cap reached");
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_cursor
                .as_deref(),
            Some(activity_second(1000).as_str())
        );
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_lower
                .as_deref(),
            Some(ACTIVITY_T0)
        );
        let resumed = harness.poll().await;
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].payload["comment_id"], 1001);
        events.extend(resumed);
        let ids: HashSet<_> = events
            .iter()
            .map(|event| event.payload["comment_id"].as_u64().unwrap())
            .collect();
        assert_eq!(events.len(), 1001);
        assert_eq!(ids, (1..=1001).collect());
        assert_eq!(
            harness
                .repo_state()
                .activity
                .review_comment_lower
                .as_deref(),
            Some(activity_second(1001).as_str())
        );
        assert!(harness.poll().await.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn issue_380_default_config_emits_issue_commented() {
        // Test defect 1: issue comments should be detected when emit_issue_opened=true,
        // independent of emit_pr_activity=false (the default).
        let prs = json!([activity_pr(1, ACTIVITY_T0)]);
        let mut responses = activity_poll_responses(
            json!([activity_issue_comment(1, 42, ACTIVITY_T0, ACTIVITY_T0)]),
            prs.clone(),
            json!([]),
        );
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([]),
        ));
        for _ in 0..2 {
            responses.extend(activity_poll_responses(json!([]), prs.clone(), json!([])));
            responses.push(activity_response(
                "/repos/org/repo/pulls/1/reviews",
                json!([]),
            ));
        }
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        // Default config: emit_pr_activity=false, emit_issue_opened=true
        harness.config.monitors.git.repos[0].emit_pr_activity = false;
        // emit_issue_opened is true by default
        assert!(harness.poll().await.is_empty());
        let events = harness.poll().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.issue-commented");
        assert_eq!(events[0].payload["number"], 42);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn issue_380_second_comment_on_same_issue_in_later_poll_emits() {
        // Test defect 2: cross-poll dedup by comment ID, not issue number.
        // Second comment on the same issue should emit in a later poll.
        let prs = json!([activity_pr(1, ACTIVITY_T0)]);
        let mut responses = activity_poll_responses(
            json!([activity_issue_comment(1, 42, ACTIVITY_T0, ACTIVITY_T0)]),
            prs.clone(),
            json!([]),
        );
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([]),
        ));
        // Second poll: same issue, different comment
        for _ in 0..2 {
            responses.extend(activity_poll_responses(
                json!([activity_issue_comment(2, 42, ACTIVITY_T1, ACTIVITY_T1)]),
                prs.clone(),
                json!([]),
            ));
            responses.push(activity_response(
                "/repos/org/repo/pulls/1/reviews",
                json!([]),
            ));
        }
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        harness.config.monitors.git.repos[0].emit_pr_activity = false;
        assert!(harness.poll().await.is_empty());
        let events1 = harness.poll().await;
        assert_eq!(events1.len(), 1);
        assert_eq!(events1[0].payload["number"], 42);
        let events2 = harness.poll().await;
        // Should emit again because it's a different comment (comment_id=2 vs 1)
        assert_eq!(events2.len(), 1);
        assert_eq!(events2[0].canonical_kind(), "github.issue-commented");
        assert_eq!(events2[0].payload["number"], 42);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn issue_380_two_comments_same_issue_same_poll_emit_once() {
        // Test defect 2b: within-poll dedup by issue number.
        // Two comments on one issue in one poll should emit only once.
        let prs = json!([activity_pr(1, ACTIVITY_T0)]);
        let mut responses = activity_poll_responses(
            json!([
                activity_issue_comment(1, 42, ACTIVITY_T0, ACTIVITY_T0),
                activity_issue_comment(2, 42, ACTIVITY_T0, ACTIVITY_T0),
            ]),
            prs.clone(),
            json!([]),
        );
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([]),
        ));
        // Second poll with empty responses
        responses.extend(activity_poll_responses(json!([]), prs.clone(), json!([])));
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([]),
        ));
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        harness.config.monitors.git.repos[0].emit_pr_activity = false;
        assert!(harness.poll().await.is_empty());
        let events = harness.poll().await;
        // Should emit once per issue per poll, not once per comment
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].canonical_kind(), "github.issue-commented");
        assert_eq!(events[0].payload["number"], 42);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn issue_380_edited_old_comment_does_not_emit() {
        // Test defect 2c: edited old comment (same comment ID, just updated_at changed)
        // should not emit because the comment ID was already seen.
        let prs = json!([activity_pr(1, ACTIVITY_T0)]);
        let mut responses = activity_poll_responses(
            json!([activity_issue_comment(1, 42, ACTIVITY_T0, ACTIVITY_T0)]),
            prs.clone(),
            json!([]),
        );
        responses.push(activity_response(
            "/repos/org/repo/pulls/1/reviews",
            json!([]),
        ));
        // Second poll: same comment (same ID), but updated_at changed
        for _ in 0..2 {
            responses.extend(activity_poll_responses(
                json!([activity_issue_comment(1, 42, ACTIVITY_T0, ACTIVITY_T1)]),
                prs.clone(),
                json!([]),
            ));
            responses.push(activity_response(
                "/repos/org/repo/pulls/1/reviews",
                json!([]),
            ));
        }
        let (base, _, server) = activity_server(responses).await;
        let mut harness = ActivityHarness::new(base);
        harness.config.monitors.git.repos[0].emit_pr_activity = false;
        assert!(harness.poll().await.is_empty());
        let events1 = harness.poll().await;
        assert_eq!(events1.len(), 1);
        assert_eq!(events1[0].payload["number"], 42);
        let events2 = harness.poll().await;
        // Should not emit because it's the same comment_id
        assert_eq!(events2.len(), 0);
        server.await.unwrap();
    }
}
