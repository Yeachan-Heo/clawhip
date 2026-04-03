use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::Result;
use crate::config::{AppConfig, GitRepoMonitor};
use crate::events::IncomingEvent;
use crate::source::Source;

pub struct GitSource {
    config: Arc<AppConfig>,
}

impl GitSource {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
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
            poll_git(self.config.as_ref(), &tx, &mut state).await?;
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GitMonitorFailureState {
    classification: GitMonitorFailureClass,
    message: String,
    attempts: u32,
    suppressed_polls: u32,
    next_retry_at: Instant,
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MonitoredGitPath {
    state_key: String,
    repo_path: String,
    worktree_path: String,
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
) -> Result<()> {
    poll_git_at(config, tx, state, Instant::now()).await
}

async fn poll_git_at(
    config: &AppConfig,
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut HashMap<String, GitMonitorState>,
    now: Instant,
) -> Result<()> {
    let mut active_keys = HashSet::new();
    let poll_interval = Duration::from_secs(config.monitors.poll_interval_secs.max(1));

    for repo in &config.monitors.git.repos {
        let discovery_key = discovery_state_key(repo);
        active_keys.insert(discovery_key.clone());

        if let Some(discovery_state) = state.get_mut(&discovery_key)
            && should_skip_failed_monitor(discovery_state, now)
        {
            preserve_repo_monitor_keys(state, &mut active_keys, repo);
            continue;
        }

        let monitored_paths = match discover_monitored_git_paths(repo).await {
            Ok(monitored_paths) => {
                if let Some(discovery_state) = state.get_mut(&discovery_key) {
                    clear_monitor_failure(discovery_state, &repo.path, "worktree discovery");
                }
                monitored_paths
            }
            Err(error) => {
                record_monitor_failure(
                    state.entry(discovery_key).or_default(),
                    &repo.path,
                    "worktree discovery",
                    error.to_string(),
                    now,
                    poll_interval,
                );
                preserve_repo_monitor_keys(state, &mut active_keys, repo);
                continue;
            }
        };

        for monitored in monitored_paths {
            active_keys.insert(monitored.state_key.clone());
            let monitor_state = state.entry(monitored.state_key.clone()).or_default();

            if should_skip_failed_monitor(monitor_state, now) {
                continue;
            }

            match snapshot_git_worktree(repo, &monitored).await {
                Ok(snapshot) => {
                    clear_monitor_failure(monitor_state, &monitored.worktree_path, "snapshot");
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
                    &monitored.worktree_path,
                    "snapshot",
                    error.to_string(),
                    now,
                    poll_interval,
                ),
            }
        }
    }

    state.retain(|key, _| active_keys.contains(key));

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
            monitored.push(MonitoredGitPath {
                state_key: monitored_state_key(repo, &worktree_path),
                repo_path: repo.path.clone(),
                worktree_path,
            });
        }
    }

