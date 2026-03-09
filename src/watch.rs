use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::sleep;

use crate::Result;
use crate::config::{AppConfig, GitRepoWatch, TmuxSessionWatch, WatchConfig};
use crate::discord::DiscordClient;
use crate::events::IncomingEvent;
use crate::router::Router;

pub async fn run(
    config: std::sync::Arc<AppConfig>,
    router: std::sync::Arc<Router>,
    discord: std::sync::Arc<DiscordClient>,
    once: bool,
) -> Result<()> {
    if config.watch.git.repos.is_empty() && config.watch.tmux.sessions.is_empty() {
        return Err("watch mode requires at least one configured git repo or tmux session".into());
    }

    let github_client = build_github_client(&config.watch, config.effective_github_token())?;
    let mut state = WatchState::default();

    loop {
        poll_git_repos(
            config.as_ref(),
            &github_client,
            &router,
            discord.as_ref(),
            &mut state,
        )
        .await;
        poll_tmux_sessions(config.as_ref(), &router, discord.as_ref(), &mut state).await;

        if once {
            break;
        }

        sleep(Duration::from_secs(config.watch.poll_interval_secs.max(1))).await;
    }

    Ok(())
}

#[derive(Default)]
struct WatchState {
    git: HashMap<String, GitRepoState>,
    tmux: HashMap<String, TmuxPaneState>,
}

struct GitRepoState {
    branch: String,
    head: String,
    prs: HashMap<u64, PullRequestSnapshot>,
    last_polled: Instant,
}

