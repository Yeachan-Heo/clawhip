use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::Result;
use crate::event::{
    AgentEvent, CustomEvent, EventBody, EventEnvelope, EventMetadata, EventPriority,
    GitBranchChangedEvent, GitCommitAggregatedEvent, GitCommitEvent, GitHubCIEvent,
    GitHubIssueEvent, GitHubPREvent, GitHubPRStatusEvent, TmuxKeywordAggregatedEvent,
    TmuxKeywordEvent, TmuxStaleEvent,
};
use crate::events::{IncomingEvent, normalize_event};

pub fn from_incoming_event(event: &IncomingEvent) -> Result<EventEnvelope> {
    EventEnvelope::try_from(event)
}

impl TryFrom<&IncomingEvent> for EventEnvelope {
    type Error = crate::DynError;

    fn try_from(event: &IncomingEvent) -> Result<Self> {
        let normalized = normalize_event(event.clone());
        let kind = normalized.canonical_kind();
        let payload = &normalized.payload;

        Ok(Self {
            id: event_id_for(&normalized).unwrap_or_else(Uuid::new_v4),
            timestamp: OffsetDateTime::now_utc(),
            source: source_for_kind(kind),
            body: body_for(kind, payload)?,
            metadata: EventMetadata {
                channel_hint: normalized.channel.clone(),
                mention: normalized
                    .mention
                    .clone()
                    .or_else(|| optional_string_field(payload, "mention")),
                format: normalized.format.clone(),
                template: normalized.template.clone(),
                priority: priority_for(kind, payload),
            },
        })
    }
}

fn body_for(kind: &str, payload: &Value) -> Result<EventBody> {
    match kind {
        "git.commit" => git_commit_body(payload),
        "git.branch-changed" => Ok(EventBody::GitBranchChanged(GitBranchChangedEvent {
            repo: string_field(payload, "repo")?,
            old_branch: string_field(payload, "old_branch")?,
            new_branch: string_field(payload, "new_branch")?,
        })),
        "github.issue-opened" => Ok(EventBody::GitHubIssueOpened(github_issue_event(payload)?)),
        "github.issue-commented" => Ok(EventBody::GitHubIssueCommented(github_issue_event(
            payload,
        )?)),
        "github.issue-closed" => Ok(EventBody::GitHubIssueClosed(github_issue_event(payload)?)),
        "github.pr-status-changed" => github_pr_body(payload),
        "github.ci-failed" => Ok(EventBody::GitHubCIFailed(GitHubCIEvent {
            repo: string_field(payload, "repo")?,
            number: payload.get("number").and_then(Value::as_u64),
            branch: optional_string_field(payload, "branch"),
            sha: optional_string_field(payload, "sha"),
            status: optional_string_field(payload, "status"),
            conclusion: optional_string_field(payload, "conclusion"),
            url: optional_string_field(payload, "url"),
            workflow: optional_string_field(payload, "workflow"),
            message: optional_string_field(payload, "message"),
        })),
        "tmux.keyword" => tmux_keyword_body(payload),
        "tmux.stale" => Ok(EventBody::TmuxStale(TmuxStaleEvent {
            session: string_field(payload, "session")?,
            pane: string_field(payload, "pane")?,
            minutes: u64_field(payload, "minutes")?,
            last_line: string_field(payload, "last_line")?,
        })),
        "agent.started" | "session.started" => Ok(EventBody::AgentStarted(agent_event(payload)?)),
        "agent.blocked" | "session.blocked" => Ok(EventBody::AgentBlocked(agent_event(payload)?)),
        "agent.finished" | "session.finished" => {
            Ok(EventBody::AgentFinished(agent_event(payload)?))
        }
        "agent.failed" | "session.failed" => Ok(EventBody::AgentFailed(agent_event(payload)?)),
        _ => Ok(EventBody::Custom(CustomEvent {
            kind: kind.to_string(),
            message: optional_string_field(payload, "message").unwrap_or_else(|| kind.to_string()),
            payload: if payload.is_null() {
                None
            } else {
                Some(payload.clone())
            },
        })),
    }
}