    if monitored.is_empty() {
        monitored.push(MonitoredGitPath {
            state_key: monitored_state_key(repo, &repo.path),
            repo_path: repo.path.clone(),
            worktree_path: repo.path.clone(),
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
    format!("path::{}::{worktree_path}", repo.path)
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

fn should_skip_failed_monitor(state: &mut GitMonitorState, now: Instant) -> bool {
    let Some(failure) = state.failure.as_mut() else {
        return false;
    };
    if now < failure.next_retry_at {
        failure.suppressed_polls += 1;
        return true;
    }
    false
}

fn clear_monitor_failure(state: &mut GitMonitorState, path: &str, context: &str) {
    let Some(previous) = state.failure.take() else {
        return;
    };
    eprintln!(
        "clawhip source git {context} recovered for {path} after {} failure(s) and {} suppressed poll(s)",
        previous.attempts, previous.suppressed_polls
    );
}

fn record_monitor_failure(
    state: &mut GitMonitorState,
    path: &str,
    context: &str,
    message: String,
    now: Instant,
    poll_interval: Duration,
) {
    let classification = classify_git_monitor_failure(&message);
    let (attempts, suppressed_polls) = match state.failure.take() {
        Some(previous)
            if previous.classification == classification && previous.message == message =>
        {
            (
                previous.attempts.saturating_add(1),
                previous.suppressed_polls,
            )
        }
        _ => (1, 0),
    };
    let backoff = git_monitor_backoff(attempts, poll_interval);
    eprintln!(
        "clawhip source git {context} degraded for {path}: class={}, attempts={}, suppressed={}, next_retry_secs={}, error={message}",
        classification.as_str(),
        attempts,
        suppressed_polls,
        backoff.as_secs()
    );
    state.failure = Some(GitMonitorFailureState {
        classification,
        message,
        attempts,
        suppressed_polls: 0,
        next_retry_at: now + backoff,
    });
}

fn git_monitor_backoff(attempts: u32, poll_interval: Duration) -> Duration {
    let multiplier = 2u32.saturating_pow(attempts.min(6));
    let capped = poll_interval
        .as_secs()
        .saturating_mul(multiplier.into())
        .min(300);
    Duration::from_secs(capped.max(1))
}

fn classify_git_monitor_failure(message: &str) -> GitMonitorFailureClass {
    let lowered = message.to_ascii_lowercase();
    if lowered.contains("no such file or directory") || lowered.contains("cannot change to") {
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
        let config = AppConfig {
            monitors: crate::config::MonitorConfig {
                git: crate::config::GitMonitorConfig { repos: vec![repo] },
                ..crate::config::MonitorConfig::default()
            },
            ..AppConfig::default()
        };

        let (tx, mut rx) = mpsc::channel(16);
        let mut state = HashMap::new();

        poll_git(&config, &tx, &mut state).await.unwrap();
        assert!(rx.try_recv().is_err());

        git(&worktree, &["checkout", "-b", "feat/issue-115-v2"]).await;
        poll_git(&config, &tx, &mut state).await.unwrap();
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

        poll_git(&config, &tx, &mut state).await.unwrap();
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
    async fn poll_git_backs_off_invalid_repo_discovery_failures() {
        let sandbox = TempDir::new().unwrap();
        let missing = sandbox.path().join("missing-repo");
        let repo = GitRepoMonitor {
            path: path_str(&missing).to_string(),
            ..GitRepoMonitor::default()
        };
        let config = AppConfig {
            monitors: crate::config::MonitorConfig {
                poll_interval_secs: 1,
                git: crate::config::GitMonitorConfig {
                    repos: vec![repo.clone()],
                },
                ..crate::config::MonitorConfig::default()
            },
            ..AppConfig::default()
        };

        let (tx, mut rx) = mpsc::channel(4);
        let mut state = HashMap::new();
        let start = Instant::now();

        poll_git_at(&config, &tx, &mut state, start).await.unwrap();
        assert!(rx.try_recv().is_err());

        let key = discovery_state_key(&repo);
        let failure = state
            .get(&key)
            .and_then(|entry| entry.failure.as_ref())
            .expect("failure state recorded");
        assert_eq!(failure.classification, GitMonitorFailureClass::Missing);
        assert_eq!(failure.attempts, 1);
        let next_retry = failure.next_retry_at;

        poll_git_at(&config, &tx, &mut state, start + Duration::from_secs(1))
            .await
            .unwrap();
        let failure = state
            .get(&key)
            .and_then(|entry| entry.failure.as_ref())
            .expect("failure state retained during cooldown");
        assert_eq!(failure.attempts, 1);
        assert_eq!(failure.suppressed_polls, 1);

        poll_git_at(&config, &tx, &mut state, next_retry)
            .await
            .unwrap();
        let failure = state
            .get(&key)
            .and_then(|entry| entry.failure.as_ref())
            .expect("failure state updated after retry");
        assert_eq!(failure.attempts, 2);
        assert_eq!(failure.suppressed_polls, 0);
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
