use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::sleep;

use crate::Result;
use crate::config::{AppConfig, GitRepoMonitor, TmuxSessionMonitor};
use crate::discord::DiscordClient;
use crate::events::{IncomingEvent, MessageFormat};
use crate::router::Router;

pub type SharedTmuxRegistry = Arc<RwLock<HashMap<String, RegisteredTmuxSession>>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredTmuxSession {
    pub session: String,
    pub channel: Option<String>,
    pub mention: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    pub stale_minutes: u64,
    pub format: Option<MessageFormat>,
    #[serde(default)]
    pub active_wrapper_monitor: bool,
}

impl From<&TmuxSessionMonitor> for RegisteredTmuxSession {
    fn from(value: &TmuxSessionMonitor) -> Self {
        Self {
            session: value.session.clone(),
            channel: value.channel.clone(),
            mention: value.mention.clone(),
            keywords: value.keywords.clone(),
            stale_minutes: value.stale_minutes,
            format: value.format.clone(),
            active_wrapper_monitor: false,
        }
    }
}

pub async fn run(
    config: Arc<AppConfig>,
    router: Arc<Router>,
    discord: Arc<DiscordClient>,
    tmux_registry: SharedTmuxRegistry,
) {
    let github_client = match build_github_client(config.monitor_github_token()) {
        Ok(client) => Some(client),
        Err(error) => {
            eprintln!("clawhip monitor: failed to build GitHub client: {error}");
            None
        }
    };
    let mut git_state: HashMap<String, GitRepoState> = HashMap::new();
    let mut tmux_state: HashMap<String, TmuxPaneState> = HashMap::new();

    loop {
        poll_git(
            config.as_ref(),
            github_client.as_ref(),
            router.as_ref(),
            discord.as_ref(),
            &mut git_state,
        )
        .await;
        poll_tmux(
            config.as_ref(),
            router.as_ref(),
            discord.as_ref(),
            &tmux_registry,
            &mut tmux_state,
        )
        .await;
        sleep(Duration::from_secs(
            config.monitors.poll_interval_secs.max(1),
        ))
        .await;
    }
}

struct GitRepoState {
    branch: String,
    head: String,
    issues: HashMap<u64, IssueSnapshot>,
    prs: HashMap<u64, PullRequestSnapshot>,
}

struct TmuxPaneState {
    session: String,
    pane_name: String,
    snapshot: String,
    content_hash: u64,
    last_change: Instant,
    last_stale_notification: Option<Instant>,
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
}

#[derive(Clone)]
struct CommitEntry {
    sha: String,
    summary: String,
}

struct GitSnapshot {
    repo_name: String,
    branch: String,
    head: String,
    commits: Vec<CommitEntry>,
    github_repo: Option<String>,
}

struct TmuxPaneSnapshot {
    pane_id: String,
    session: String,
    pane_name: String,
    content: String,
}

