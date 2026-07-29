use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::Result;
use crate::config::{AppConfig, GitRepoMonitor};
use crate::events::IncomingEvent;
use crate::source::Source;
use crate::telemetry;

const INVALID_FAILURE_THRESHOLD: u32 = 3;
const MAX_FAILURE_ATTEMPTS: u32 = 32;
const SUPPRESSED_SUMMARY_INTERVAL: u32 = 10;
const QUARANTINE_PROBE_INTERVAL: Duration = Duration::from_secs(15 * 60);

pub type SharedGitMonitorDiagnostics = Arc<Mutex<GitMonitorLifecycleCounts>>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct GitMonitorLifecycleCounts {
    pub active: usize,
    pub degraded: usize,
    pub quarantined: usize,
    pub retired: usize,
}

pub fn new_shared_git_monitor_diagnostics() -> SharedGitMonitorDiagnostics {
    Arc::new(Mutex::new(GitMonitorLifecycleCounts::default()))
}

pub fn snapshot_git_monitor_diagnostics(
    diagnostics: &SharedGitMonitorDiagnostics,
) -> GitMonitorLifecycleCounts {
    match diagnostics.lock() {
        Ok(counts) => *counts,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

pub struct GitSource {
    config: Arc<AppConfig>,
    diagnostics: SharedGitMonitorDiagnostics,
}

impl GitSource {
    pub fn new(config: Arc<AppConfig>, diagnostics: SharedGitMonitorDiagnostics) -> Self {
        Self {
            config,
            diagnostics,
        }
    }
}

#[async_trait::async_trait]
impl Source for GitSource {
    fn name(&self) -> &str {
        "git"
    }

    async fn run(&self, tx: mpsc::Sender<IncomingEvent>) -> Result<()> {
        let mut state = HashMap::new();

        loop {
            poll_git(self.config.as_ref(), &tx, &mut state, &self.diagnostics).await?;
            sleep(Duration::from_secs(
                self.config.monitors.poll_interval_secs.max(1),
            ))
            .await;
        }
    }
}

#[derive(Debug)]
struct GitRepoState {
    branch: String,
    head: String,
}

#[derive(Debug, Default)]
struct GitMonitorState {
    repo: Option<GitRepoState>,
    failure: Option<GitMonitorFailureState>,
    origin: Option<GitMonitorOrigin>,
    identity: Option<GitPathIdentity>,
    lifecycle: GitMonitorLifecycle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GitMonitorFailureState {
    classification: GitMonitorFailureClass,
    operation: GitMonitorOperation,
    message: String,
    attempts: u32,
    suppressed_polls: u32,
    next_retry_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitMonitorOperation {
    Discovery,
    Snapshot,
}

impl GitMonitorOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "worktree discovery",
            Self::Snapshot => "snapshot",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum GitMonitorLifecycle {
    #[default]
    Active,
    Degraded,
    Quarantined,
    Retired,
}

impl GitMonitorLifecycle {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Degraded => "degraded",
            Self::Quarantined => "quarantined",
            Self::Retired => "retired",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitMonitorOrigin {
    Declarative,
    Dynamic,
}

impl GitMonitorOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Declarative => "declarative",
            Self::Dynamic => "dynamic",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GitPathIdentity {
    root: Option<FileIdentity>,
    git_marker: Option<FileIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    is_dir: bool,
    is_file: bool,
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GitMonitorFailureClass {
    Missing,
    NotGit,
    GitdirBroken,
    Unknown,
}

impl GitMonitorFailureClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::NotGit => "not-git",
            Self::GitdirBroken => "gitdir-broken",
            Self::Unknown => "unknown",
        }
    }

    fn is_permanently_invalid(self) -> bool {
        matches!(self, Self::Missing | Self::NotGit | Self::GitdirBroken)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MonitoredGitPath {
    state_key: String,
    repo_path: String,
    worktree_path: String,
    origin: GitMonitorOrigin,
    identity: GitPathIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommitEntry {
    pub(crate) sha: String,
    pub(crate) summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitSnapshot {
    pub(crate) repo_name: String,
    pub(crate) repo_path: String,
    pub(crate) worktree_path: String,
    pub(crate) branch: String,
    pub(crate) head: String,
    pub(crate) commits: Vec<CommitEntry>,
    pub(crate) github_repo: Option<String>,
}

async fn poll_git(
    config: &AppConfig,
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut HashMap<String, GitMonitorState>,
    diagnostics: &SharedGitMonitorDiagnostics,
) -> Result<()> {
    poll_git_at(config, tx, state, diagnostics, Instant::now()).await
}

async fn poll_git_at(
    config: &AppConfig,
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut HashMap<String, GitMonitorState>,
    diagnostics: &SharedGitMonitorDiagnostics,
    now: Instant,
) -> Result<()> {
    let mut active_keys = HashSet::new();
    let poll_interval = Duration::from_secs(config.monitors.poll_interval_secs.max(1));

    for repo in &config.monitors.git.repos {
        let discovery_key = discovery_state_key(repo);
        let discovery_identity = git_path_identity(&repo.path);
        active_keys.insert(discovery_key.clone());
        reconcile_monitor_registration(
            state.entry(discovery_key.clone()).or_default(),
            GitMonitorOrigin::Declarative,
            discovery_identity.clone(),
            &repo.path,
            "worktree discovery",
        );

        if let Some(discovery_state) = state.get_mut(&discovery_key)
            && should_skip_failed_monitor(discovery_state, &repo.path, now)
        {
            preserve_repo_monitor_keys(state, &mut active_keys, repo);
            continue;
        }

        let monitored_paths = match discover_monitored_git_paths(repo).await {
            Ok(monitored_paths) => {
                if let Some(discovery_state) = state.get_mut(&discovery_key) {
                    clear_monitor_failure(
                        discovery_state,
                        &repo.path,
                        GitMonitorOperation::Discovery,
                    );
                }
                monitored_paths
            }
            Err(error) => {
                record_monitor_failure(
                    state.entry(discovery_key).or_default(),
                    MonitorFailureInput {
                        origin: GitMonitorOrigin::Declarative,
                        expected_identity: &discovery_identity,
                        path: &repo.path,
                        operation: GitMonitorOperation::Discovery,
                        message: error.to_string(),
                        now,
                        poll_interval,
                    },
                );
                preserve_repo_monitor_keys(state, &mut active_keys, repo);
                continue;
            }
        };

        for monitored in monitored_paths {
            active_keys.insert(monitored.state_key.clone());
            let monitor_state = state.entry(monitored.state_key.clone()).or_default();
            reconcile_monitor_registration(
                monitor_state,
                monitored.origin,
                monitored.identity.clone(),
                &monitored.worktree_path,
                "snapshot",
            );

            if should_skip_failed_monitor(monitor_state, &monitored.worktree_path, now) {
                continue;
            }

            match snapshot_git_worktree(repo, &monitored).await {
                Ok(snapshot) => {
                    clear_monitor_failure(
                        monitor_state,
                        &monitored.worktree_path,
                        GitMonitorOperation::Snapshot,
                    );
                    if let Some(previous) = monitor_state.repo.as_ref() {
                        if repo.emit_branch_changes && previous.branch != snapshot.branch {
                            send_event(
                                tx,
                                IncomingEvent::git_branch_changed(
                                    snapshot.repo_name.clone(),
                                    previous.branch.clone(),
                                    snapshot.branch.clone(),
                                    repo.channel.clone(),
                                )
                                .with_repo_context(
                                    Some(snapshot.repo_path.clone()),
                                    Some(snapshot.worktree_path.clone()),
                                )
                                .with_mention(repo.mention.clone())
                                .with_format(repo.format.clone()),
                            )
                            .await?;
                        }
                        if repo.emit_commits && previous.head != snapshot.head {
                            let commits = list_new_commits_for_path(
                                &snapshot.worktree_path,
                                &previous.head,
                                &snapshot.head,
                            )
                            .await
                            .ok()
                            .filter(|entries| !entries.is_empty())
                            .unwrap_or_else(|| snapshot.commits.clone());
                            let events = IncomingEvent::git_commit_events(
                                snapshot.repo_name.clone(),
                                snapshot.branch.clone(),
                                commits
                                    .into_iter()
                                    .map(|commit| (commit.sha, commit.summary))
                                    .collect(),
                                repo.channel.clone(),
                            );
                            for event in events {
                                send_event(
                                    tx,
                                    event
                                        .with_repo_context(
                                            Some(snapshot.repo_path.clone()),
                                            Some(snapshot.worktree_path.clone()),
                                        )
                                        .with_mention(repo.mention.clone())
                                        .with_format(repo.format.clone()),
                                )
                                .await?;
                            }
                        }
                    }

                    monitor_state.repo = Some(GitRepoState {
                        branch: snapshot.branch,
                        head: snapshot.head,
                    });
                }
                Err(error) => record_monitor_failure(
                    monitor_state,
                    MonitorFailureInput {
                        origin: monitored.origin,
                        expected_identity: &monitored.identity,
                        path: &monitored.worktree_path,
                        operation: GitMonitorOperation::Snapshot,
                        message: error.to_string(),
                        now,
                        poll_interval,
                    },
                ),
            }
        }
    }

    state.retain(|key, _| active_keys.contains(key));
    update_git_monitor_diagnostics(diagnostics, state);

    Ok(())
}

async fn discover_monitored_git_paths(repo: &GitRepoMonitor) -> Result<Vec<MonitoredGitPath>> {
    let output = run_command(
        &git_bin(),
        &["-C", &repo.path, "worktree", "list", "--porcelain"],
    )
    .await?;

    let mut seen = HashSet::new();
    let mut monitored = Vec::new();
    for worktree_path in parse_worktree_list(&output) {
        if seen.insert(worktree_path.clone()) {
            let origin = monitor_origin(repo, &worktree_path);
            monitored.push(MonitoredGitPath {
                state_key: monitored_state_key(repo, &worktree_path),
                repo_path: repo.path.clone(),
                identity: git_path_identity(&worktree_path),
                worktree_path,
                origin,
            });
        }
    }

    if monitored.is_empty() {
        monitored.push(MonitoredGitPath {
            state_key: monitored_state_key(repo, &repo.path),
            repo_path: repo.path.clone(),
            worktree_path: repo.path.clone(),
            origin: GitMonitorOrigin::Declarative,
            identity: git_path_identity(&repo.path),
        });
    }

    Ok(monitored)
}

fn parse_worktree_list(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(ToString::to_string)
        .collect()
}

async fn send_event(tx: &mpsc::Sender<IncomingEvent>, event: IncomingEvent) -> Result<()> {
    tx.send(event)
        .await
        .map_err(|error| format!("git source channel closed: {error}").into())
}

pub(crate) async fn snapshot_git_repo(repo: &GitRepoMonitor) -> Result<GitSnapshot> {
    snapshot_git_worktree(
        repo,
        &MonitoredGitPath {
            state_key: repo.path.clone(),
            repo_path: repo.path.clone(),
            worktree_path: repo.path.clone(),
            origin: GitMonitorOrigin::Declarative,
            identity: git_path_identity(&repo.path),
        },
    )
    .await
}

async fn snapshot_git_worktree(
    repo: &GitRepoMonitor,
    monitored: &MonitoredGitPath,
) -> Result<GitSnapshot> {
    let head = run_command(
        &git_bin(),
        &["-C", &monitored.worktree_path, "rev-parse", "HEAD"],
    )
    .await?;
    let branch = run_command(
        &git_bin(),
        &[
            "-C",
            &monitored.worktree_path,
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
        ],
    )
    .await?;
    let summary = run_command(
        &git_bin(),
        &["-C", &monitored.worktree_path, "log", "-1", "--pretty=%s"],
    )
    .await?;
    let remote_url = run_command(
        &git_bin(),
        &[
            "-C",
            &monitored.worktree_path,
            "config",
            "--get",
            &format!("remote.{}.url", repo.remote),
        ],
    )
    .await
    .unwrap_or_default();

    Ok(GitSnapshot {
        repo_name: repo_display_name(repo),
        repo_path: monitored.repo_path.clone(),
        worktree_path: monitored.worktree_path.clone(),
        branch,
        head: head.clone(),
        commits: vec![CommitEntry { sha: head, summary }],
        github_repo: repo
            .github_repo
            .clone()
            .or_else(|| parse_github_repo(&remote_url)),
    })
}

async fn list_new_commits_for_path(path: &str, old: &str, new: &str) -> Result<Vec<CommitEntry>> {
    let output = run_command(
        &git_bin(),
        &[
            "-C",
            path,
            "log",
            "--reverse",
            "--pretty=%H%x1f%s",
            &format!("{old}..{new}"),
        ],
    )
    .await?;

    Ok(output
        .lines()
        .filter_map(|line| {
            let (sha, summary) = line.split_once('\u{1f}')?;
            Some(CommitEntry {
                sha: sha.to_string(),
                summary: summary.to_string(),
            })
        })
        .collect())
}

pub(crate) async fn run_command(binary: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(binary).args(args).output().await?;
    if output.status.success() {
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    } else {
        Err(format!(
            "{} {:?} failed: {}",
            binary,
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into())
    }
}

pub(crate) fn repo_display_name(repo: &GitRepoMonitor) -> String {
    repo.name.clone().unwrap_or_else(|| {
        Path::new(&repo.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&repo.path)
            .to_string()
    })
}

pub(crate) fn git_bin() -> String {
    std::env::var("CLAWHIP_GIT_BIN").unwrap_or_else(|_| "git".to_string())
}

fn discovery_state_key(repo: &GitRepoMonitor) -> String {
    format!("discovery::{}", repo.path)
}

fn monitored_state_key(repo: &GitRepoMonitor, worktree_path: &str) -> String {
    if monitor_origin(repo, worktree_path) == GitMonitorOrigin::Declarative {
        discovery_state_key(repo)
    } else {
        format!("path::{}::{worktree_path}", repo.path)
    }
}

fn monitor_origin(repo: &GitRepoMonitor, worktree_path: &str) -> GitMonitorOrigin {
    if paths_match(&repo.path, worktree_path) {
        GitMonitorOrigin::Declarative
    } else {
        GitMonitorOrigin::Dynamic
    }
}

fn paths_match(left: &str, right: &str) -> bool {
    if Path::new(left) == Path::new(right) {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn repo_state_prefix(repo: &GitRepoMonitor) -> String {
    format!("path::{}::", repo.path)
}

fn preserve_repo_monitor_keys(
    state: &HashMap<String, GitMonitorState>,
    active_keys: &mut HashSet<String>,
    repo: &GitRepoMonitor,
) {
    let prefix = repo_state_prefix(repo);
    for key in state.keys() {
        if key.starts_with(&prefix) {
            active_keys.insert(key.clone());
        }
    }
}

fn git_path_identity(path: &str) -> GitPathIdentity {
    let raw_path = Path::new(path);
    if let Ok(canonical_path) = fs::canonicalize(raw_path) {
        return GitPathIdentity {
            root: file_identity(&canonical_path),
            git_marker: file_identity(&canonical_path.join(".git")),
        };
    }

    GitPathIdentity {
        root: file_identity(raw_path),
        git_marker: file_identity(&raw_path.join(".git")),
    }
}

fn file_identity(path: &Path) -> Option<FileIdentity> {
    let metadata = fs::symlink_metadata(path).ok()?;
    let file_type = metadata.file_type();
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    Some(FileIdentity {
        is_dir: file_type.is_dir(),
        is_file: file_type.is_file(),
        len: if file_type.is_file() {
            metadata.len()
        } else {
            0
        },
        modified: if file_type.is_file() {
            metadata.modified().ok()
        } else {
            None
        },
        created: metadata.created().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    })
}

fn reconcile_monitor_registration(
    state: &mut GitMonitorState,
    origin: GitMonitorOrigin,
    identity: GitPathIdentity,
    path: &str,
    context: &str,
) {
    let changed = state.origin.is_some_and(|previous| previous != origin)
        || state
            .identity
            .as_ref()
            .is_some_and(|previous| previous != &identity);
    let was_unhealthy = state.lifecycle != GitMonitorLifecycle::Active || state.failure.is_some();
    let previous = changed.then(|| state.failure.take()).flatten();

    if state.origin.is_none() || changed {
        state.origin = Some(origin);
        state.identity = Some(identity);
        state.repo = None;
        state.failure = None;
        state.lifecycle = GitMonitorLifecycle::Active;
    }

    if changed && was_unhealthy {
        emit_monitor_recovery(
            state,
            previous.as_ref(),
            path,
            context,
            "source_identity_changed",
        );
    }
}

fn should_skip_failed_monitor(state: &mut GitMonitorState, path: &str, now: Instant) -> bool {
    let current_identity = git_path_identity(path);
    if state.identity.as_ref() != Some(&current_identity) {
        let origin = state.origin.unwrap_or(GitMonitorOrigin::Declarative);
        reconcile_monitor_registration(state, origin, current_identity, path, "identity probe");
        return false;
    }

    if state.lifecycle == GitMonitorLifecycle::Retired {
        return true;
    }

    let Some(failure) = state.failure.as_mut() else {
        return false;
    };
    if now < failure.next_retry_at {
        failure.suppressed_polls = failure
            .suppressed_polls
            .saturating_add(1)
            .min(SUPPRESSED_SUMMARY_INTERVAL);
        if failure.suppressed_polls == SUPPRESSED_SUMMARY_INTERVAL {
            telemetry::emit(source_record(SourceTelemetryInput {
                event_name: telemetry::event_name::SOURCE_INVENTORY,
                reason_code: "source_suppressed_summary",
                source: "git",
                path: Some(path),
                origin: state.origin.map(GitMonitorOrigin::as_str),
                lifecycle: Some(state.lifecycle.as_str()),
                classification: Some(failure.classification.as_str()),
                message: Some(&failure.message),
                attempts: Some(failure.attempts),
                suppressed_polls: Some(failure.suppressed_polls),
            }));
            failure.suppressed_polls = 0;
        }
        return true;
    }
    false
}

fn clear_monitor_failure(state: &mut GitMonitorState, path: &str, operation: GitMonitorOperation) {
    if state
        .failure
        .as_ref()
        .is_some_and(|failure| failure.operation != operation)
    {
        return;
    }
    let Some(previous) = state.failure.take() else {
        state.lifecycle = GitMonitorLifecycle::Active;
        return;
    };
    state.lifecycle = GitMonitorLifecycle::Active;
    emit_monitor_recovery(
        state,
        Some(&previous),
        path,
        operation.as_str(),
        "source_recovered",
    );
}

fn emit_monitor_recovery(
    state: &GitMonitorState,
    previous: Option<&GitMonitorFailureState>,
    path: &str,
    context: &str,
    reason_code: &str,
) {
    telemetry::emit(source_record(SourceTelemetryInput {
        event_name: "source_recovered",
        reason_code,
        source: "git",
        path: Some(path),
        origin: state.origin.map(GitMonitorOrigin::as_str),
        lifecycle: Some(GitMonitorLifecycle::Active.as_str()),
        classification: previous.map(|failure| failure.classification.as_str()),
        message: None,
        attempts: previous.map(|failure| failure.attempts),
        suppressed_polls: previous.map(|failure| failure.suppressed_polls),
    }));
    eprintln!(
        "clawhip source git {context} recovered for {path} after {} bounded failure(s)",
        previous.map(|failure| failure.attempts).unwrap_or_default()
    );
}

struct MonitorFailureInput<'a> {
    origin: GitMonitorOrigin,
    expected_identity: &'a GitPathIdentity,
    path: &'a str,
    operation: GitMonitorOperation,
    message: String,
    now: Instant,
    poll_interval: Duration,
}

fn record_monitor_failure(state: &mut GitMonitorState, input: MonitorFailureInput<'_>) {
    let MonitorFailureInput {
        origin,
        expected_identity,
        path,
        operation,
        message,
        now,
        poll_interval,
    } = input;
    let current_identity = git_path_identity(path);
    if &current_identity != expected_identity {
        reconcile_monitor_registration(state, origin, current_identity, path, operation.as_str());
        return;
    }

    let classification = classify_git_monitor_failure(&message);
    let previous_lifecycle = state.lifecycle;
    let same_failure = state.failure.as_ref().is_some_and(|previous| {
        previous.classification == classification && previous.operation == operation
    });
    let attempts = if same_failure {
        state
            .failure
            .as_ref()
            .map(|previous| previous.attempts.saturating_add(1))
            .unwrap_or(1)
    } else {
        1
    }
    .min(MAX_FAILURE_ATTEMPTS);
    let lifecycle =
        if classification.is_permanently_invalid() && attempts >= INVALID_FAILURE_THRESHOLD {
            match origin {
                GitMonitorOrigin::Declarative => GitMonitorLifecycle::Quarantined,
                GitMonitorOrigin::Dynamic => GitMonitorLifecycle::Retired,
            }
        } else {
            GitMonitorLifecycle::Degraded
        };
    let next_retry = match lifecycle {
        GitMonitorLifecycle::Quarantined => QUARANTINE_PROBE_INTERVAL,
        GitMonitorLifecycle::Retired => Duration::ZERO,
        _ => git_monitor_backoff(attempts, poll_interval),
    };
    let transition = !same_failure || lifecycle != previous_lifecycle;
    let reason_code = match lifecycle {
        GitMonitorLifecycle::Degraded => match operation {
            GitMonitorOperation::Discovery => "source_discovery_failed",
            GitMonitorOperation::Snapshot => "source_snapshot_failed",
        },
        GitMonitorLifecycle::Quarantined => "source_quarantined",
        GitMonitorLifecycle::Retired => "source_retired",
        GitMonitorLifecycle::Active => "source_recovered",
    };

    state.origin = Some(origin);
    state.identity = Some(current_identity);
    state.lifecycle = lifecycle;
    state.failure = Some(GitMonitorFailureState {
        classification,
        operation,
        message: message.clone(),
        attempts,
        suppressed_polls: 0,
        next_retry_at: now + next_retry,
    });

    if transition {
        telemetry::emit(source_record(SourceTelemetryInput {
            event_name: telemetry::event_name::SOURCE_DEGRADED,
            reason_code,
            source: "git",
            path: Some(path),
            origin: Some(origin.as_str()),
            lifecycle: Some(lifecycle.as_str()),
            classification: Some(classification.as_str()),
            message: Some(&message),
            attempts: Some(attempts),
            suppressed_polls: Some(0),
        }));
        eprintln!(
            "clawhip source git {} {} for {path}: origin={}, class={}, attempts={}, next_probe_secs={}, error={message}",
            operation.as_str(),
            lifecycle.as_str(),
            origin.as_str(),
            classification.as_str(),
            attempts,
            next_retry.as_secs()
        );
    }
}

fn update_git_monitor_diagnostics(
    diagnostics: &SharedGitMonitorDiagnostics,
    state: &HashMap<String, GitMonitorState>,
) {
    let mut counts = GitMonitorLifecycleCounts::default();
    for monitor in state.values() {
        match monitor.lifecycle {
            GitMonitorLifecycle::Active => counts.active += 1,
            GitMonitorLifecycle::Degraded => counts.degraded += 1,
            GitMonitorLifecycle::Quarantined => counts.quarantined += 1,
            GitMonitorLifecycle::Retired => counts.retired += 1,
        }
    }
    match diagnostics.lock() {
        Ok(mut current) => *current = counts,
        Err(poisoned) => *poisoned.into_inner() = counts,
    }
}

fn git_monitor_backoff(attempts: u32, poll_interval: Duration) -> Duration {
    let multiplier = 2u32.saturating_pow(attempts.min(6));
    let capped = poll_interval
        .as_secs()
        .saturating_mul(multiplier.into())
        .min(300);
    Duration::from_secs(capped.max(1))
}

struct SourceTelemetryInput<'a> {
    event_name: &'a str,
    reason_code: &'a str,
    source: &'a str,
    path: Option<&'a str>,
    origin: Option<&'a str>,
    lifecycle: Option<&'a str>,
    classification: Option<&'a str>,
    message: Option<&'a str>,
    attempts: Option<u32>,
    suppressed_polls: Option<u32>,
}

fn source_record(input: SourceTelemetryInput<'_>) -> serde_json::Map<String, serde_json::Value> {
    let correlation = format!(
        "source:{}:{}",
        input.source,
        input.path.unwrap_or("inventory")
    );
    let mut record = telemetry::record(input.event_name, input.reason_code, correlation);
    record.insert("source".to_string(), serde_json::json!(input.source));
    if let Some(path) = input.path {
        record.insert("path".to_string(), serde_json::json!(path));
    }
    if let Some(origin) = input.origin {
        record.insert("origin".to_string(), serde_json::json!(origin));
    }
    if let Some(lifecycle) = input.lifecycle {
        record.insert("lifecycle".to_string(), serde_json::json!(lifecycle));
    }
    if let Some(classification) = input.classification {
        record.insert(
            "classification".to_string(),
            serde_json::json!(classification),
        );
    }
    if let Some(message) = input.message {
        record.insert("error".to_string(), serde_json::json!(message));
    }
    if let Some(attempts) = input.attempts {
        record.insert("attempts".to_string(), serde_json::json!(attempts));
    }
    if let Some(suppressed_polls) = input.suppressed_polls {
        record.insert(
            "suppressed_polls".to_string(),
            serde_json::json!(suppressed_polls),
        );
    }
    record
}

fn classify_git_monitor_failure(message: &str) -> GitMonitorFailureClass {
    let lowered = message.to_ascii_lowercase();
    if lowered.contains("no such file or directory") || lowered.contains("path not found") {
        GitMonitorFailureClass::Missing
    } else if lowered.contains("not a git repository")
        && (lowered.contains(".git/worktrees") || lowered.contains("gitdir"))
    {
        GitMonitorFailureClass::GitdirBroken
    } else if lowered.contains("not a git repository") {
        GitMonitorFailureClass::NotGit
    } else {
        GitMonitorFailureClass::Unknown
    }
}

pub(crate) fn parse_github_repo(remote: &str) -> Option<String> {
    let trimmed = remote.trim().trim_end_matches(".git");
    if let Some(rest) = trimmed.strip_prefix("git@github.com:") {
        return Some(rest.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("https://github.com/") {
        return Some(rest.to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("ssh://git@github.com/") {
        return Some(rest.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn classifies_common_invalid_monitor_failures() {
        assert_eq!(
            classify_git_monitor_failure(
                "fatal: cannot change to '/tmp/missing': No such file or directory"
            ),
            GitMonitorFailureClass::Missing
        );
        assert_eq!(
            classify_git_monitor_failure(
                "fatal: not a git repository (or any of the parent directories): .git"
            ),
            GitMonitorFailureClass::NotGit
        );
        assert_eq!(
            classify_git_monitor_failure(
                "fatal: not a git repository: /tmp/repo/.git/worktrees/issue-129"
            ),
            GitMonitorFailureClass::GitdirBroken
        );
        assert_eq!(
            classify_git_monitor_failure(
                "fatal: cannot change to '/root/private': Permission denied"
            ),
            GitMonitorFailureClass::Unknown
        );
    }

    #[test]
    fn git_monitor_backoff_grows_and_caps() {
        let poll_interval = Duration::from_secs(3);
        assert_eq!(
            git_monitor_backoff(1, poll_interval),
            Duration::from_secs(6)
        );
        assert_eq!(
            git_monitor_backoff(2, poll_interval),
            Duration::from_secs(12)
        );
        assert_eq!(
            git_monitor_backoff(6, poll_interval),
            Duration::from_secs(192)
        );
        assert_eq!(
            git_monitor_backoff(7, poll_interval),
            Duration::from_secs(192)
        );
        assert_eq!(
            git_monitor_backoff(6, Duration::from_secs(30)),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn parses_github_repo_urls() {
        assert_eq!(
            parse_github_repo("git@github.com:bellman/clawhip.git"),
            Some("bellman/clawhip".to_string())
        );
        assert_eq!(
            parse_github_repo("https://github.com/bellman/clawhip.git"),
            Some("bellman/clawhip".to_string())
        );
    }

    #[test]
    fn parses_worktree_list_output() {
        let output = "worktree /repo/root\nHEAD abc\nbranch refs/heads/main\n\nworktree /repo/.worktrees/issue-115\nHEAD def\nbranch refs/heads/feat/issue-115\n";
        assert_eq!(
            parse_worktree_list(output),
            vec![
                "/repo/root".to_string(),
                "/repo/.worktrees/issue-115".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn poll_git_emits_branch_and_commit_events_for_linked_worktree() {
        let sandbox = TempDir::new().unwrap();
        let root = sandbox.path().join("repo");
        let worktree = sandbox.path().join("repo-issue-115");
        init_repo(&root).await;
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "feat/issue-115",
                path_str(&worktree),
            ],
        )
        .await;

        let repo = GitRepoMonitor {
            path: path_str(&root).to_string(),
            name: Some("clawhip".into()),
            ..GitRepoMonitor::default()
        };
        let config = config_with_repo(repo);
        let diagnostics = new_shared_git_monitor_diagnostics();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = HashMap::new();

        poll_git(&config, &tx, &mut state, &diagnostics)
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(snapshot_git_monitor_diagnostics(&diagnostics).active, 2);

        git(&worktree, &["checkout", "-b", "feat/issue-115-v2"]).await;
        poll_git(&config, &tx, &mut state, &diagnostics)
            .await
            .unwrap();
        let branch_event = rx.try_recv().unwrap();
        assert_eq!(branch_event.kind, "git.branch-changed");
        assert_eq!(branch_event.payload["repo"], "clawhip");
        assert_eq!(branch_event.payload["repo_path"], path_str(&root));
        assert_eq!(branch_event.payload["worktree_path"], path_str(&worktree));
        assert_eq!(branch_event.payload["old_branch"], "feat/issue-115");
        assert_eq!(branch_event.payload["new_branch"], "feat/issue-115-v2");
        assert!(rx.try_recv().is_err());

        std::fs::write(worktree.join("worktree.txt"), "hello from worktree\n").unwrap();
        git(&worktree, &["add", "worktree.txt"]).await;
        git(&worktree, &["commit", "-m", "worktree commit"]).await;

        poll_git(&config, &tx, &mut state, &diagnostics)
            .await
            .unwrap();
        let commit_event = rx.try_recv().unwrap();
        assert_eq!(commit_event.kind, "git.commit");
        assert_eq!(commit_event.payload["repo"], "clawhip");
        assert_eq!(commit_event.payload["repo_path"], path_str(&root));
        assert_eq!(commit_event.payload["worktree_path"], path_str(&worktree));
        assert_eq!(commit_event.payload["branch"], "feat/issue-115-v2");
        assert_eq!(commit_event.payload["summary"], "worktree commit");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn missing_declarative_monitor_quarantines_then_recovers_on_identity_change() {
        let sandbox = TempDir::new().unwrap();
        let missing = sandbox.path().join("missing-repo");
        let repo = GitRepoMonitor {
            path: path_str(&missing).to_string(),
            ..GitRepoMonitor::default()
        };
        let config = config_with_repo(repo.clone());
        let diagnostics = new_shared_git_monitor_diagnostics();
        let (tx, mut rx) = mpsc::channel(4);
        let mut state = HashMap::new();
        let key = discovery_state_key(&repo);
        let mut probe_at = Instant::now();
        let mut last_probe_at = probe_at;

        for _ in 0..INVALID_FAILURE_THRESHOLD {
            last_probe_at = probe_at;
            poll_git_at(&config, &tx, &mut state, &diagnostics, probe_at)
                .await
                .unwrap();
            probe_at = state[&key]
                .failure
                .as_ref()
                .expect("failure state")
                .next_retry_at;
        }

        let quarantined = &state[&key];
        assert_eq!(quarantined.origin, Some(GitMonitorOrigin::Declarative));
        assert_eq!(quarantined.lifecycle, GitMonitorLifecycle::Quarantined);
        assert_eq!(quarantined.failure.as_ref().unwrap().attempts, 3);
        assert_eq!(
            probe_at.duration_since(last_probe_at),
            QUARANTINE_PROBE_INTERVAL
        );
        assert_eq!(
            snapshot_git_monitor_diagnostics(&diagnostics).quarantined,
            1
        );
        assert!(rx.try_recv().is_err());

        let before_sparse_probe = probe_at - Duration::from_secs(1);
        poll_git_at(&config, &tx, &mut state, &diagnostics, before_sparse_probe)
            .await
            .unwrap();
        assert_eq!(state[&key].failure.as_ref().unwrap().attempts, 3);

        init_repo(&missing).await;
        poll_git_at(&config, &tx, &mut state, &diagnostics, before_sparse_probe)
            .await
            .unwrap();
        assert_eq!(state[&key].lifecycle, GitMonitorLifecycle::Active);
        assert!(state[&key].failure.is_none());
        assert_eq!(snapshot_git_monitor_diagnostics(&diagnostics).active, 1);
    }

    #[tokio::test]
    async fn broken_gitfile_declarative_monitor_quarantines() {
        let sandbox = TempDir::new().unwrap();
        let root = sandbox.path().join("broken-worktree");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), "gitdir: /definitely/missing/gitdir\n").unwrap();
        let repo = GitRepoMonitor {
            path: path_str(&root).to_string(),
            ..GitRepoMonitor::default()
        };
        let config = config_with_repo(repo.clone());
        let diagnostics = new_shared_git_monitor_diagnostics();
        let (tx, _rx) = mpsc::channel(4);
        let mut state = HashMap::new();
        let key = discovery_state_key(&repo);
        let mut probe_at = Instant::now();

        for _ in 0..INVALID_FAILURE_THRESHOLD {
            poll_git_at(&config, &tx, &mut state, &diagnostics, probe_at)
                .await
                .unwrap();
            probe_at = state[&key].failure.as_ref().unwrap().next_retry_at;
        }

        assert_eq!(state[&key].origin, Some(GitMonitorOrigin::Declarative));
        assert_eq!(state[&key].lifecycle, GitMonitorLifecycle::Quarantined);
        assert_eq!(
            snapshot_git_monitor_diagnostics(&diagnostics).quarantined,
            1
        );
    }

    #[tokio::test]
    async fn missing_dynamic_worktree_retires_without_deleting_declarative_root() {
        let sandbox = TempDir::new().unwrap();
        let root = sandbox.path().join("repo");
        let worktree = sandbox.path().join("retired-worktree");
        init_repo(&root).await;
        git(
            &root,
            &["worktree", "add", "-b", "feat/retired", path_str(&worktree)],
        )
        .await;
        let repo = GitRepoMonitor {
            path: path_str(&root).to_string(),
            ..GitRepoMonitor::default()
        };
        let config = config_with_repo(repo.clone());
        let diagnostics = new_shared_git_monitor_diagnostics();
        let (tx, _rx) = mpsc::channel(4);
        let mut state = HashMap::new();
        poll_git(&config, &tx, &mut state, &diagnostics)
            .await
            .unwrap();

        std::fs::remove_dir_all(&worktree).unwrap();
        let dynamic_key = monitored_state_key(&repo, path_str(&worktree));
        let root_key = discovery_state_key(&repo);
        let mut probe_at = Instant::now();
        for _ in 0..INVALID_FAILURE_THRESHOLD {
            poll_git_at(&config, &tx, &mut state, &diagnostics, probe_at)
                .await
                .unwrap();
            probe_at = state[&dynamic_key]
                .failure
                .as_ref()
                .expect("dynamic failure state")
                .next_retry_at;
        }

        assert_eq!(state[&dynamic_key].origin, Some(GitMonitorOrigin::Dynamic));
        assert_eq!(state[&dynamic_key].lifecycle, GitMonitorLifecycle::Retired);
        assert_eq!(state[&dynamic_key].failure.as_ref().unwrap().attempts, 3);
        assert_eq!(state[&root_key].lifecycle, GitMonitorLifecycle::Active);
        let counts = snapshot_git_monitor_diagnostics(&diagnostics);
        assert_eq!(counts.active, 1);
        assert_eq!(counts.retired, 1);

        poll_git_at(&config, &tx, &mut state, &diagnostics, probe_at)
            .await
            .unwrap();
        assert_eq!(state[&dynamic_key].failure.as_ref().unwrap().attempts, 3);
    }

    #[tokio::test]
    async fn discovered_root_is_declarative_and_non_root_worktree_is_dynamic() {
        let sandbox = TempDir::new().unwrap();
        let root = sandbox.path().join("repo");
        let worktree = sandbox.path().join("dynamic-worktree");
        init_repo(&root).await;
        git(
            &root,
            &["worktree", "add", "-b", "feat/dynamic", path_str(&worktree)],
        )
        .await;
        let repo = GitRepoMonitor {
            path: path_str(&root).to_string(),
            ..GitRepoMonitor::default()
        };

        let monitored = discover_monitored_git_paths(&repo).await.unwrap();
        assert_eq!(monitored.len(), 2);
        assert_eq!(
            monitored
                .iter()
                .find(|entry| entry.worktree_path == path_str(&root))
                .unwrap()
                .origin,
            GitMonitorOrigin::Declarative
        );
        assert_eq!(
            monitored
                .iter()
                .find(|entry| entry.worktree_path == path_str(&worktree))
                .unwrap()
                .origin,
            GitMonitorOrigin::Dynamic
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_configured_root_preserves_commit_baseline() {
        use std::os::unix::fs::symlink;

        let sandbox = TempDir::new().unwrap();
        let root = sandbox.path().join("repo");
        let configured = sandbox.path().join("repo-link");
        init_repo(&root).await;
        symlink(&root, &configured).unwrap();
        let repo = GitRepoMonitor {
            path: path_str(&configured).to_string(),
            ..GitRepoMonitor::default()
        };
        let config = config_with_repo(repo.clone());
        let diagnostics = new_shared_git_monitor_diagnostics();
        let (tx, mut rx) = mpsc::channel(4);
        let mut state = HashMap::new();

        poll_git(&config, &tx, &mut state, &diagnostics)
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());

        std::fs::write(root.join("identity.txt"), "stable\n").unwrap();
        git(&root, &["add", "identity.txt"]).await;
        git(&root, &["commit", "-m", "identity-stable commit"]).await;
        poll_git(&config, &tx, &mut state, &diagnostics)
            .await
            .unwrap();

        let event = rx
            .try_recv()
            .expect("commit baseline preserved through symlink");
        assert_eq!(event.kind, "git.commit");
        assert_eq!(event.payload["summary"], "identity-stable commit");
        assert_eq!(
            state[&discovery_state_key(&repo)].origin,
            Some(GitMonitorOrigin::Declarative)
        );
        assert_eq!(snapshot_git_monitor_diagnostics(&diagnostics).active, 1);
    }

    #[test]
    fn recreation_identity_fence_discards_stale_terminal_failure() {
        let sandbox = TempDir::new().unwrap();
        let path = sandbox.path().join("recreated-worktree");
        let expected_identity = git_path_identity(path_str(&path));
        let mut state = GitMonitorState::default();
        reconcile_monitor_registration(
            &mut state,
            GitMonitorOrigin::Dynamic,
            expected_identity.clone(),
            path_str(&path),
            "snapshot",
        );

        std::fs::create_dir_all(&path).unwrap();
        record_monitor_failure(
            &mut state,
            MonitorFailureInput {
                origin: GitMonitorOrigin::Dynamic,
                expected_identity: &expected_identity,
                path: path_str(&path),
                operation: GitMonitorOperation::Snapshot,
                message: "fatal: cannot change to missing path: No such file or directory".into(),
                now: Instant::now(),
                poll_interval: Duration::from_secs(1),
            },
        );

        assert_eq!(state.lifecycle, GitMonitorLifecycle::Active);
        assert!(state.failure.is_none());
        assert_eq!(state.identity, Some(git_path_identity(path_str(&path))));
    }

    #[test]
    fn failure_attempts_and_suppression_windows_are_bounded() {
        let sandbox = TempDir::new().unwrap();
        let path = sandbox.path().join("missing");
        let identity = git_path_identity(path_str(&path));
        let mut state = GitMonitorState::default();
        reconcile_monitor_registration(
            &mut state,
            GitMonitorOrigin::Declarative,
            identity.clone(),
            path_str(&path),
            "snapshot",
        );
        let now = Instant::now();

        for offset in 0..(MAX_FAILURE_ATTEMPTS + 10) {
            record_monitor_failure(
                &mut state,
                MonitorFailureInput {
                    origin: GitMonitorOrigin::Declarative,
                    expected_identity: &identity,
                    path: path_str(&path),
                    operation: GitMonitorOperation::Snapshot,
                    message: "unexpected monitor failure".into(),
                    now: now + Duration::from_secs(u64::from(offset)),
                    poll_interval: Duration::from_secs(1),
                },
            );
        }
        assert_eq!(
            state.failure.as_ref().unwrap().attempts,
            MAX_FAILURE_ATTEMPTS
        );

        for _ in 0..(SUPPRESSED_SUMMARY_INTERVAL * 3 + 1) {
            assert!(should_skip_failed_monitor(&mut state, path_str(&path), now));
        }
        assert!(state.failure.as_ref().unwrap().suppressed_polls < SUPPRESSED_SUMMARY_INTERVAL);
    }

    #[test]
    fn equivalent_invalid_errors_share_the_consecutive_failure_threshold() {
        let sandbox = TempDir::new().unwrap();
        let path = sandbox.path().join("missing");
        let identity = git_path_identity(path_str(&path));
        let mut state = GitMonitorState::default();
        reconcile_monitor_registration(
            &mut state,
            GitMonitorOrigin::Declarative,
            identity.clone(),
            path_str(&path),
            "worktree discovery",
        );
        let now = Instant::now();
        let messages = [
            "fatal: cannot change to '/first': No such file or directory",
            "fatal: cannot change to '/second': No such file or directory",
            "fatal: repository path not found",
        ];

        for (offset, message) in messages.into_iter().enumerate() {
            record_monitor_failure(
                &mut state,
                MonitorFailureInput {
                    origin: GitMonitorOrigin::Declarative,
                    expected_identity: &identity,
                    path: path_str(&path),
                    operation: GitMonitorOperation::Discovery,
                    message: message.into(),
                    now: now + Duration::from_secs(offset as u64),
                    poll_interval: Duration::from_secs(1),
                },
            );
        }

        assert_eq!(state.lifecycle, GitMonitorLifecycle::Quarantined);
        assert_eq!(state.failure.as_ref().unwrap().attempts, 3);
    }

    #[test]
    fn successful_discovery_does_not_clear_a_snapshot_failure() {
        let sandbox = TempDir::new().unwrap();
        let path = sandbox.path().join("repo");
        std::fs::create_dir_all(&path).unwrap();
        let identity = git_path_identity(path_str(&path));
        let mut state = GitMonitorState::default();
        reconcile_monitor_registration(
            &mut state,
            GitMonitorOrigin::Declarative,
            identity.clone(),
            path_str(&path),
            "snapshot",
        );
        record_monitor_failure(
            &mut state,
            MonitorFailureInput {
                origin: GitMonitorOrigin::Declarative,
                expected_identity: &identity,
                path: path_str(&path),
                operation: GitMonitorOperation::Snapshot,
                message: "fatal: not a git repository".into(),
                now: Instant::now(),
                poll_interval: Duration::from_secs(1),
            },
        );

        clear_monitor_failure(&mut state, path_str(&path), GitMonitorOperation::Discovery);

        assert_eq!(state.lifecycle, GitMonitorLifecycle::Degraded);
        assert_eq!(
            state.failure.as_ref().unwrap().operation,
            GitMonitorOperation::Snapshot
        );
    }

    fn config_with_repo(repo: GitRepoMonitor) -> AppConfig {
        AppConfig {
            monitors: crate::config::MonitorConfig {
                poll_interval_secs: 1,
                git: crate::config::GitMonitorConfig { repos: vec![repo] },
                ..crate::config::MonitorConfig::default()
            },
            ..AppConfig::default()
        }
    }

    async fn init_repo(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        git(root, &["init"]).await;
        git(root, &["config", "user.name", "Test User"]).await;
        git(root, &["config", "user.email", "test@example.com"]).await;
        std::fs::write(root.join("README.md"), "seed\n").unwrap();
        git(root, &["add", "README.md"]).await;
        git(root, &["commit", "-m", "initial commit"]).await;
    }

    async fn git(root: &Path, args: &[&str]) {
        let mut command_args = vec!["-C", path_str(root)];
        command_args.extend_from_slice(args);
        run_command(&git_bin(), &command_args).await.unwrap();
    }

    fn path_str(path: &Path) -> &str {
        path.to_str().unwrap()
    }
}
