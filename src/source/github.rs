use std::cell::Cell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, LINK, USER_AGENT};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::Result;
use crate::config::{AppConfig, GitRepoMonitor};
use crate::events::IncomingEvent;
use crate::source::Source;
use crate::source::git::{GitSnapshot, repo_display_name, snapshot_git_repo};
use crate::telemetry;

thread_local! {
    static SYNC_FILE_CALLS: Cell<u32> = const { Cell::new(0) };
}

pub struct GitHubSource {
    config: Arc<AppConfig>,
    ci_baseline_path: Option<PathBuf>,
}

impl GitHubSource {
    /// Restarts and re-enrollments reuse a persisted terminal-run baseline so
    /// historical workflow results never replay as fresh events (#317).
    pub fn with_ci_baseline_path(config: Arc<AppConfig>, ci_baseline_path: PathBuf) -> Self {
        Self {
            config,
            ci_baseline_path: Some(ci_baseline_path),
        }
    }
}

#[async_trait::async_trait]
impl Source for GitHubSource {
    fn name(&self) -> &str {
        "github"
    }

    async fn run(&self, tx: mpsc::Sender<IncomingEvent>) -> Result<()> {
        let github_client = match build_github_client(self.config.monitor_github_token()) {
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
}

/// Durable receipt for one terminal run. Identities are aliases (run-id,
/// check-run id, fallback) so representation drift still suppresses.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct TerminalRunRecord {
    identities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
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
    send_attempts: u32,
    #[serde(default)]
    last_sent_unix: Option<u64>,
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
            acked_unix: now,
        }
    }

    fn matches_pending(&self, pending: &PendingCiDelivery) -> bool {
        if self.kind != pending.kind || self.conclusion != pending.conclusion {
            return false;
        }
        let self_checks = pending_check_identities(&self.identities);
        let pending_checks = pending_check_identities(&pending.identities);
        if !self_checks.is_empty() && !pending_checks.is_empty() {
            return self_checks
                .iter()
                .any(|identity| pending_checks.iter().any(|candidate| candidate == identity));
        }
        let run_match = self.run_id.is_some()
            && self.run_id == pending.run_id
            && run_attempt_or_one(self.run_attempt) == run_attempt_or_one(pending.run_attempt)
            && self.workflow == pending.workflow;
        if run_match {
            return true;
        }
        self.sha == pending.sha
            && self.workflow == pending.workflow
            && self.pr_number == pending.pr_number
    }

    fn merge_aliases(&mut self, pending: &PendingCiDelivery) {
        for identity in &pending.identities {
            if !self.identities.iter().any(|existing| existing == identity) {
                self.identities.push(identity.clone());
            }
        }
        if self.run_id.is_none() {
            self.run_id = pending.run_id.clone();
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
            send_attempts: 0,
            last_sent_unix: None,
        }
    }

    fn same_event(&self, other: &Self) -> bool {
        if self.kind != other.kind || self.conclusion != other.conclusion {
            return false;
        }
        let self_checks = pending_check_identities(&self.identities);
        let other_checks = pending_check_identities(&other.identities);
        if !self_checks.is_empty() && !other_checks.is_empty() {
            return self_checks
                .iter()
                .any(|identity| other_checks.iter().any(|candidate| candidate == identity));
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
        .with_mention(self.mention);
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

const MAX_BASELINE_RUNS_PER_REPO: usize = 256;
const CI_PAGE_SIZE: usize = 100;
const MAX_CI_PAGES: usize = 5;
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

fn parse_run_identity(identity: &str) -> Option<(String, u32)> {
    let rest = identity.strip_prefix("run:")?;
    if rest.is_empty() {
        return None;
    }
    if rest.matches(':').count() == 1
        && let Some((id, attempt)) = rest.split_once(':')
        && !id.is_empty()
        && let Ok(attempt) = attempt.parse::<u32>()
        && attempt >= 1
    {
        return Some((id.to_string(), attempt));
    }
    if rest.contains(':') {
        return None;
    }
    Some((rest.to_string(), 1))
}

impl TerminalRunRecord {
    fn from_snapshot(ci: &GitHubCISnapshot) -> Self {
        Self {
            identities: ci.identities(),
            run_id: ci.run_id.clone(),
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
        let unique = ci.unique_identities();
        if unique
            .iter()
            .any(|identity| self.contains_identity(identity))
        {
            return true;
        }
        for identity in &self.identities {
            if identity_matches_snapshot(identity, ci)
                && (unique.is_empty() || self.unique_identities().next().is_none())
            {
                return true;
            }
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

fn identity_matches_snapshot(identity: &str, ci: &GitHubCISnapshot) -> bool {
    if ci
        .identities()
        .iter()
        .any(|candidate| candidate == identity)
    {
        return true;
    }
    let key = ci.dedupe_key();
    if identity == key {
        return true;
    }
    if let Some((run_id, attempt)) = parse_run_identity(identity) {
        return ci.run_id.as_deref() == Some(run_id.as_str()) && ci.run_attempt() == attempt;
    }
    false
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

fn cap_repo_baseline(repo_baseline: &mut RepoCIBaseline) -> bool {
    if repo_baseline.terminal_runs.len() <= MAX_BASELINE_RUNS_PER_REPO {
        return false;
    }
    let mut records: Vec<TerminalRunRecord> = repo_baseline.terminal_runs.drain(..).collect();
    records.sort_by_key(|record| record.eviction_key());
    let drop_count = records.len() - MAX_BASELINE_RUNS_PER_REPO;
    records.drain(..drop_count);
    repo_baseline.terminal_runs = records.into();
    true
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
        self.record_terminal_runs_filtered(repo_key, repo_path, current, |_| true)
    }

    fn record_terminal_runs_filtered(
        &mut self,
        repo_key: &str,
        repo_path: &str,
        current: &HashMap<String, GitHubCISnapshot>,
        include: impl Fn(&GitHubCISnapshot) -> bool,
    ) -> bool {
        let mut incoming: Vec<&GitHubCISnapshot> = current
            .values()
            .filter(|ci| ci.is_terminal() && include(ci))
            .collect();
        incoming.sort_by_key(|ci| snapshot_eviction_key(ci));

        let repo_baseline = self.repo_baseline_mut(repo_key, repo_path);
        let mut dirty = false;
        for ci in incoming {
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
        dirty |= cap_repo_baseline(repo_baseline);
        dirty
    }
}

fn terminal_records_same_run(left: &TerminalRunRecord, right: &TerminalRunRecord) -> bool {
    let left_unique: Vec<&str> = left.unique_identities().collect();
    let right_unique: Vec<&str> = right.unique_identities().collect();
    if !left_unique.is_empty() && !right_unique.is_empty() {
        return left_unique
            .iter()
            .any(|identity| right_unique.contains(identity));
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

fn compact_acked(repo: &mut RepoCIBaseline, now: u64) {
    let before = repo.acked.len();
    repo.acked
        .retain(|acked| now.saturating_sub(acked.acked_unix) < ACK_RETENTION_SECS);
    if repo.acked.len() < before {
        repo.min_writer_epoch = repo.min_writer_epoch.max(repo.epoch);
    }
}

fn merge_repo_baseline(dest: &mut RepoCIBaseline, source: RepoCIBaseline) {
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
    cap_repo_baseline(dest);
    dest.epoch = dest.epoch.max(source.epoch);
    dest.min_writer_epoch = dest.min_writer_epoch.max(source.min_writer_epoch);
    for acked in source.acked {
        if let Some(existing) = dest.acked.iter_mut().find(|candidate| {
            candidate.kind == acked.kind
                && candidate.conclusion == acked.conclusion
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
    let source_retired = source.epoch < dest.min_writer_epoch;
    if !source_retired {
        for delivery in source.pending {
            if pending_is_acked(dest, &delivery) {
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
    }
    let acked = dest.acked.clone();
    dest.pending
        .retain(|pending| !acked.iter().any(|item| item.matches_pending(pending)));
    retire_exhausted_pending(dest);
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
                cap_repo_baseline(repo_baseline);
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
    for _ in 0..40 {
        if try_lock_baseline_file(&file) {
            return Ok(BaselineLock { file });
        }
        std::thread::sleep(Duration::from_millis(5));
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
fn try_lock_baseline_file(file: &File) -> bool {
    use std::os::windows::io::AsRawHandle;
    unsafe { lock_file(file.as_raw_handle(), 0, 0, 1, 0) != 0 }
}

#[cfg(windows)]
fn unlock_baseline_file(file: &File) {
    use std::os::windows::io::AsRawHandle;
    unsafe {
        unlock_file(file.as_raw_handle(), 0, 0, 1, 0);
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "LockFile"]
    fn lock_file(
        handle: *mut core::ffi::c_void,
        offset_low: u32,
        offset_high: u32,
        length_low: u32,
        length_high: u32,
    ) -> i32;
    #[link_name = "UnlockFile"]
    fn unlock_file(
        handle: *mut core::ffi::c_void,
        offset_low: u32,
        offset_high: u32,
        length_low: u32,
        length_high: u32,
    ) -> i32;
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
        fs::rename(&temp_path, path)?;
        let dest = File::open(path)?;
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
        retire_exhausted_pending(repo);
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
    events: Vec<IncomingEvent>,
    delivered: &[PendingCiDelivery],
) -> Result<()> {
    let mut sent = Vec::new();
    let mut send_error = None;
    for (event, delivery) in events.into_iter().zip(delivered.iter()) {
        match send_event(tx, event).await {
            Ok(()) => sent.push(delivery.clone()),
            Err(error) => {
                send_error = Some(error);
                break;
            }
        }
    }
    if !sent.is_empty() {
        ack_pending_deliveries(ci_baseline, path, &sent)?;
    }
    if let Some(error) = send_error {
        return Err(error);
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
    if due.is_empty() {
        return Ok(());
    }
    if let Err(error) = persist_pending_send_attempts(ci_baseline, path, &due, now) {
        eprintln!("clawhip source github CI baseline outbox attempt persist failed: {error}");
        return Ok(());
    }
    let mut sent = Vec::new();
    for delivery in &due {
        if send_event(tx, delivery.clone().into_event()).await.is_err() {
            break;
        }
        sent.push(delivery.clone());
    }
    if sent.is_empty() {
        return Ok(());
    }
    if let Err(error) = ack_pending_deliveries(ci_baseline, path, &sent) {
        eprintln!("clawhip source github CI baseline outbox ack failed: {error}");
    }
    Ok(())
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
            if sent.iter().any(|item| pending.same_event(item)) {
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
                record_acked(repo, delivery);
            }
            repo.pending
                .retain(|pending| !delivered.iter().any(|item| pending.same_event(item)));
        }
        return Ok(());
    };
    let _lock = acquire_baseline_lock(path)?;
    let mut disk = load_ci_baseline(Some(path));
    merge_ci_baseline(&mut disk, ci_baseline);
    for (key, repo) in disk.repos.iter_mut() {
        let local_repo = ci_baseline.repos.get(key);
        for delivery in delivered {
            let in_disk = repo
                .pending
                .iter()
                .any(|pending| pending.same_event(delivery));
            let in_local = local_repo
                .map(|local| {
                    local
                        .pending
                        .iter()
                        .any(|pending| pending.same_event(delivery))
                })
                .unwrap_or(false);
            if in_disk || in_local {
                record_acked(repo, delivery);
            }
        }
        repo.pending
            .retain(|pending| !delivered.iter().any(|item| pending.same_event(item)));
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
        if current.values().any(|ci| snapshots_same_run(ci, old)) {
            continue;
        }
        current.insert(old.dedupe_key(), old.clone());
    }
    current
}

fn snapshots_same_run(left: &GitHubCISnapshot, right: &GitHubCISnapshot) -> bool {
    let left_unique = left.unique_identities();
    let right_unique = right.unique_identities();
    if !left_unique.is_empty()
        && left_unique
            .iter()
            .any(|identity| right_unique.iter().any(|candidate| candidate == identity))
    {
        return true;
    }
    left.dedupe_key() == right.dedupe_key()
}

struct GitHubRepoState {
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
    title: String,
    status: String,
    url: String,
    head_branch: String,
    head_sha: String,
}

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
    github_client: Option<&reqwest::Client>,
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
    {
        telemetry::emit(source_record(
            telemetry::event_name::SOURCE_DEGRADED,
            "source_poll_failed",
            None,
            Some(error.to_string()),
        ));
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
    github_client: Option<&reqwest::Client>,
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut HashMap<String, GitHubRepoState>,
    ci_baseline: &mut CIBaseline,
    ci_baseline_path: Option<&Path>,
) -> Result<()> {
    if let Err(error) = drain_pending_outbox(ci_baseline, ci_baseline_path, tx).await {
        eprintln!("clawhip source github CI outbox drain failed: {error}");
    }

    for repo in &config.monitors.git.repos {
        if !repo.emit_issue_opened && !repo.emit_pr_status {
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
        let prs =
            match poll_pull_requests(config, github_client, repo, &snapshot, previous, tx).await {
                Ok(prs) => prs,
                Err(error) => {
                    eprintln!(
                        "clawhip source GitHub pull request processing failed for {}: {error}",
                        repo.path
                    );
                    previous.map(|entry| entry.prs.clone()).unwrap_or_default()
                }
            };
        let repo_key = ci_baseline_repo_key(&snapshot, repo);
        let repo_ci_baseline = ci_baseline.repos.get(&repo_key).cloned();
        let (ci, ci_baseline_established, ci_events, window_complete) = match poll_ci_statuses(
            config,
            github_client,
            repo,
            &snapshot,
            previous,
            &prs,
            repo_ci_baseline.as_ref(),
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
                    Vec::new(),
                    false,
                )
            }
        };

        // Durable outbox: persist pending receipts, then publish, then
        // drop the outbox. Persistence failure must not mutate in-memory
        // suppression state. A crash after persist and before send retries
        // from the outbox on the next poll.
        let mut next_baseline = ci_baseline.clone();
        let previous_ci = previous.map(|entry| &entry.ci);
        next_baseline.record_terminal_runs_filtered(&repo_key, &repo.path, &ci, |snapshot| {
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
        });
        next_baseline.enqueue_pending(&repo_key, &repo.path, &ci_events, &ci);
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
            if let Err(error) =
                persist_pending_send_attempts(ci_baseline, ci_baseline_path, &delivered, unix_now())
            {
                eprintln!(
                    "clawhip source github CI baseline outbox attempt persist failed for {}: {error}",
                    repo.path
                );
                state.insert(
                    repo.path.clone(),
                    GitHubRepoState {
                        issues,
                        prs,
                        ci,
                        ci_baseline_established,
                    },
                );
                continue;
            }
            send_then_ack_prefix(tx, ci_baseline, ci_baseline_path, ci_events, &delivered).await?;
        }

        state.insert(
            repo.path.clone(),
            GitHubRepoState {
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
    github_client: Option<&reqwest::Client>,
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
            telemetry::emit(source_record(
                telemetry::event_name::SOURCE_DEGRADED,
                "source_poll_failed",
                Some(&repo.path),
                Some(error.to_string()),
            ));
            eprintln!(
                "clawhip source GitHub issue polling failed for {}: {error}",
                repo.path
            );
            Ok(previous
                .map(|entry| entry.issues.clone())
                .unwrap_or_default())
        }
    }
}

async fn poll_pull_requests(
    config: &AppConfig,
    github_client: Option<&reqwest::Client>,
    repo: &GitRepoMonitor,
    snapshot: &GitSnapshot,
    previous: Option<&GitHubRepoState>,
    tx: &mpsc::Sender<IncomingEvent>,
) -> Result<HashMap<u64, PullRequestSnapshot>> {
    if !repo.emit_pr_status {
        return Ok(previous.map(|entry| entry.prs.clone()).unwrap_or_default());
    }

    let Some(client) = github_client else {
        return Ok(previous.map(|entry| entry.prs.clone()).unwrap_or_default());
    };

    match fetch_pull_requests(client, &config.monitors.github_api_base, repo, snapshot).await {
        Ok(prs) => {
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
            Ok(prs)
        }
        Err(error) => {
            telemetry::emit(source_record(
                telemetry::event_name::SOURCE_DEGRADED,
                "source_poll_failed",
                Some(&repo.path),
                Some(error.to_string()),
            ));
            eprintln!(
                "clawhip source GitHub polling failed for {}: {error}",
                repo.path
            );
            Ok(previous.map(|entry| entry.prs.clone()).unwrap_or_default())
        }
    }
}

async fn poll_ci_statuses(
    config: &AppConfig,
    github_client: Option<&reqwest::Client>,
    repo: &GitRepoMonitor,
    snapshot: &GitSnapshot,
    previous: Option<&GitHubRepoState>,
    prs: &HashMap<u64, PullRequestSnapshot>,
    repo_ci_baseline: Option<&RepoCIBaseline>,
) -> Result<(
    HashMap<String, GitHubCISnapshot>,
    bool,
    Vec<IncomingEvent>,
    bool,
)> {
    if !repo.emit_pr_status {
        return Ok((
            previous.map(|entry| entry.ci.clone()).unwrap_or_default(),
            previous
                .map(|entry| entry.ci_baseline_established)
                .unwrap_or(false),
            Vec::new(),
            true,
        ));
    }

    let Some(client) = github_client else {
        return Ok((
            previous.map(|entry| entry.ci.clone()).unwrap_or_default(),
            previous
                .map(|entry| entry.ci_baseline_established)
                .unwrap_or(false),
            Vec::new(),
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
        Ok((fetched, window_complete)) => {
            let ci = if !window_complete {
                if let Some(previous) = previous {
                    merge_incomplete_ci(&previous.ci, fetched)
                } else {
                    fetched
                }
            } else {
                fetched
            };
            let events = if let Some(previous) = previous {
                collect_ci_events(
                    repo,
                    &snapshot.repo_name,
                    previous.ci_baseline_established,
                    &previous.ci,
                    &ci,
                    repo_ci_baseline,
                )
            } else {
                Vec::new()
            };
            let established = previous
                .map(|entry| entry.ci_baseline_established)
                .unwrap_or(true);
            Ok((ci, established, events, window_complete))
        }
        Err(error) => {
            telemetry::emit(source_record(
                telemetry::event_name::SOURCE_DEGRADED,
                "source_poll_failed",
                Some(&repo.path),
                Some(error.to_string()),
            ));
            eprintln!(
                "clawhip source GitHub CI polling failed for {}: {error}",
                repo.path
            );
            // Keep the last snapshot for diffing but drop the established flag:
            // the failed poll may be a partial view (pagination/cursor loss), and
            // treating it as complete would replay whatever the next full poll
            // returns — including terminal runs already delivered before (#317).
            Ok((
                previous.map(|entry| entry.ci.clone()).unwrap_or_default(),
                false,
                Vec::new(),
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
            return ci.check_run_id.as_deref() == Some(check_run_id);
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
    })
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
    client: &reqwest::Client,
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
    eprintln!("clawhip source github: GET {url} ({context})");

    let response = client.get(&url).query(query).send().await?;
    let status = response.status();

    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        eprintln!("clawhip source github: GET {url} ({context}) failed with {status}: {body}");
        return Err(format!("GitHub API request failed with {status}: {body}").into());
    }

    eprintln!("clawhip source github: GET {url} ({context}) -> {status}");
    Ok(response)
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
                if issue.comments > old.comments {
                    events.push(
                        IncomingEvent::github_issue_commented(
                            repo_name.to_string(),
                            *number,
                            issue.title.clone(),
                            issue.comments,
                            repo.channel.clone(),
                        )
                        .with_mention(repo.mention.clone())
                        .with_format(repo.format.clone()),
                    );
                }
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
        return Some(old);
    }
    let unique = ci.unique_identities();
    if unique.is_empty() {
        return None;
    }
    previous.values().find(|old| {
        old.unique_identities()
            .iter()
            .any(|identity| unique.iter().any(|candidate| candidate == identity))
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
    client: &reqwest::Client,
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
    client: &reqwest::Client,
    api_base: &str,
    repo: &GitRepoMonitor,
    snapshot: &GitSnapshot,
) -> Result<HashMap<u64, PullRequestSnapshot>> {
    let github_repo = snapshot
        .github_repo
        .clone()
        .ok_or_else(|| format!("no GitHub repo configured or inferred for {}", repo.path))?;
    let response = github_get(
        client,
        api_base,
        &format!("repos/{github_repo}/pulls"),
        &[("state", "all"), ("per_page", "100")],
        &format!("pull requests for {github_repo}"),
    )
    .await?;
    let pulls: Vec<GitHubPullRequest> = response.json().await?;
    Ok(pulls
        .into_iter()
        .map(|pull| {
            let status = if pull.merged_at.is_some() {
                "merged".to_string()
            } else {
                pull.state
            };
            (
                pull.number,
                PullRequestSnapshot {
                    title: pull.title,
                    status,
                    url: pull.html_url,
                    head_branch: pull.head.reference,
                    head_sha: pull.head.sha,
                },
            )
        })
        .collect())
}

async fn fetch_ci_statuses(
    client: &reqwest::Client,
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
        let pr_attempts =
            fetch_workflow_attempts_by_head_sha(client, api_base, &github_repo, &pr.head_sha)
                .await?;
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

fn ci_window_complete(has_next: bool, pages_fetched: usize) -> bool {
    !(has_next && pages_fetched >= MAX_CI_PAGES)
}

async fn fetch_check_runs(
    client: &reqwest::Client,
    api_base: &str,
    github_repo: &str,
    pr_number: u64,
    pr: &PullRequestSnapshot,
) -> Result<(Vec<GitHubCISnapshot>, bool)> {
    let mut all_runs = Vec::new();
    let mut page = 1_usize;
    let mut window_complete = true;
    loop {
        let page_str = page.to_string();
        let response = github_get(
            client,
            api_base,
            &format!("repos/{github_repo}/commits/{}/check-runs", pr.head_sha),
            &[("per_page", "100"), ("page", page_str.as_str())],
            &format!("check runs for {github_repo} PR #{pr_number}"),
        )
        .await?;
        let has_next = github_link_next(response.headers()).is_some();
        let runs: GitHubCheckRunsResponse = response.json().await?;
        let page_len = runs.check_runs.len();
        all_runs.extend(runs.check_runs);
        if !has_next || page_len < CI_PAGE_SIZE {
            break;
        }
        if !ci_window_complete(has_next, page) {
            window_complete = false;
            break;
        }
        page += 1;
        if page > MAX_CI_PAGES {
            window_complete = false;
            break;
        }
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
    client: &reqwest::Client,
    api_base: &str,
    github_repo: &str,
    snapshot: &GitSnapshot,
) -> Result<(Vec<GitHubCISnapshot>, bool, HashMap<String, u32>)> {
    let mut all_runs = Vec::new();
    let mut page = 1_usize;
    let mut window_complete = true;
    loop {
        let page_str = page.to_string();
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
        let has_next = github_link_next(response.headers()).is_some();
        let runs: GitHubWorkflowRunsResponse = response.json().await?;
        let page_len = runs.workflow_runs.len();
        all_runs.extend(runs.workflow_runs);
        if !has_next || page_len < CI_PAGE_SIZE {
            break;
        }
        if !ci_window_complete(has_next, page) {
            window_complete = false;
            break;
        }
        page += 1;
        if page > MAX_CI_PAGES {
            window_complete = false;
            break;
        }
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
    client: &reqwest::Client,
    api_base: &str,
    github_repo: &str,
    head_sha: &str,
) -> Result<HashMap<String, u32>> {
    let mut attempts = HashMap::new();
    let mut page = 1_usize;
    loop {
        let page_str = page.to_string();
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
        let has_next = github_link_next(response.headers()).is_some();
        let runs: GitHubWorkflowRunsResponse = response.json().await?;
        let page_len = runs.workflow_runs.len();
        for run in runs.workflow_runs {
            let attempt = run_attempt_or_one(run.run_attempt);
            attempts
                .entry(run.id.to_string())
                .and_modify(|existing: &mut u32| *existing = (*existing).max(attempt))
                .or_insert(attempt);
        }
        if !has_next || page_len < CI_PAGE_SIZE || page >= MAX_CI_PAGES {
            break;
        }
        page += 1;
    }
    Ok(attempts)
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

fn build_github_client(token: Option<String>) -> Result<reqwest::Client> {
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
    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .build()?)
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
struct GitHubPullRequest {
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
        client: &reqwest::Client,
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
            events.is_empty(),
            "run-id representation of a fallback-persisted terminal run must stay suppressed"
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
    fn issue_317_single_poll_evicts_oldest_github_runs_first() {
        let mut runs = Vec::new();
        for id in 0..(MAX_BASELINE_RUNS_PER_REPO + 20) {
            let mut run = run_snapshot(id as u64, "CI", Some("success"), "main");
            run.created_at = Some(format!("2026-01-01T00:{:02}:{:02}Z", id / 60, id % 60));
            runs.push(run);
        }
        let mut ci_baseline = CIBaseline::default();
        ci_baseline.record_terminal_runs("/repo", "/repo", &run_map(&runs));
        let repo = &ci_baseline.repos["/repo"];
        assert_eq!(repo.terminal_runs.len(), MAX_BASELINE_RUNS_PER_REPO);
        assert!(!repo.contains_identity("run:0"));
        assert!(!repo.contains_identity("run:19"));
        assert!(repo.contains_identity("run:20"));
        assert!(repo.contains_identity(&format!("run:{}", MAX_BASELINE_RUNS_PER_REPO + 19)));
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
    fn issue_317_oversized_loaded_state_is_truncated_oldest_first() {
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
        assert_eq!(repo.terminal_runs.len(), MAX_BASELINE_RUNS_PER_REPO);
        assert!(!repo.contains_identity("run:0"));
        assert!(repo.contains_identity(&format!("run:{}", MAX_BASELINE_RUNS_PER_REPO + 11)));
    }

    #[test]
    fn issue_317_incomplete_pagination_window_is_detected() {
        assert!(ci_window_complete(false, 1));
        assert!(ci_window_complete(true, 1));
        assert!(!ci_window_complete(true, MAX_CI_PAGES));
        assert!(ci_window_complete(false, MAX_CI_PAGES));
    }

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
        assert!(result.is_err(), "closed receiver must fail the second send");
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
            "retired writer must not restore pending after ACK compaction"
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

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut left = CIBaseline::default();
                left.record_terminal_runs(
                    "repo:org/repo",
                    "/a",
                    &run_map(&[run_snapshot(1, "CI", Some("success"), "main")]),
                );
                commit_ci_baseline(Some(&path), &left).unwrap();
            });
            scope.spawn(|| {
                let mut right = CIBaseline::default();
                right.record_terminal_runs(
                    "repo:org/repo",
                    "/a",
                    &run_map(&[run_snapshot(2, "CI", Some("success"), "main")]),
                );
                commit_ci_baseline(Some(&path), &right).unwrap();
            });
        });

        let loaded = load_ci_baseline(Some(&path));
        let repo = loaded.repos.get("repo:org/repo").unwrap();
        assert!(repo.contains_identity("run:1"));
        assert!(repo.contains_identity("run:2"));
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

        let (ci, ci_baseline_established, events, _window_complete) =
            poll_ci_statuses(&config, Some(&client), &repo, &snapshot, None, &prs, None)
                .await
                .unwrap();

        assert_eq!(ci.len(), 1);
        assert!(ci_baseline_established);
        assert!(
            events.is_empty(),
            "first poll after startup should prime CI baseline without emitting historical events"
        );

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
}