struct TmuxPaneState {
    session: String,
    pane_name: String,
    last_hash: u64,
    snapshot: String,
    last_change: Instant,
    last_stale_notification: Option<Instant>,
    last_polled: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PullRequestSnapshot {
    number: u64,
    title: String,
    status: String,
    url: String,
}

#[derive(Debug)]
struct GitRepoSnapshot {
    name: String,
    branch: String,
    head: String,
    commits: Vec<CommitEntry>,
    github_repo: Option<String>,
}

#[derive(Clone, Debug)]
struct CommitEntry {
    sha: String,
    summary: String,
}

#[derive(Debug)]
struct TmuxPaneSnapshot {
    pane_id: String,
    session: String,
    pane_name: String,
    content: String,
}

async fn poll_git_repos(
    config: &AppConfig,
    github_client: &reqwest::Client,
    router: &Router,
    discord: &DiscordClient,
    state: &mut WatchState,
) {
    for repo in &config.watch.git.repos {
        if !should_poll_git(repo, &state.git, config.watch.poll_interval_secs) {
            continue;
        }

        match snapshot_git_repo(repo).await {
            Ok(snapshot) => {
                let key = repo.path.clone();
                let previous = state.git.get(&key);
                let now = Instant::now();

                if let Some(previous) = previous {
                    if repo.emit_branch_changes && previous.branch != snapshot.branch {
                        let event = IncomingEvent::git_branch_changed(
                            snapshot.name.clone(),
                            previous.branch.clone(),
                            snapshot.branch.clone(),
                            repo.channel.clone(),
                        )
                        .with_format(repo.format.clone())
                        .with_template(repo.template.clone());
                        if let Err(error) = router.dispatch(&event, discord).await {
                            eprintln!("clawhip watch git branch event failed: {error}");
                        }
                    }

                    if repo.emit_new_commits && previous.head != snapshot.head {
                        let commits = match list_new_commits(repo, &previous.head, &snapshot.head).await {
                            Ok(commits) if !commits.is_empty() => commits,
                            Ok(_) | Err(_) => snapshot.commits.clone(),
                        };

                        let events = IncomingEvent::git_commit_events(
                            snapshot.name.clone(),
                            snapshot.branch.clone(),
                            commits
                                .into_iter()
                                .map(|commit| (commit.sha, commit.summary))
                                .collect(),
                            repo.channel.clone(),
                        );

                        for event in events {
                            let event = event
                                .with_format(repo.format.clone())
                                .with_template(repo.template.clone());
                            if let Err(error) = router.dispatch(&event, discord).await {
                                eprintln!("clawhip watch git commit event failed: {error}");
                            }
                        }
                    }
                }

                let prs = if repo.emit_pr_status {
                    match fetch_pull_requests(
                        github_client,
                        &config.watch.github_api_base,
                        repo,
                        &snapshot,
                    )
                    .await
                    {
                        Ok(prs) => {
                            if let Some(previous) = previous {
                                emit_pr_changes(
                                    repo,
                                    router,
                                    discord,
                                    &snapshot.name,
                                    &previous.prs,
                                    &prs,
                                )
                                .await;
                            }
                            prs
                        }
                        Err(error) => {
                            eprintln!(
                                "clawhip watch GitHub PR polling failed for {}: {error}",
                                repo.path
                            );
                            previous.map(|entry| entry.prs.clone()).unwrap_or_default()
                        }
                    }
                } else {
                    previous.map(|entry| entry.prs.clone()).unwrap_or_default()
                };

                state.git.insert(
                    key,
                    GitRepoState {
                        branch: snapshot.branch,
                        head: snapshot.head,
                        prs,
                        last_polled: now,
                    },
                );
            }
            Err(error) => eprintln!("clawhip watch git poll failed for {}: {error}", repo.path),
        }
    }
}

async fn emit_pr_changes(
    repo: &GitRepoWatch,
    router: &Router,
    discord: &DiscordClient,
    repo_name: &str,
    previous: &HashMap<u64, PullRequestSnapshot>,
    current: &HashMap<u64, PullRequestSnapshot>,
) {
    for (number, pr) in current {
        match previous.get(number) {
            None => {
                let event = IncomingEvent::git_pr_status_changed(
                    repo_name.to_string(),
                    *number,
                    pr.title.clone(),
                    "<new>".to_string(),
                    pr.status.clone(),
                    pr.url.clone(),
                    repo.channel.clone(),
                )
                .with_format(repo.format.clone())
                .with_template(repo.template.clone());
                if let Err(error) = router.dispatch(&event, discord).await {
                    eprintln!("clawhip watch PR new event failed: {error}");
                }
            }
            Some(previous_pr) if previous_pr.status != pr.status => {
                let event = IncomingEvent::git_pr_status_changed(
                    repo_name.to_string(),
                    *number,
                    pr.title.clone(),
                    previous_pr.status.clone(),
                    pr.status.clone(),
                    pr.url.clone(),
                    repo.channel.clone(),
                )
                .with_format(repo.format.clone())
                .with_template(repo.template.clone());
                if let Err(error) = router.dispatch(&event, discord).await {
                    eprintln!("clawhip watch PR change event failed: {error}");
                }
            }
            _ => {}
        }
    }
}

async fn poll_tmux_sessions(
    config: &AppConfig,
    router: &Router,
    discord: &DiscordClient,
    state: &mut WatchState,
) {
    let mut active_panes = HashSet::new();

    for session in &config.watch.tmux.sessions {
        if !should_poll_tmux(session, &state.tmux, config.watch.poll_interval_secs) {
            continue;
        }

        match snapshot_tmux_session(session).await {
            Ok(panes) => {
                for pane in panes {
                    active_panes.insert(pane.pane_id.clone());
                    let pane_key = pane.pane_id.clone();
                    let now = Instant::now();
                    let hash = content_hash(&pane.content);
                    let latest_line = last_nonempty_line(&pane.content);

                    if let Some(existing) = state.tmux.get_mut(&pane_key) {
                        let previous_snapshot = existing.snapshot.clone();
                        if existing.last_hash != hash {
                            let hits = collect_keyword_hits(
                                &previous_snapshot,
                                &pane.content,
                                &session.keyword_patterns,
                            );
                            for hit in hits {
                                let event = IncomingEvent::tmux_keyword(
                                    pane.session.clone(),
                                    hit.keyword,
                                    hit.line,
                                    session.channel.clone(),
                                )
                                .with_format(session.format.clone())
                                .with_template(session.template.clone());
                                if let Err(error) = router.dispatch(&event, discord).await {
                                    eprintln!("clawhip watch tmux keyword event failed: {error}");
                                }
                            }

                            existing.last_hash = hash;
                            existing.snapshot = pane.content;
                            existing.last_change = now;
                            existing.last_stale_notification = None;
                            existing.last_polled = now;
                            existing.session = pane.session;
                            existing.pane_name = pane.pane_name;
                        } else {
                            existing.last_polled = now;
                            if should_emit_stale(
                                existing.last_change,
                                existing.last_stale_notification,
                                Duration::from_secs(session.stale_after_minutes.max(1) * 60),
                                Duration::from_secs(
                                    session
                                        .stale_reminder_minutes
                                        .unwrap_or(session.stale_after_minutes)
                                        .max(1)
                                        * 60,
                                ),
                                now,
                            ) {
                                let event = IncomingEvent::tmux_stale(
                                    existing.session.clone(),
                                    existing.pane_name.clone(),
                                    session.stale_after_minutes,
                                    latest_line.clone(),
                                    session.channel.clone(),
                                )
                                .with_format(session.format.clone())
                                .with_template(session.template.clone());
                                if let Err(error) = router.dispatch(&event, discord).await {
                                    eprintln!("clawhip watch tmux stale event failed: {error}");
                                }
                                existing.last_stale_notification = Some(now);
                            }
                        }
                    } else {
                        state.tmux.insert(
                            pane_key,
                            TmuxPaneState {
                                session: pane.session,
                                pane_name: pane.pane_name,
                                last_hash: hash,
                                snapshot: pane.content,
                                last_change: now,
                                last_stale_notification: None,
                                last_polled: now,
                            },
                        );
                    }
                }
            }
            Err(error) => eprintln!("clawhip watch tmux poll failed for {}: {error}", session.session),
        }
    }

    state.tmux.retain(|pane_id, _| active_panes.contains(pane_id));
}

async fn snapshot_git_repo(repo: &GitRepoWatch) -> Result<GitRepoSnapshot> {
    let head = run_command(&git_bin(), &["-C", &repo.path, "rev-parse", "HEAD"]).await?;
    let branch = run_command(
        &git_bin(),
        &["-C", &repo.path, "rev-parse", "--abbrev-ref", "HEAD"],
    )
    .await?;
    let summary = run_command(
        &git_bin(),
        &["-C", &repo.path, "log", "-1", "--pretty=%s"],
    )
    .await?;
    let name = repo.name.clone().unwrap_or_else(|| {
        Path::new(&repo.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&repo.path)
            .to_string()
    });
    let github_repo = repo.github_repo.clone().or(parse_github_repo(
        &run_command(
            &git_bin(),
            &["-C", &repo.path, "config", "--get", &format!("remote.{}.url", repo.remote)],
        )
        .await
        .unwrap_or_default(),
    ));

    Ok(GitRepoSnapshot {
        name,
        branch,
        head: head.clone(),
        commits: vec![CommitEntry { sha: head, summary }],
        github_repo,
    })
}

async fn list_new_commits(repo: &GitRepoWatch, old: &str, new: &str) -> Result<Vec<CommitEntry>> {
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

async fn snapshot_tmux_session(session: &TmuxSessionWatch) -> Result<Vec<TmuxPaneSnapshot>> {
    let output = run_command(
        &tmux_bin(),
        &[
            "list-panes",
            "-t",
            &session.session,
            "-F",
            "#{pane_id}|#{session_name}|#{window_index}.#{pane_index}|#{pane_title}",
        ],
    )
    .await?;

    let mut panes = Vec::new();
    for line in output.lines() {
        let mut parts = line.splitn(4, '|');
        let pane_id = parts.next().unwrap_or_default().to_string();
        if pane_id.is_empty() {
            continue;
        }
        let session_name = parts.next().unwrap_or_default().to_string();
        let pane_name = parts.next().unwrap_or_default().to_string();
        let content = run_command(
            &tmux_bin(),
            &["capture-pane", "-p", "-t", &pane_id, "-S", "-200"],
        )
        .await?;
        panes.push(TmuxPaneSnapshot {
            pane_id,
            session: session_name,
            pane_name,
            content,
        });
    }

    Ok(panes)
}

async fn fetch_pull_requests(
    client: &reqwest::Client,
    api_base: &str,
    repo: &GitRepoWatch,
    snapshot: &GitRepoSnapshot,
) -> Result<HashMap<u64, PullRequestSnapshot>> {
    let github_repo = snapshot
        .github_repo
        .clone()
        .ok_or_else(|| format!("no GitHub repo configured or detected for {}", repo.path))?;
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
                    number: pull.number,
                    title: pull.title,
                    status,
                    url: pull.html_url,
                },
            )
        })
        .collect())
}