async fn poll_git(
    config: &AppConfig,
    github_client: Option<&reqwest::Client>,
    router: &Router,
    discord: &DiscordClient,
    state: &mut HashMap<String, GitRepoState>,
) {
    for repo in &config.monitors.git.repos {
        match snapshot_git_repo(repo).await {
            Ok(snapshot) => {
                let previous = state.get(&repo.path);
                if let Some(previous) = previous {
                    if repo.emit_branch_changes && previous.branch != snapshot.branch {
                        let event = IncomingEvent::git_branch_changed(
                            snapshot.repo_name.clone(),
                            previous.branch.clone(),
                            snapshot.branch.clone(),
                            repo.channel.clone(),
                        )
                        .with_format(repo.format.clone());
                        if let Err(error) =
                            dispatch_event(router, discord, &event, repo.mention.as_deref()).await
                        {
                            eprintln!("clawhip monitor git branch dispatch failed: {error}");
                        }
                    }
                    if repo.emit_commits && previous.head != snapshot.head {
                        let commits = list_new_commits(repo, &previous.head, &snapshot.head)
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
                            let event = event.with_format(repo.format.clone());
                            if let Err(error) =
                                dispatch_event(router, discord, &event, repo.mention.as_deref())
                                    .await
                            {
                                eprintln!("clawhip monitor git commit dispatch failed: {error}");
                            }
                        }
                    }
                }

                let issues = if repo.emit_issue_opened {
                    if let Some(client) = github_client {
                        match fetch_issues(
                            client,
                            &config.monitors.github_api_base,
                            repo,
                            &snapshot,
                        )
                        .await
                        {
                            Ok(issues) => {
                                if let Some(previous) = previous {
                                    for event in collect_issue_events(
                                        repo,
                                        &snapshot.repo_name,
                                        &previous.issues,
                                        &issues,
                                    ) {
                                        if let Err(error) = dispatch_event(
                                            router,
                                            discord,
                                            &event,
                                            repo.mention.as_deref(),
                                        )
                                        .await
                                        {
                                            eprintln!(
                                                "clawhip monitor issue dispatch failed: {error}"
                                            );
                                        }
                                    }
                                }
                                issues
                            }
                            Err(error) => {
                                eprintln!(
                                    "clawhip monitor GitHub issue polling failed for {}: {error}",
                                    repo.path
                                );
                                previous
                                    .map(|entry| entry.issues.clone())
                                    .unwrap_or_default()
                            }
                        }
                    } else {
                        previous
                            .map(|entry| entry.issues.clone())
                            .unwrap_or_default()
                    }
                } else {
                    previous
                        .map(|entry| entry.issues.clone())
                        .unwrap_or_default()
                };

                let prs = if repo.emit_pr_status {
                    if let Some(client) = github_client {
                        match fetch_pull_requests(
                            client,
                            &config.monitors.github_api_base,
                            repo,
                            &snapshot,
                        )
                        .await
                        {
                            Ok(prs) => {
                                if let Some(previous) = previous {
                                    for (number, pr) in &prs {
                                        match previous.prs.get(number) {
                                            Some(old) if old.status == pr.status => {}
                                            old => {
                                                let event =
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
                                                    .with_format(repo.format.clone());
                                                if let Err(error) = dispatch_event(
                                                    router,
                                                    discord,
                                                    &event,
                                                    repo.mention.as_deref(),
                                                )
                                                .await
                                                {
                                                    eprintln!(
                                                        "clawhip monitor PR dispatch failed: {error}"
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                prs
                            }
                            Err(error) => {
                                eprintln!(
                                    "clawhip monitor GitHub polling failed for {}: {error}",
                                    repo.path
                                );
                                previous.map(|entry| entry.prs.clone()).unwrap_or_default()
                            }
                        }
                    } else {
                        previous.map(|entry| entry.prs.clone()).unwrap_or_default()
                    }
                } else {
                    previous.map(|entry| entry.prs.clone()).unwrap_or_default()
                };

                state.insert(
                    repo.path.clone(),
                    GitRepoState {
                        branch: snapshot.branch,
                        head: snapshot.head,
                        issues,
                        prs,
                    },
                );
            }
            Err(error) => eprintln!(
                "clawhip monitor git snapshot failed for {}: {error}",
                repo.path
            ),
        }
    }
}

async fn poll_tmux(
    config: &AppConfig,
    router: &Router,
    discord: &DiscordClient,
    registry: &SharedTmuxRegistry,
    state: &mut HashMap<String, TmuxPaneState>,
) {
    let mut sessions: BTreeMap<String, RegisteredTmuxSession> = config
        .monitors
        .tmux
        .sessions
        .iter()
        .map(|session| {
            (
                session.session.clone(),
                RegisteredTmuxSession::from(session),
            )
        })
        .collect();
    for (session, registration) in registry.read().await.iter() {
        sessions.insert(session.clone(), registration.clone());
    }

    let mut active_panes = HashSet::new();
    let mut sessions_to_unregister = Vec::new();

    for (session_name, registration) in sessions {
        if registration.active_wrapper_monitor {
            continue;
        }
        match session_exists(&session_name).await {
            Ok(false) => {
                sessions_to_unregister.push(session_name.clone());
                state.retain(|key, pane| !key.starts_with(&format!("{}::", pane.session)));
                continue;
            }
            Err(error) => {
                eprintln!(
                    "clawhip monitor tmux has-session failed for {}: {error}",
                    session_name
                );
                continue;
            }
            Ok(true) => {}
        }

        match snapshot_tmux_session(&session_name).await {
            Ok(panes) => {
                for pane in panes {
                    let pane_key = format!("{}::{}", pane.session, pane.pane_id);
                    active_panes.insert(pane_key.clone());
                    let now = Instant::now();
                    let hash = content_hash(&pane.content);
                    let latest_line = last_nonempty_line(&pane.content);
                    match state.get_mut(&pane_key) {
                        None => {
                            state.insert(
                                pane_key,
                                TmuxPaneState {
                                    session: pane.session,
                                    pane_name: pane.pane_name,
                                    snapshot: pane.content,
                                    content_hash: hash,
                                    last_change: now,
                                    last_stale_notification: None,
                                },
                            );
                        }
                        Some(existing) => {
                            if existing.content_hash != hash {
                                let hits = collect_keyword_hits(
                                    &existing.snapshot,
                                    &pane.content,
                                    &registration.keywords,
                                );
                                for hit in hits {
                                    let event = IncomingEvent::tmux_keyword(
                                        pane.session.clone(),
                                        hit.keyword,
                                        hit.line,
                                        registration.channel.clone(),
                                    )
                                    .with_mention(registration.mention.clone())
                                    .with_format(registration.format.clone());
                                    if let Err(error) = dispatch_event(
                                        router,
                                        discord,
                                        &event,
                                        registration.mention.as_deref(),
                                    )
                                    .await
                                    {
                                        eprintln!(
                                            "clawhip monitor tmux keyword dispatch failed: {error}"
                                        );
                                    }
                                }
                                existing.pane_name = pane.pane_name;
                                existing.snapshot = pane.content;
                                existing.content_hash = hash;
                                existing.last_change = now;
                                existing.last_stale_notification = None;
                            } else {
                                let stale_after =
                                    Duration::from_secs(registration.stale_minutes.max(1) * 60);
                                let should_notify = now.duration_since(existing.last_change)
                                    >= stale_after
                                    && existing
                                        .last_stale_notification
                                        .map(|previous| now.duration_since(previous) >= stale_after)
                                        .unwrap_or(true);
                                if should_notify {
                                    let event = IncomingEvent::tmux_stale(
                                        existing.session.clone(),
                                        existing.pane_name.clone(),
                                        registration.stale_minutes,
                                        latest_line,
                                        registration.channel.clone(),
                                    )
                                    .with_mention(registration.mention.clone())
                                    .with_format(registration.format.clone());
                                    if let Err(error) = dispatch_event(
                                        router,
                                        discord,
                                        &event,
                                        registration.mention.as_deref(),
                                    )
                                    .await
                                    {
                                        eprintln!(
                                            "clawhip monitor tmux stale dispatch failed: {error}"
                                        );
                                    }
                                    existing.last_stale_notification = Some(now);
                                }
                            }
                        }
                    }
                }
            }
            Err(error) => eprintln!(
                "clawhip monitor tmux snapshot failed for {}: {error}",
                session_name
            ),
        }
    }

    state.retain(|key, _| active_panes.contains(key));
    if !sessions_to_unregister.is_empty() {
        let mut write = registry.write().await;
        for session in sessions_to_unregister {
            write.remove(&session);
        }
    }
}

async fn dispatch_event(
    router: &Router,
    discord: &DiscordClient,
    event: &IncomingEvent,
    mention: Option<&str>,
) -> Result<()> {
    let event = match (event.mention.as_ref(), mention.map(str::trim)) {
        (None, Some(mention)) if !mention.is_empty() => {
            event.clone().with_mention(Some(mention.to_string()))
        }
        _ => event.clone(),
    };
    let delivery = router.preview_delivery(&event).await?;
    discord.send(&delivery.target, &delivery.content).await
}

async fn snapshot_git_repo(repo: &GitRepoMonitor) -> Result<GitSnapshot> {
    let head = run_command(&git_bin(), &["-C", &repo.path, "rev-parse", "HEAD"]).await?;
    let branch = run_command(
        &git_bin(),
        &["-C", &repo.path, "rev-parse", "--abbrev-ref", "HEAD"],
    )
    .await?;
    let summary = run_command(&git_bin(), &["-C", &repo.path, "log", "-1", "--pretty=%s"]).await?;
    let remote_url = run_command(
        &git_bin(),
        &[
            "-C",
            &repo.path,
            "config",
            "--get",
            &format!("remote.{}.url", repo.remote),
        ],
    )
    .await
    .unwrap_or_default();
    Ok(GitSnapshot {
        repo_name: repo.name.clone().unwrap_or_else(|| {
            Path::new(&repo.path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&repo.path)
                .to_string()
        }),
        branch,
        head: head.clone(),
        commits: vec![CommitEntry { sha: head, summary }],
        github_repo: repo
            .github_repo
            .clone()
            .or_else(|| parse_github_repo(&remote_url)),
    })
}

async fn list_new_commits(repo: &GitRepoMonitor, old: &str, new: &str) -> Result<Vec<CommitEntry>> {
    let output = run_command(
        &git_bin(),
        &[
            "-C",
            &repo.path,
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
                        .with_format(repo.format.clone()),
                    );
                }
            }
        }
    }
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
    let response = client
        .get(format!(
            "{}/repos/{}/issues",
            api_base.trim_end_matches('/'),
            github_repo
        ))
        .query(&[("state", "all"), ("per_page", "100")])
        .send()
        .await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("GitHub API request failed with {status}: {body}").into());
    }
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
    let response = client
        .get(format!(
            "{}/repos/{}/pulls",
            api_base.trim_end_matches('/'),
            github_repo
        ))
        .query(&[("state", "all"), ("per_page", "100")])
        .send()
        .await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("GitHub API request failed with {status}: {body}").into());
    }
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
                },
            )
        })
        .collect())
}

