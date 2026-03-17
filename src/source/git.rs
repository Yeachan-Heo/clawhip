use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

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

struct GitRepoState {
    branch: String,
    head: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommitEntry {
    pub(crate) sha: String,
    pub(crate) summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitSnapshot {
    pub(crate) repo_name: String,
    pub(crate) branch: String,
    pub(crate) head: String,
    pub(crate) commits: Vec<CommitEntry>,
    pub(crate) github_repo: Option<String>,
}

async fn poll_git(
    config: &AppConfig,
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut HashMap<String, GitRepoState>,
) -> Result<()> {
    for repo in &config.monitors.git.repos {
        match snapshot_git_repo(repo).await {
            Ok(snapshot) => {
                if let Some(previous) = state.get(&repo.path) {
                    if repo.emit_branch_changes && previous.branch != snapshot.branch {
                        send_event(
                            tx,
                            IncomingEvent::git_branch_changed(
                                snapshot.repo_name.clone(),
                                previous.branch.clone(),
                                snapshot.branch.clone(),
                                repo.channel.clone(),
                            )
                            .with_mention(repo.mention.clone())
                            .with_format(repo.format.clone()),
                        )
                        .await?;
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
                            send_event(
                                tx,
                                event
                                    .with_mention(repo.mention.clone())
                                    .with_format(repo.format.clone()),
                            )
                            .await?;
                        }
                    }
                }

                state.insert(
                    repo.path.clone(),
                    GitRepoState {
                        branch: snapshot.branch,
                        head: snapshot.head,
                    },
                );
            }
            Err(error) => eprintln!(
                "clawhip source git snapshot failed for {}: {error}",
                repo.path
            ),
        }
    }

    Ok(())
}

async fn send_event(tx: &mpsc::Sender<IncomingEvent>, event: IncomingEvent) -> Result<()> {
    tx.send(event)
        .await
        .map_err(|error| format!("git source channel closed: {error}").into())
}

pub(crate) async fn snapshot_git_repo(repo: &GitRepoMonitor) -> Result<GitSnapshot> {
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
        repo_name: repo_display_name(repo),
        branch,
        head: head.clone(),
        commits: vec![CommitEntry { sha: head, summary }],
        github_repo: repo
            .github_repo
            .clone()
            .or_else(|| parse_github_repo(&remote_url)),
    })
}

pub(crate) async fn list_new_commits(
    repo: &GitRepoMonitor,
    old: &str,
    new: &str,
) -> Result<Vec<CommitEntry>> {
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
}