fn build_github_client(_config: &WatchConfig, token: Option<String>) -> Result<reqwest::Client> {
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

    Ok(reqwest::Client::builder().default_headers(headers).build()?)
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

fn git_bin() -> String {
    std::env::var("CLAWHIP_GIT_BIN").unwrap_or_else(|_| "git".to_string())
}

fn tmux_bin() -> String {
    std::env::var("CLAWHIP_TMUX_BIN").unwrap_or_else(|_| "tmux".to_string())
}

fn should_poll_git(repo: &GitRepoWatch, state: &HashMap<String, GitRepoState>, default_secs: u64) -> bool {
    let interval = Duration::from_secs(repo.poll_interval_secs.unwrap_or(default_secs).max(1));
    state
        .get(&repo.path)
        .map(|entry| entry.last_polled.elapsed() >= interval)
        .unwrap_or(true)
}

fn should_poll_tmux(
    session: &TmuxSessionWatch,
    state: &HashMap<String, TmuxPaneState>,
    default_secs: u64,
) -> bool {
    let interval = Duration::from_secs(session.poll_interval_secs.unwrap_or(default_secs).max(1));
    let any_matching = state.values().find(|pane| pane.session == session.session);
    any_matching
        .map(|entry| entry.last_polled.elapsed() >= interval)
        .unwrap_or(true)
}

fn content_hash(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

fn collect_keyword_hits(previous: &str, current: &str, patterns: &[String]) -> Vec<KeywordHit> {
    if patterns.is_empty() {
        return Vec::new();
    }

    let previous_lines: HashSet<&str> = previous.lines().collect();
    current
        .lines()
        .filter(|line| !previous_lines.contains(*line))
        .flat_map(|line| {
            patterns.iter().filter_map(move |pattern| {
                if line.to_ascii_lowercase().contains(&pattern.to_ascii_lowercase()) {
                    Some(KeywordHit {
                        keyword: pattern.clone(),
                        line: line.to_string(),
                    })
                } else {
                    None
                }
            })
        })
        .collect()
}

fn should_emit_stale(
    last_change: Instant,
    last_notification: Option<Instant>,
    stale_after: Duration,
    remind_every: Duration,
    now: Instant,
) -> bool {
    if now.duration_since(last_change) < stale_after {
        return false;
    }

    match last_notification {
        None => true,
        Some(last_notification) => now.duration_since(last_notification) >= remind_every,
    }
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

#[derive(Debug)]
struct KeywordHit {
    keyword: String,
    line: String,
}

#[derive(Debug, Deserialize)]
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
    fn keyword_hits_only_new_matching_lines() {
        let hits = collect_keyword_hits(
            "line one\nall good",
            "line one\nall good\nerror: build failed\ncomplete",
            &["error".into(), "complete".into()],
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].keyword, "error");
        assert_eq!(hits[1].keyword, "complete");
    }

    #[test]
    fn stale_emission_respects_threshold_and_reminder() {
        let start = Instant::now();
        assert!(should_emit_stale(
            start,
            None,
            Duration::from_secs(60),
            Duration::from_secs(120),
            start + Duration::from_secs(61),
        ));
        assert!(!should_emit_stale(
            start,
            Some(start + Duration::from_secs(61)),
            Duration::from_secs(60),
            Duration::from_secs(120),
            start + Duration::from_secs(100),
        ));
    }
}