async fn session_exists(session: &str) -> Result<bool> {
    let output = Command::new(tmux_bin())
        .arg("has-session")
        .arg("-t")
        .arg(session)
        .output()
        .await?;
    Ok(output.status.success())
}

async fn snapshot_tmux_session(session: &str) -> Result<Vec<TmuxPaneSnapshot>> {
    let output = Command::new(tmux_bin())
        .arg("list-panes")
        .arg("-t")
        .arg(session)
        .arg("-F")
        .arg("#{pane_id}|#{session_name}|#{window_index}.#{pane_index}|#{pane_title}")
        .output()
        .await?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .to_string()
            .into());
    }
    let mut panes = Vec::new();
    for line in String::from_utf8(output.stdout)?.lines() {
        let mut parts = line.splitn(4, '|');
        let pane_id = parts.next().unwrap_or_default().to_string();
        if pane_id.is_empty() {
            continue;
        }
        let session_name = parts.next().unwrap_or_default().to_string();
        let pane_name = parts.next().unwrap_or_default().to_string();
        let capture = Command::new(tmux_bin())
            .arg("capture-pane")
            .arg("-p")
            .arg("-t")
            .arg(&pane_id)
            .arg("-S")
            .arg("-200")
            .output()
            .await?;
        if !capture.status.success() {
            return Err(String::from_utf8_lossy(&capture.stderr)
                .trim()
                .to_string()
                .into());
        }
        panes.push(TmuxPaneSnapshot {
            pane_id,
            session: session_name,
            pane_name,
            content: String::from_utf8(capture.stdout)?,
        });
    }
    Ok(panes)
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

