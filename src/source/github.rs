use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
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
    run_job_count: usize,
    run_all_terminal: bool,
    channel: Option<String>,
    mention: Option<String>,
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
            run_job_count: payload
                .and_then(|object| object.get("run_job_count"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1) as usize,
            run_all_terminal: payload
                .and_then(|object| object.get("run_all_terminal"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            channel: event.channel.clone(),
            mention: event.mention.clone(),
        }
    }

    fn same_event(&self, other: &Self) -> bool {
        if !self.identities.is_empty()
            && self.identities.iter().any(|identity| {
                other
                    .identities
                    .iter()
                    .any(|candidate| candidate == identity)
            })
        {
            return true;
        }
        self.run_id.is_some() && self.run_id == other.run_id && self.workflow == other.workflow
            || self.sha == other.sha
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

const MAX_BASELINE_RUNS_PER_REPO: usize = 256;
const CI_PAGE_SIZE: usize = 100;
const MAX_CI_PAGES: usize = 5;

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
        let created_rank = u8::from(self.created_at.is_none());
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
    fn record_terminal_runs(
        &mut self,
        repo_key: &str,
        repo_path: &str,
        current: &HashMap<String, GitHubCISnapshot>,
    ) -> bool {
        let mut incoming: Vec<&GitHubCISnapshot> =
            current.values().filter(|ci| ci.is_terminal()).collect();
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

fn merge_repo_baseline(dest: &mut RepoCIBaseline, source: RepoCIBaseline) {
    for record in source.terminal_runs {
        if let Some(existing) = dest.terminal_runs.iter_mut().find(|candidate| {
            candidate
                .identities
                .iter()
                .any(|identity| record.contains_identity(identity))
        }) {
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
    for delivery in source.pending {
        if !dest
            .pending
            .iter()
            .any(|existing| existing.same_event(&delivery))
        {
            dest.pending.push(delivery);
        }
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

struct BaselineLock {
    lock_path: PathBuf,
}

impl Drop for BaselineLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.lock_path);
    }
}

fn acquire_baseline_lock(path: &Path) -> Result<BaselineLock> {
    let lock_path = path.with_extension("json.lock");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    for _ in 0..200 {
        match fs::create_dir(&lock_path) {
            Ok(()) => return Ok(BaselineLock { lock_path }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err("timed out waiting for GitHub CI baseline lock".into())
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
    if let Err(error) = fs::write(&temp_path, payload.as_bytes()) {
        let _ = fs::remove_file(&temp_path);
        return Err(error.into());
    }
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error.into());
    }
    Ok(())
}

fn commit_ci_baseline(path: Option<&Path>, local: &CIBaseline) -> Result<CIBaseline> {
    let Some(path) = path else {
        return Ok(local.clone());
    };
    let _lock = acquire_baseline_lock(path)?;
    let mut disk = load_ci_baseline(Some(path));
    merge_ci_baseline(&mut disk, local);
    write_ci_baseline_atomic(&disk, path)?;
    Ok(disk)
}

#[cfg(test)]
fn save_ci_baseline(baseline: &CIBaseline, path: Option<&Path>) -> Result<()> {
    commit_ci_baseline(path, baseline).map(|_| ())
}

async fn drain_pending_outbox(
    ci_baseline: &mut CIBaseline,
    path: Option<&Path>,
    tx: &mpsc::Sender<IncomingEvent>,
) -> Result<()> {
    let pending: Vec<PendingCiDelivery> = ci_baseline
        .repos
        .values()
        .flat_map(|repo| repo.pending.iter().cloned())
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    for delivery in &pending {
        send_event(tx, delivery.clone().into_event()).await?;
    }
    ack_pending_deliveries(ci_baseline, path, &pending)?;
    Ok(())
}

fn ack_pending_deliveries(
    ci_baseline: &mut CIBaseline,
    path: Option<&Path>,
    delivered: &[PendingCiDelivery],
) -> Result<()> {
    for repo in ci_baseline.repos.values_mut() {
        repo.pending
            .retain(|pending| !delivered.iter().any(|item| pending.same_event(item)));
    }
    let Some(path) = path else {
        return Ok(());
    };
    let _lock = acquire_baseline_lock(path)?;
    let mut disk = load_ci_baseline(Some(path));
    merge_ci_baseline(&mut disk, ci_baseline);
    for repo in disk.repos.values_mut() {
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
        if let Some(run_id) = &self.run_id {
            return format!("run:{run_id}:{}:{}", self.run_attempt(), self.workflow);
        }
        if let Some(check_run_id) = &self.check_run_id {
            return format!("check:{check_run_id}");
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
    drain_pending_outbox(ci_baseline, ci_baseline_path, tx).await?;

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
        let (ci, ci_baseline_established, ci_events) = match poll_ci_statuses(
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
                )
            }
        };

        // Durable outbox: persist pending receipts, then publish, then
        // drop the outbox. Persistence failure must not mutate in-memory
        // suppression state. A crash after persist and before send retries
        // from the outbox on the next poll.
        let mut next_baseline = ci_baseline.clone();
        next_baseline.record_terminal_runs(&repo_key, &repo.path, &ci);
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
            .map(|event| PendingCiDelivery::from_event(event, Vec::new()))
            .collect();
        for event in ci_events {
            send_event(tx, event).await?;
        }
        if let Err(error) = ack_pending_deliveries(ci_baseline, ci_baseline_path, &delivered) {
            eprintln!(
                "clawhip source github CI baseline outbox ack failed for {}: {error}",
                repo.path
            );
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
) -> Result<(HashMap<String, GitHubCISnapshot>, bool, Vec<IncomingEvent>)> {
    if !repo.emit_pr_status {
        return Ok((
            previous.map(|entry| entry.ci.clone()).unwrap_or_default(),
            previous
                .map(|entry| entry.ci_baseline_established)
                .unwrap_or(false),
            Vec::new(),
        ));
    }

    let Some(client) = github_client else {
        return Ok((
            previous.map(|entry| entry.ci.clone()).unwrap_or_default(),
            previous
                .map(|entry| entry.ci_baseline_established)
                .unwrap_or(false),
            Vec::new(),
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
            Ok((ci, window_complete, events))
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
    let run_id = payload.get("run_id").and_then(|value| value.as_str());
    previous.values().find(|ci| {
        if let Some(run_id) = run_id
            && ci.run_id.as_deref() == Some(run_id)
        {
            return true;
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
            Some(old) => old.status != ci.status || old.conclusion != ci.conclusion,
            None => {
                if !previous_ci_baseline_established {
                    // First poll after startup or re-priming after a partial
                    // poll: seed the snapshot. In-progress runs emit; terminal
                    // runs are historical noise, not transitions (#317).
                    ci.status != "completed"
                } else if ci.is_terminal() {
                    // Established monitor seeing a terminal run with no
                    // in-process record: the pagination drift / cursor loss /
                    // re-enrollment replay window. Suppress it when the
                    // persisted baseline already observed it (#317).
                    !repo_ci_baseline_is_suppressed(repo_ci_baseline, ci)
                } else {
                    // Established monitor seeing a genuinely new run start.
                    true
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

    let (workflow_runs, complete) =
        fetch_direct_workflow_runs(client, api_base, &github_repo, snapshot).await?;
    window_complete &= complete;
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
                let (run_job_count, run_all_terminal) = run_id
                    .as_deref()
                    .and_then(|id| run_summaries.get(id).copied())
                    .unwrap_or((1, check_run.status == "completed"));
                GitHubCISnapshot {
                    pr_number: Some(pr_number),
                    workflow: check_run.name,
                    status: check_run.status,
                    conclusion: check_run.conclusion,
                    sha: check_run.head_sha,
                    url,
                    branch: Some(pr.head_branch.clone()),
                    run_id,
                    run_attempt: 1,
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
) -> Result<(Vec<GitHubCISnapshot>, bool)> {
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
    ))
}

fn workflow_run_id(url: &str) -> Option<String> {
    url.split("/actions/runs/")
        .nth(1)
        .and_then(|tail| tail.split('/').next())
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
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
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
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

        let (ci, ci_baseline_established, events) =
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
                    "workflow_runs": [
                        {
                            "id": 123_u64,
                            "name": "CI",
                            "status": "completed",
                            "conclusion": "failure",
                            "head_branch": "feat/pr",
                            "head_sha": "prsha",
                            "html_url": "https://github.com/org/repo/actions/runs/123",
                            "pull_requests": [{"number": 42}]
                        },
                        {
                            "id": 456_u64,
                            "name": "Rust CI",
                            "status": "completed",
                            "conclusion": "failure",
                            "head_branch": "main",
                            "head_sha": "mainsha",
                            "html_url": "https://github.com/org/repo/actions/runs/456",
                            "pull_requests": []
                        }
                    ]
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
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("GET /repos/org/repo/commits/prsha/check-runs?"));
        assert!(requests[1].contains("GET /repos/org/repo/actions/runs?"));
        assert!(requests[1].contains("branch=main"));
        assert!(requests[1].contains("event=push"));
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
