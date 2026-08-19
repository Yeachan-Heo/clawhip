use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
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

/// Persisted terminal-run identity per monitored repo. Survives restart and
/// re-enrollment so historical workflow runs cannot replay as fresh events
/// (#317). Bounded: at most [`MAX_BASELINE_RUNS_PER_REPO`] identities per repo.
#[derive(Default)]
struct CIBaseline {
    repos: HashMap<String, RepoCIBaseline>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct RepoCIBaseline {
    /// Terminal run identities observed for this repo, oldest evicted first.
    #[serde(default)]
    terminal_runs: VecDeque<String>,
}

const MAX_BASELINE_RUNS_PER_REPO: usize = 256;

/// Beside the cron state file, matching `tmux-watch-registry.json` and
/// `discord-watch-state.json`.
pub fn default_github_ci_baseline_path(cron_state_path: &Path) -> PathBuf {
    cron_state_path.with_file_name("github-ci-baseline.json")
}

fn baseline_identity(ci: &GitHubCISnapshot) -> String {
    // Prefer the immutable GitHub run id; fall back to the dedupe key so
    // check-run snapshots without a run id still dedupe by sha+workflow.
    ci.run_id
        .as_ref()
        .map(|run_id| format!("run:{run_id}"))
        .unwrap_or_else(|| ci.dedupe_key())
}

fn repo_ci_baseline_is_suppressed(
    repo_ci_baseline: Option<&RepoCIBaseline>,
    dedupe_key: &str,
) -> bool {
    let Some(baseline) = repo_ci_baseline else {
        return false;
    };
    baseline.terminal_runs.iter().any(|identity| {
        identity == dedupe_key || {
            // Persisted identities are run-id based; a terminal snapshot keyed
            // by `run:<id>:<workflow>` must still match its persisted
            // `run:<id>` identity.
            identity
                .strip_prefix("run:")
                .is_some_and(|id| dedupe_key.starts_with(&format!("run:{id}:")))
        }
    })
}

impl CIBaseline {
    /// Records terminal-run identities for one repo. Returns whether the
    /// persisted state changed (bounded, oldest entries evicted first).
    fn record_terminal_runs(
        &mut self,
        repo_path: &str,
        current: &HashMap<String, GitHubCISnapshot>,
    ) -> bool {
        let repo_baseline = self.repos.entry(repo_path.to_string()).or_default();
        let mut dirty = repo_baseline.terminal_runs.is_empty();
        for ci in current.values() {
            if !ci.is_terminal() {
                continue;
            }
            let identity = baseline_identity(ci);
            if !repo_baseline.terminal_runs.contains(&identity) {
                repo_baseline.terminal_runs.push_back(identity);
                dirty = true;
            }
        }
        while repo_baseline.terminal_runs.len() > MAX_BASELINE_RUNS_PER_REPO {
            repo_baseline.terminal_runs.pop_front();
            dirty = true;
        }
        dirty
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
        Ok(repos) => CIBaseline { repos },
        Err(error) => {
            eprintln!(
                "clawhip source github CI baseline '{}' invalid; starting a fresh baseline: {error}",
                path.display()
            );
            CIBaseline::default()
        }
    }
}

fn save_ci_baseline(baseline: &CIBaseline, path: Option<&Path>) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_string(&baseline.repos)?;
    let temp_path = path.with_extension("json.tmp");
    fs::write(&temp_path, payload.as_bytes())?;
    fs::rename(&temp_path, path)?;
    Ok(())
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
    run_job_count: usize,
    run_all_terminal: bool,
}

impl GitHubCISnapshot {
    fn dedupe_key(&self) -> String {
        if let Some(run_id) = &self.run_id {
            return format!("run:{run_id}:{}", self.workflow);
        }
        format!(
            "{}:{}:{}",
            self.pr_number
                .map(|number| number.to_string())
                .unwrap_or_else(|| "none".to_string()),
            self.sha,
            self.workflow
        )
    }