fn git_commit_body(payload: &Value) -> Result<EventBody> {
    let repo = string_field(payload, "repo")?;
    let branch = string_field(payload, "branch")?;

    let commits = payload
        .get("commits")
        .and_then(Value::as_array)
        .map(|commits| {
            commits
                .iter()
                .map(|commit| -> Result<_> {
                    Ok(GitCommitEvent {
                        repo: repo.clone(),
                        branch: branch.clone(),
                        sha: string_field(commit, "commit")?,
                        short_sha: string_field(commit, "short_commit")?,
                        summary: string_field(commit, "summary")?,
                    })
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();

    if commits.len() > 1
        || payload
            .get("commit_count")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 1
    {
        return Ok(EventBody::GitCommitAggregated(GitCommitAggregatedEvent {
            repo,
            branch,
            commit_count: payload
                .get("commit_count")
                .and_then(Value::as_u64)
                .map(|count| count as usize)
                .unwrap_or(commits.len()),
            commits,
        }));
    }

    Ok(EventBody::GitCommit(GitCommitEvent {
        repo,
        branch,
        sha: string_field(payload, "commit")?,
        short_sha: string_field(payload, "short_commit")?,
        summary: string_field(payload, "summary")?,
    }))
}

fn github_issue_event(payload: &Value) -> Result<GitHubIssueEvent> {
    Ok(GitHubIssueEvent {
        repo: string_field(payload, "repo")?,
        number: u64_field(payload, "number")?,
        title: string_field(payload, "title")?,
        comments: payload.get("comments").and_then(Value::as_u64),
    })
}

fn github_pr_body(payload: &Value) -> Result<EventBody> {
    let pr = GitHubPREvent {
        repo: string_field(payload, "repo")?,
        number: u64_field(payload, "number")?,
        title: string_field(payload, "title")?,
        url: string_field(payload, "url")?,
    };
    let old_status = string_field(payload, "old_status")?;
    let new_status = string_field(payload, "new_status")?;

    match new_status.as_str() {
        "open" if old_status == "<new>" || old_status == "closed" => {
            Ok(EventBody::GitHubPROpened(pr))
        }
        "merged" => Ok(EventBody::GitHubPRMerged(pr)),
        _ => Ok(EventBody::GitHubPRStatusChanged(GitHubPRStatusEvent {
            repo: pr.repo,
            number: pr.number,
            title: pr.title,
            old_status,
            new_status,
            url: pr.url,
        })),
    }
}

fn tmux_keyword_body(payload: &Value) -> Result<EventBody> {
    let session = string_field(payload, "session")?;
    let hits = payload
        .get("hits")
        .and_then(Value::as_array)
        .map(|hits| {
            hits.iter()
                .map(|hit| -> Result<_> {
                    Ok(TmuxKeywordEvent {
                        session: session.clone(),
                        keyword: string_field(hit, "keyword")?,
                        line: string_field(hit, "line")?,
                    })
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();

    if hits.len() > 1
        || payload
            .get("hit_count")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 1
    {
        return Ok(EventBody::TmuxKeywordAggregated(
            TmuxKeywordAggregatedEvent {
                session,
                hit_count: payload
                    .get("hit_count")
                    .and_then(Value::as_u64)
                    .map(|count| count as usize)
                    .unwrap_or(hits.len()),
                hits,
            },
        ));
    }

    Ok(EventBody::TmuxKeyword(TmuxKeywordEvent {
        session,
        keyword: string_field(payload, "keyword")?,
        line: string_field(payload, "line")?,
    }))
}

fn agent_event(payload: &Value) -> Result<AgentEvent> {
    Ok(AgentEvent {
        agent_name: string_field(payload, "agent_name")?,
        status: string_field(payload, "status")?,
        session_id: optional_string_field(payload, "session_id"),
        project: optional_string_field(payload, "project"),
        elapsed_secs: payload.get("elapsed_secs").and_then(Value::as_u64),
        summary: optional_string_field(payload, "summary"),
        error_message: optional_string_field(payload, "error_message"),
        mention: optional_string_field(payload, "mention"),
    })
}

fn priority_for(kind: &str, payload: &Value) -> EventPriority {
    match kind {
        "agent.failed" | "session.failed" | "session.test-failed" | "github.ci-failed" => {
            EventPriority::Critical
        }
        "agent.blocked"
        | "session.blocked"
        | "session.retry-needed"
        | "session.handoff-needed"
        | "tmux.stale" => EventPriority::High,
        "github.pr-status-changed"
            if optional_string_field(payload, "new_status")
                .map(|status| status == "merged" || status == "closed")
                .unwrap_or(false) =>
        {
            EventPriority::High
        }
        "custom" => EventPriority::Low,
        _ => EventPriority::Normal,
    }
}

fn source_for_kind(kind: &str) -> String {
    kind.split('.').next().unwrap_or("custom").to_string()
}

fn string_field(payload: &Value, key: &str) -> Result<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| format!("missing string field '{key}'").into())
}

fn optional_string_field(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn event_id_for(event: &IncomingEvent) -> Option<Uuid> {
    event
        .payload
        .get("event_id")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
}

fn u64_field(payload: &Value, key: &str) -> Result<u64> {
    payload
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("missing u64 field '{key}'").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::IncomingEvent;
    use serde_json::json;

    #[test]
    fn converts_aggregated_git_commits() {
        let event = IncomingEvent::git_commit_events(
            "clawhip".into(),
            "main".into(),
            vec![
                ("abcdef123456".into(), "first".into()),
                ("123456abcdef".into(), "second".into()),
            ],
            Some("ops".into()),
        )
        .into_iter()
        .next()
        .unwrap();

        let envelope = from_incoming_event(&event).unwrap();
        assert_eq!(envelope.source, "git");
        assert_eq!(envelope.metadata.channel_hint.as_deref(), Some("ops"));
        match envelope.body {
            EventBody::GitCommitAggregated(body) => {
                assert_eq!(body.commit_count, 2);
                assert_eq!(body.commits.len(), 2);
                assert_eq!(body.commits[0].summary, "first");
            }
            other => panic!("expected aggregated git commit, got {other:?}"),
        }
    }

    #[test]
    fn converts_tmux_keyword_hits() {
        let event = IncomingEvent::tmux_keywords(
            "issue-48".into(),
            vec![
                ("panic".into(), "boom".into()),
                ("error".into(), "bad".into()),
            ],
            None,
        );

        let envelope = from_incoming_event(&event).unwrap();
        match envelope.body {
            EventBody::TmuxKeywordAggregated(body) => {
                assert_eq!(body.session, "issue-48");
                assert_eq!(body.hit_count, 2);
            }
            other => panic!("expected aggregated tmux keyword, got {other:?}"),
        }
    }

    #[test]
    fn maps_pr_open_and_merge_statuses() {
        let opened = IncomingEvent::github_pr_status_changed(
            "clawhip".into(),
            48,
            "Phase 1".into(),
            "<new>".into(),
            "open".into(),
            "https://example.test/pr/48".into(),
            None,
        );
        let merged = IncomingEvent::github_pr_status_changed(
            "clawhip".into(),
            48,
            "Phase 1".into(),
            "open".into(),
            "merged".into(),
            "https://example.test/pr/48".into(),
            None,
        );

        assert!(matches!(
            from_incoming_event(&opened).unwrap().body,
            EventBody::GitHubPROpened(_)
        ));
        assert!(matches!(
            from_incoming_event(&merged).unwrap().body,
            EventBody::GitHubPRMerged(_)
        ));
    }

    #[test]
    fn keeps_unknown_events_as_custom() {
        let event = IncomingEvent {
            kind: "plugin.custom".into(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: json!({"message": "hello", "extra": true}),
        };

        let envelope = from_incoming_event(&event).unwrap();
        match envelope.body {
            EventBody::Custom(body) => {
                assert_eq!(body.kind, "plugin.custom");
                assert_eq!(body.message, "hello");
                assert_eq!(body.payload.unwrap()["extra"], json!(true));
            }
            other => panic!("expected custom body, got {other:?}"),
        }
    }

    #[test]
    fn keeps_github_ci_failed_route_compatibility_fields() {
        let event = IncomingEvent::github_ci(
            "github.ci-failed",
            "clawhip".into(),
            Some(58),
            "CI / test".into(),
            "completed".into(),
            Some("failure".into()),
            "abcdef1234567890".into(),
            "https://github.com/Yeachan-Heo/clawhip/actions/runs/1".into(),
            Some("feat/branch".into()),
            Some("alerts".into()),
        );

        let envelope = from_incoming_event(&event).unwrap();
        assert_eq!(envelope.metadata.channel_hint.as_deref(), Some("alerts"));
        match envelope.body {
            EventBody::GitHubCIFailed(body) => {
                assert_eq!(body.repo, "clawhip");
                assert_eq!(body.number, Some(58));
                assert_eq!(body.workflow.as_deref(), Some("CI / test"));
                assert_eq!(body.status.as_deref(), Some("completed"));
                assert_eq!(body.conclusion.as_deref(), Some("failure"));
                assert_eq!(body.sha.as_deref(), Some("abcdef1234567890"));
                assert_eq!(
                    body.url.as_deref(),
                    Some("https://github.com/Yeachan-Heo/clawhip/actions/runs/1")
                );
            }
            other => panic!("expected GitHubCIFailed body, got {other:?}"),
        }
    }

    #[test]
    fn reuses_normalized_event_id_when_available() {
        let event = crate::events::normalize_event(IncomingEvent::custom(None, "hello".into()));
        let envelope = from_incoming_event(&event).unwrap();

        assert_eq!(
            envelope.id.to_string(),
            event.payload["event_id"].as_str().unwrap()
        );
    }
}