async fn run_command(binary: &str, args: &[&str]) -> Result<String> {
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

fn collect_keyword_hits(previous: &str, current: &str, keywords: &[String]) -> Vec<KeywordHit> {
    if keywords.is_empty() {
        return Vec::new();
    }
    let previous_lines: HashSet<&str> = previous.lines().collect();
    current
        .lines()
        .filter(|line| !previous_lines.contains(*line))
        .flat_map(|line| {
            keywords.iter().filter_map(move |keyword| {
                if line
                    .to_ascii_lowercase()
                    .contains(&keyword.to_ascii_lowercase())
                {
                    Some(KeywordHit {
                        keyword: keyword.clone(),
                        line: line.to_string(),
                    })
                } else {
                    None
                }
            })
        })
        .collect()
}

fn content_hash(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

fn last_nonempty_line(content: &str) -> String {
    content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("<no output>")
        .trim()
        .to_string()
}

fn git_bin() -> String {
    std::env::var("CLAWHIP_GIT_BIN").unwrap_or_else(|_| "git".to_string())
}

fn tmux_bin() -> String {
    std::env::var("CLAWHIP_TMUX_BIN").unwrap_or_else(|_| "tmux".to_string())
}

fn parse_github_repo(remote: &str) -> Option<String> {
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

struct KeywordHit {
    keyword: String,
    line: String,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, DefaultsConfig, RouteRule};
    use crate::router::Router;

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

    #[tokio::test]
    async fn new_issue_events_match_repo_filter_and_route_mention() {
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
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "github.*".into(),
                filter: [("repo".to_string(), "clawhip".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("route-channel".into()),
                webhook: None,
                mention: Some("<@1465264645320474637>".into()),
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Alert),
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let (channel, _, content) = router.preview(&events[0]).await.unwrap();
        assert_eq!(channel, "dev-channel");
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
                .any(|e| e.canonical_kind() == "github.issue-commented")
        );
        assert!(
            events
                .iter()
                .any(|e| e.canonical_kind() == "github.issue-closed")
        );
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

    #[test]
    fn keyword_hits_only_emit_for_new_lines() {
        let hits = collect_keyword_hits(
            "done\nall good",
            "done\nall good\nerror: failed\nPR created #7",
            &["error".into(), "PR created".into()],
        );
        assert_eq!(hits.len(), 2);
    }
}