    fn event_kind(&self) -> &'static str {
        classify_ci_event_kind(&self.status, self.conclusion.as_deref())
    }

    /// A terminal run will never change state again, so its identity can be
    /// persisted and suppressed on later re-observation (#317).
    fn is_terminal(&self) -> bool {
        self.status == "completed"
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
        let (ci, ci_baseline_established) = match poll_ci_statuses(
            config,
            github_client,
            repo,
            &snapshot,
            previous,
            &prs,
            tx,
            ci_baseline.repos.get(&repo.path),
        )
        .await
        {
            Ok(ci) => ci,
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
                )
            }
        };

        let repo_baseline_dirty = ci_baseline.record_terminal_runs(&repo.path, &ci);
        state.insert(
            repo.path.clone(),
            GitHubRepoState {
                issues,
                prs,
                ci,
                ci_baseline_established,
            },
        );
        if repo_baseline_dirty && let Err(error) = save_ci_baseline(ci_baseline, ci_baseline_path) {
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
        }
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

#[allow(clippy::too_many_arguments)]
async fn poll_ci_statuses(
    config: &AppConfig,
    github_client: Option<&reqwest::Client>,
    repo: &GitRepoMonitor,
    snapshot: &GitSnapshot,
    previous: Option<&GitHubRepoState>,
    prs: &HashMap<u64, PullRequestSnapshot>,
    tx: &mpsc::Sender<IncomingEvent>,
    repo_ci_baseline: Option<&RepoCIBaseline>,
) -> Result<(HashMap<String, GitHubCISnapshot>, bool)> {
    if !repo.emit_pr_status {
        return Ok((
            previous.map(|entry| entry.ci.clone()).unwrap_or_default(),
            previous
                .map(|entry| entry.ci_baseline_established)
                .unwrap_or(false),
        ));
    }

    let Some(client) = github_client else {
        return Ok((
            previous.map(|entry| entry.ci.clone()).unwrap_or_default(),
            previous
                .map(|entry| entry.ci_baseline_established)
                .unwrap_or(false),
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
        Ok(ci) => {
            if let Some(previous) = previous {
                for event in collect_ci_events(
                    repo,
                    &snapshot.repo_name,
                    previous.ci_baseline_established,
                    &previous.ci,
                    &ci,
                    repo_ci_baseline,
                ) {
                    send_event(tx, event).await?;
                }
            }
            Ok((ci, true))
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
            ))
        }
    }
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

fn collect_ci_events(
    repo: &GitRepoMonitor,
    repo_name: &str,
    previous_ci_baseline_established: bool,
    previous: &HashMap<String, GitHubCISnapshot>,
    current: &HashMap<String, GitHubCISnapshot>,
    repo_ci_baseline: Option<&RepoCIBaseline>,
) -> Vec<IncomingEvent> {
    let mut events = Vec::new();
    for (key, ci) in current {
        let changed = match previous.get(key) {
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
                    !repo_ci_baseline_is_suppressed(repo_ci_baseline, key)
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
) -> Result<HashMap<String, GitHubCISnapshot>> {
    let github_repo = snapshot
        .github_repo
        .clone()
        .ok_or_else(|| format!("no GitHub repo configured or inferred for {}", repo.path))?;
    let mut check_runs = HashMap::new();
    let mut seen_run_ids = HashSet::new();

    for (number, pr) in open_prs {
        for check_run in fetch_check_runs(client, api_base, &github_repo, *number, pr).await? {
            if let Some(run_id) = &check_run.run_id {
                seen_run_ids.insert(run_id.clone());
            }
            check_runs.insert(check_run.dedupe_key(), check_run);
        }
    }

    for workflow_run in fetch_direct_workflow_runs(client, api_base, &github_repo, snapshot).await?
    {
        if workflow_run
            .run_id
            .as_ref()
            .is_some_and(|run_id| seen_run_ids.contains(run_id))
        {
            continue;
        }
        check_runs.insert(workflow_run.dedupe_key(), workflow_run);
    }

    Ok(check_runs)
}

async fn fetch_check_runs(
    client: &reqwest::Client,
    api_base: &str,
    github_repo: &str,
    pr_number: u64,
    pr: &PullRequestSnapshot,
) -> Result<Vec<GitHubCISnapshot>> {
    let response = github_get(
        client,
        api_base,
        &format!("repos/{github_repo}/commits/{}/check-runs", pr.head_sha),
        &[("per_page", "100")],
        &format!("check runs for {github_repo} PR #{pr_number}"),
    )
    .await?;

    let runs: GitHubCheckRunsResponse = response.json().await?;
    let run_summaries = summarize_workflow_runs(&runs.check_runs);
    Ok(runs
        .check_runs
        .into_iter()
        .map(|check_run| {
            let url = check_run.details_url.unwrap_or_else(|| pr.url.clone());
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
                run_job_count,
                run_all_terminal,
            }
        })
        .collect())
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
) -> Result<Vec<GitHubCISnapshot>> {
    let mut query = vec![("per_page", "100"), ("event", "push")];
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

    let runs: GitHubWorkflowRunsResponse = response.json().await?;
    Ok(runs
        .workflow_runs
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
                run_job_count: 1,
                run_all_terminal,
            }
        })
        .collect())
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
    name: String,
    status: String,
    conclusion: Option<String>,
    details_url: Option<String>,
    head_sha: String,
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
    pull_requests: Vec<serde_json::Value>,
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
        assert!(ci_baseline.record_terminal_runs("/repo", &run_map(&historical_runs)));
        save_ci_baseline(&ci_baseline, Some(&path)).unwrap();

        // --- Daemon restart: state HashMap is empty, baseline reloads from disk.
        let restarted = load_ci_baseline(Some(&path));
        let repo_baseline = restarted.repos.get("/repo").expect("persisted repo");

        // Bounded: only terminal identities, at most MAX_BASELINE_RUNS_PER_REPO.
        assert_eq!(repo_baseline.terminal_runs.len(), 3);
        for run in &historical_runs {
            assert!(
                repo_ci_baseline_is_suppressed(restarted.repos.get("/repo"), &run.dedupe_key()),
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
            ci_baseline.record_terminal_runs("/repo", &run_map(&runs));
        }
        let repo_baseline = ci_baseline.repos.get("/repo").unwrap();
        assert_eq!(
            repo_baseline.terminal_runs.len(),
            MAX_BASELINE_RUNS_PER_REPO
        );
        assert!(
            repo_baseline
                .terminal_runs
                .contains(&format!("run:{}", MAX_BASELINE_RUNS_PER_REPO + 63))
        );
        assert!(!repo_baseline.terminal_runs.contains(&"run:0".to_string()));

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
        mixed.record_terminal_runs("/other-repo", &run_map(&other));
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
        ci_baseline.record_terminal_runs("/repo", &run_map(std::slice::from_ref(&in_progress)));
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

        ci_baseline.record_terminal_runs("/repo", &run_map(std::slice::from_ref(&completed)));
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
        assert!(
            persisted["/tmp/clawhip-replay-repo"].terminal_runs.len() <= MAX_BASELINE_RUNS_PER_REPO
        );

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
        let (tx, mut rx) = mpsc::channel(4);
        let prs = HashMap::new();

        let (ci, ci_baseline_established) = poll_ci_statuses(
            &config,
            Some(&client),
            &repo,
            &snapshot,
            None,
            &prs,
            &tx,
            None,
        )
        .await
        .unwrap();

        assert_eq!(ci.len(), 1);
        assert!(ci_baseline_established);
        assert!(
            rx.try_recv().is_err(),
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

        let ci = fetch_ci_statuses(
            &client,
            &format!("http://{addr}"),
            &GitRepoMonitor::default(),
            &snapshot,
            &open_prs,
        )
        .await
        .unwrap();

        assert_eq!(ci.len(), 2);
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
