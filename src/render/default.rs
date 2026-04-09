use std::path::Path;

use serde_json::Value;

use crate::Result;
use crate::events::{IncomingEvent, MessageFormat};

use super::Renderer;

#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultRenderer;

impl Renderer for DefaultRenderer {
    fn render(&self, event: &IncomingEvent, format: &MessageFormat) -> Result<String> {
        let payload = &event.payload;
        if event.canonical_kind().starts_with("session.") {
            return render_session_event(event.canonical_kind(), payload, format);
        }
        if event.canonical_kind().starts_with("workspace.") {
            return render_workspace_event(event.canonical_kind(), payload, format);
        }
        if event.canonical_kind() == "git.commit"
            && let Some(rendered) = render_aggregated_git_commit(payload, format)?
        {
            return Ok(rendered);
        }
        if event.canonical_kind() == "tmux.keyword"
            && let Some(rendered) = render_aggregated_tmux_keyword(payload, format)?
        {
            return Ok(rendered);
        }

        let text = match (event.canonical_kind(), format) {
            ("custom", MessageFormat::Compact | MessageFormat::Inline) => {
                string_field(payload, "message")?
            }
            ("custom", MessageFormat::Alert) => {
                format!("🚨 {}", string_field(payload, "message")?)
            }
            ("custom", MessageFormat::Raw) => serde_json::to_string_pretty(payload)?,

            ("agent.started", MessageFormat::Compact)
            | ("agent.blocked", MessageFormat::Compact)
            | ("agent.finished", MessageFormat::Compact)
            | ("agent.failed", MessageFormat::Compact) => format!(
                "{}agent {}{}",
                agent_optional_mention_prefix(payload),
                string_field(payload, "agent_name")?,
                agent_detail_suffix(payload)
            ),
            ("agent.started", MessageFormat::Alert)
            | ("agent.blocked", MessageFormat::Alert)
            | ("agent.finished", MessageFormat::Alert)
            | ("agent.failed", MessageFormat::Alert) => format!(
                "🚨 {}agent {}{}",
                agent_optional_mention_prefix(payload),
                string_field(payload, "agent_name")?,
                agent_detail_suffix(payload)
            ),
            ("agent.started", MessageFormat::Inline)
            | ("agent.blocked", MessageFormat::Inline)
            | ("agent.finished", MessageFormat::Inline)
            | ("agent.failed", MessageFormat::Inline) => format!(
                "{}[agent:{}] {}{}",
                agent_optional_mention_prefix(payload),
                string_field(payload, "agent_name")?,
                string_field(payload, "status")?,
                agent_inline_suffix(payload)
            ),
            ("agent.started", MessageFormat::Raw)
            | ("agent.blocked", MessageFormat::Raw)
            | ("agent.finished", MessageFormat::Raw)
            | ("agent.failed", MessageFormat::Raw) => serde_json::to_string_pretty(payload)?,

            ("github.issue-opened", MessageFormat::Compact) => format!(
                "{}#{} opened: {}",
                string_field(payload, "repo")?,
                payload.field_u64("number")?,
                string_field(payload, "title")?
            ),
            ("github.issue-opened", MessageFormat::Alert) => format!(
                "🚨 GitHub issue opened in {}: #{} {}",
                string_field(payload, "repo")?,
                payload.field_u64("number")?,
                string_field(payload, "title")?
            ),
            ("github.issue-opened", MessageFormat::Inline) => format!(
                "[GitHub] {}#{} {}",
                string_field(payload, "repo")?,
                payload.field_u64("number")?,
                string_field(payload, "title")?
            ),
            ("github.issue-opened", MessageFormat::Raw) => serde_json::to_string_pretty(payload)?,
            ("github.issue-commented", MessageFormat::Compact) => format!(
                "{}#{} commented ({} comments): {}",
                string_field(payload, "repo")?,
                payload.field_u64("number")?,
                payload.field_u64("comments")?,
                string_field(payload, "title")?
            ),
            ("github.issue-commented", MessageFormat::Alert) => format!(
                "🚨 GitHub issue commented in {}: #{} {}",
                string_field(payload, "repo")?,
                payload.field_u64("number")?,
                string_field(payload, "title")?
            ),
            ("github.issue-commented", MessageFormat::Inline) => format!(
                "[GitHub comment] {}#{} {}",
                string_field(payload, "repo")?,
                payload.field_u64("number")?,
                string_field(payload, "title")?
            ),
            ("github.issue-commented", MessageFormat::Raw) => {
                serde_json::to_string_pretty(payload)?
            }
            ("github.issue-closed", MessageFormat::Compact) => format!(
                "{}#{} closed: {}",
                string_field(payload, "repo")?,
                payload.field_u64("number")?,
                string_field(payload, "title")?
            ),
            ("github.issue-closed", MessageFormat::Alert) => format!(
                "🚨 GitHub issue closed in {}: #{} {}",
                string_field(payload, "repo")?,
                payload.field_u64("number")?,
                string_field(payload, "title")?
            ),
            ("github.issue-closed", MessageFormat::Inline) => format!(
                "[GitHub closed] {}#{} {}",
                string_field(payload, "repo")?,
                payload.field_u64("number")?,
                string_field(payload, "title")?
            ),
            ("github.issue-closed", MessageFormat::Raw) => serde_json::to_string_pretty(payload)?,

            ("git.commit", MessageFormat::Compact) => format!(
                "git:{}@{} {} {}",
                git_repo_label(payload)?,
                string_field(payload, "branch")?,
                string_field(payload, "short_commit")?,
                string_field(payload, "summary")?
            ),
            ("git.commit", MessageFormat::Alert) => format!(
                "🚨 new commit in {}@{}: {} {}",
                git_repo_label(payload)?,
                string_field(payload, "branch")?,
                string_field(payload, "short_commit")?,
                string_field(payload, "summary")?
            ),
            ("git.commit", MessageFormat::Inline) => format!(
                "[git] {} {}",
                git_repo_label(payload)?,
                string_field(payload, "summary")?
            ),
            ("git.commit", MessageFormat::Raw) => serde_json::to_string_pretty(payload)?,

            ("git.branch-changed", MessageFormat::Compact) => format!(
                "git:{} branch changed {} -> {}",
                git_repo_label(payload)?,
                string_field(payload, "old_branch")?,
                string_field(payload, "new_branch")?
            ),
            ("git.branch-changed", MessageFormat::Alert) => format!(
                "🚨 git repo {} branch changed {} -> {}",
                git_repo_label(payload)?,
                string_field(payload, "old_branch")?,
                string_field(payload, "new_branch")?
            ),
            ("git.branch-changed", MessageFormat::Inline) => format!(
                "[git:{}] {} -> {}",
                git_repo_label(payload)?,
                string_field(payload, "old_branch")?,
                string_field(payload, "new_branch")?
            ),
            ("git.branch-changed", MessageFormat::Raw) => serde_json::to_string_pretty(payload)?,

            ("github.pr-status-changed", MessageFormat::Compact) => format!(
                "PR {}#{} {} -> {}: {}",
                string_field(payload, "repo")?,
                payload.field_u64("number")?,
                string_field(payload, "old_status")?,
                string_field(payload, "new_status")?,
                string_field(payload, "title")?
            ),
            ("github.pr-status-changed", MessageFormat::Alert) => format!(
                "🚨 PR status changed in {}: #{} {} -> {} ({})",
                string_field(payload, "repo")?,
                payload.field_u64("number")?,
                string_field(payload, "old_status")?,
                string_field(payload, "new_status")?,
                string_field(payload, "title")?
            ),
            ("github.pr-status-changed", MessageFormat::Inline) => format!(
                "[PR {}#{}] {} -> {}",
                string_field(payload, "repo")?,
                payload.field_u64("number")?,
                string_field(payload, "old_status")?,
                string_field(payload, "new_status")?
            ),
            ("github.pr-status-changed", MessageFormat::Raw) => {
                serde_json::to_string_pretty(payload)?
            }

            (
                "github.ci-started"
                | "github.ci-failed"
                | "github.ci-passed"
                | "github.ci-cancelled",
                MessageFormat::Compact,
            ) => render_github_ci(payload, event.canonical_kind(), true)?,
            (
                "github.ci-started"
                | "github.ci-failed"
                | "github.ci-passed"
                | "github.ci-cancelled",
                MessageFormat::Alert,
            ) => format!(
                "🚨 {}",
                render_github_ci(payload, event.canonical_kind(), true)?
            ),
            (
                "github.ci-started"
                | "github.ci-failed"
                | "github.ci-passed"
                | "github.ci-cancelled",
                MessageFormat::Inline,
            ) => render_github_ci(payload, event.canonical_kind(), false)?,
            (
                "github.ci-started"
                | "github.ci-failed"
                | "github.ci-passed"
                | "github.ci-cancelled",
                MessageFormat::Raw,
            ) => serde_json::to_string_pretty(payload)?,

            (
                "github.release-published" | "github.release-prereleased" | "github.release-edited",
                MessageFormat::Compact,
            ) => render_github_release(payload, event.canonical_kind())?,
            (
                "github.release-published" | "github.release-prereleased" | "github.release-edited",
                MessageFormat::Alert,
            ) => format!(
                "🚨 {}",
                render_github_release(payload, event.canonical_kind())?
            ),
            (
                "github.release-published" | "github.release-prereleased" | "github.release-edited",
                MessageFormat::Inline,
            ) => {
                let tag = string_field(payload, "tag")?;
                let repo = string_field(payload, "repo")?;
                let prerelease = payload
                    .get("is_prerelease")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let suffix = if prerelease { " (pre)" } else { "" };
                format!("[release] {repo} {tag}{suffix}")
            }
            (
                "github.release-published" | "github.release-prereleased" | "github.release-edited",
                MessageFormat::Raw,
            ) => serde_json::to_string_pretty(payload)?,

            ("tmux.keyword", MessageFormat::Compact) => format!(
                "tmux:{} matched '{}' => {}",
                string_field(payload, "session")?,
                string_field(payload, "keyword")?,
                string_field(payload, "line")?
            ),
            ("tmux.keyword", MessageFormat::Alert) => format!(
                "🚨 tmux session {} hit keyword '{}': {}",
                string_field(payload, "session")?,
                string_field(payload, "keyword")?,
                string_field(payload, "line")?
            ),
            ("tmux.keyword", MessageFormat::Inline) => format!(
                "[tmux:{}] {}",
                string_field(payload, "session")?,
                string_field(payload, "line")?
            ),
            ("tmux.keyword", MessageFormat::Raw) => serde_json::to_string_pretty(payload)?,

            ("tmux.stale", MessageFormat::Compact) => format!(
                "tmux:{} pane {} stale for {}m (last: {})",
                string_field(payload, "session")?,
                string_field(payload, "pane")?,
                payload.field_u64("minutes")?,
                string_field(payload, "last_line")?
            ),
            ("tmux.stale", MessageFormat::Alert) => format!(
                "🚨 tmux session {} pane {} stale for {}m (last: {})",
                string_field(payload, "session")?,
                string_field(payload, "pane")?,
                payload.field_u64("minutes")?,
                string_field(payload, "last_line")?
            ),
            ("tmux.stale", MessageFormat::Inline) => format!(
                "[tmux stale:{} {}] {}m",
                string_field(payload, "session")?,
                string_field(payload, "pane")?,
                payload.field_u64("minutes")?
            ),
            ("tmux.stale", MessageFormat::Raw) => serde_json::to_string_pretty(payload)?,

            (_, MessageFormat::Raw) => serde_json::to_string_pretty(payload)?,
            (_, _) => serde_json::to_string(payload)?,
        };

        Ok(text)
    }
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

fn optional_u64_field(payload: &Value, key: &str) -> Option<u64> {
    payload.get(key).and_then(Value::as_u64)
}

fn optional_bool_field(payload: &Value, key: &str) -> Option<bool> {
    match payload.get(key) {
        Some(Value::Bool(value)) => Some(*value),
        Some(Value::Number(value)) => value.as_u64().map(|number| number != 0),
        Some(Value::String(value)) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "attached" => Some(true),
            "0" | "false" | "no" | "detached" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn agent_optional_mention_prefix(payload: &Value) -> String {
    optional_string_field(payload, "mention")
        .map(|mention| format!("{mention} "))
        .unwrap_or_default()
}

fn agent_context_parts(payload: &Value) -> Vec<String> {
    let mut parts = Vec::new();

    if let Some(project) = optional_string_field(payload, "project") {
        parts.push(format!("project={project}"));
    }
    if let Some(session_id) = optional_string_field(payload, "session_id") {
        parts.push(format!("session={session_id}"));
    }
    if let Some(elapsed_secs) = optional_u64_field(payload, "elapsed_secs") {
        parts.push(format!("elapsed={elapsed_secs}s"));
    }

    parts
}

fn agent_detail_suffix(payload: &Value) -> String {
    let mut parts = vec![string_field(payload, "status").unwrap_or_default()];
    parts.extend(agent_context_parts(payload));

    if let Some(summary) = optional_string_field(payload, "summary") {
        parts.push(format!("summary={summary}"));
    }
    if let Some(error_message) = optional_string_field(payload, "error_message") {
        parts.push(format!("error={error_message}"));
    }

    format!(" ({})", parts.join(", "))
}

fn agent_inline_suffix(payload: &Value) -> String {
    let mut parts = agent_context_parts(payload);

    if let Some(summary) = optional_string_field(payload, "summary") {
        parts.push(summary);
    }
    if let Some(error_message) = optional_string_field(payload, "error_message") {
        parts.push(format!("error: {error_message}"));
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(" · "))
    }
}

fn render_session_event(kind: &str, payload: &Value, format: &MessageFormat) -> Result<String> {
    let label = session_subject(payload);
    let status = session_status_label(kind, payload);
    let detail = session_detail_suffix(payload);
    let inline = session_inline_suffix(payload);
    let prefix = agent_optional_mention_prefix(payload);

    Ok(match format {
        MessageFormat::Compact => format!("{prefix}{label} {status}{detail}"),
        MessageFormat::Alert => format!("🚨 {prefix}{label} {status}{detail}"),
        MessageFormat::Inline => format!("{prefix}[{label}] {status}{inline}"),
        MessageFormat::Raw => serde_json::to_string_pretty(payload)?,
    })
}

fn session_subject(payload: &Value) -> String {
    let tool = optional_string_field(payload, "tool").unwrap_or_else(|| "session".to_string());
    let session = optional_string_field(payload, "session_name")
        .or_else(|| optional_string_field(payload, "session_id"));
    match session {
        Some(session) => format!("{tool} {session}"),
        None => tool,
    }
}

fn session_status_label(kind: &str, payload: &Value) -> String {
    match kind {
        "session.started"
        | "session.blocked"
        | "session.finished"
        | "session.failed"
        | "session.prompt-submitted"
        | "session.prompt-delivered"
        | "session.prompt-delivery-failed"
        | "session.stopped" => optional_string_field(payload, "status").unwrap_or_else(|| {
            kind.strip_prefix("session.")
                .unwrap_or(kind)
                .replace('-', " ")
        }),
        _ => kind.strip_prefix("session.").unwrap_or(kind).to_string(),
    }
}

fn session_detail_suffix(payload: &Value) -> String {
    let mut parts = Vec::new();

    if let Some(repo_name) = optional_string_field(payload, "repo_name")
        .or_else(|| optional_string_field(payload, "project"))
    {
        parts.push(format!("repo={repo_name}"));
    }
    if let Some(issue_number) = optional_u64_field(payload, "issue_number") {
        parts.push(format!("issue=#{issue_number}"));
    }
    if let Some(pr_number) = optional_u64_field(payload, "pr_number") {
        parts.push(format!("pr=#{pr_number}"));
    }
    if let Some(branch) = optional_string_field(payload, "branch") {
        parts.push(format!("branch={branch}"));
    }
    if let Some(test_runner) = optional_string_field(payload, "test_runner") {
        parts.push(format!("runner={test_runner}"));
    }
    if let Some(elapsed_secs) = optional_u64_field(payload, "elapsed_secs") {
        parts.push(format!("elapsed={elapsed_secs}s"));
    }
    if let Some(summary) = optional_string_field(payload, "summary") {
        parts.push(format!("summary={summary}"));
    }
    if let Some(error_message) = optional_string_field(payload, "error_message") {
        parts.push(format!("error={error_message}"));
    }
    if let Some(tmux_identity) = tmux_identity(payload) {
        parts.push(format!("tmux={tmux_identity}"));
    }
    if let Some(tmux_pane_tty) = optional_string_field(payload, "tmux_pane_tty") {
        parts.push(format!("pane_tty={tmux_pane_tty}"));
    }
    if let Some(tmux_client_count) = optional_u64_field(payload, "tmux_client_count") {
        parts.push(format!("clients={tmux_client_count}"));
    }
    if let Some(tmux_attached) = optional_bool_field(payload, "tmux_attached") {
        parts.push(if tmux_attached {
            "attached".to_string()
        } else {
            "detached".to_string()
        });
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

fn session_inline_suffix(payload: &Value) -> String {
    let mut parts = Vec::new();

    if let Some(repo_name) = optional_string_field(payload, "repo_name")
        .or_else(|| optional_string_field(payload, "project"))
    {
        parts.push(repo_name);
    }
    if let Some(issue_number) = optional_u64_field(payload, "issue_number") {
        parts.push(format!("issue #{issue_number}"));
    }
    if let Some(pr_number) = optional_u64_field(payload, "pr_number") {
        parts.push(format!("PR #{pr_number}"));
    }
    if let Some(branch) = optional_string_field(payload, "branch") {
        parts.push(branch);
    }
    if let Some(test_runner) = optional_string_field(payload, "test_runner") {
        parts.push(test_runner);
    }
    if let Some(elapsed_secs) = optional_u64_field(payload, "elapsed_secs") {
        parts.push(format!("{elapsed_secs}s"));
    }
    if let Some(summary) = optional_string_field(payload, "summary") {
        parts.push(summary);
    }
    if let Some(error_message) = optional_string_field(payload, "error_message") {
        parts.push(format!("error: {error_message}"));
    }
    if let Some(tmux_identity) = tmux_identity(payload) {
        parts.push(format!("tmux {tmux_identity}"));
    }
    if let Some(tmux_pane_tty) = optional_string_field(payload, "tmux_pane_tty") {
        parts.push(tmux_pane_tty);
    }
    if let Some(tmux_client_count) = optional_u64_field(payload, "tmux_client_count") {
        parts.push(format!("{tmux_client_count} clients"));
    }
    if let Some(tmux_attached) = optional_bool_field(payload, "tmux_attached") {
        parts.push(if tmux_attached {
            "attached".to_string()
        } else {
            "detached".to_string()
        });
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(" · "))
    }
}

fn tmux_identity(payload: &Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(session) = optional_string_field(payload, "tmux_session") {
        parts.push(session);
    }
    if let Some(window) = optional_string_field(payload, "tmux_window") {
        parts.push(window);
    }
    if let Some(pane) = optional_string_field(payload, "tmux_pane") {
        parts.push(pane);
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(":"))
    }
}

fn render_github_ci(payload: &Value, kind: &str, include_url: bool) -> Result<String> {
    if payload
        .get("batched")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return render_batched_github_ci(payload, kind, include_url);
    }

    let workflow = string_field(payload, "workflow")?;
    let state = optional_string_field(payload, "conclusion")
        .or_else(|| optional_string_field(payload, "status"))
        .ok_or_else(|| "missing GitHub CI state".to_string())?;
    let sha = short_sha(&string_field(payload, "sha")?);
    let mut parts = vec![
        format!("CI {}", github_ci_action(kind)),
        github_ci_target(payload)?,
        workflow,
        state,
        sha,
    ];

    if include_url {
        parts.push(string_field(payload, "url")?);
    }

    Ok(parts.join(" · "))
}

fn render_batched_github_ci(payload: &Value, kind: &str, include_url: bool) -> Result<String> {
    let jobs = payload
        .get("jobs")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing batched GitHub CI jobs".to_string())?;
    let total = optional_u64_field(payload, "total_count").unwrap_or(jobs.len() as u64);
    let passed = optional_u64_field(payload, "passed_count").unwrap_or(0);
    let skipped = optional_u64_field(payload, "skipped_count").unwrap_or(0);
    let failed = optional_u64_field(payload, "failed_count").unwrap_or(0);
    let cancelled = optional_u64_field(payload, "cancelled_count").unwrap_or(0);
    let workflows = jobs
        .iter()
        .filter_map(|job| job.get("workflow").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(", ");

    let mut parts = vec![match kind {
        "github.ci-passed" => format!(
            "✅ CI passed · {} · {passed}/{total} passed",
            github_ci_target(payload)?
        ),
        "github.ci-failed" => format!("❌ CI failed · {}", github_ci_target(payload)?),
        "github.ci-cancelled" => format!("🟡 CI cancelled · {}", github_ci_target(payload)?),
        _ => format!("⏳ CI running · {}", github_ci_target(payload)?),
    }];

    if !workflows.is_empty() {
        parts.push(workflows);
    }

    if kind == "github.ci-failed" {
        let failed_jobs = jobs
            .iter()
            .filter_map(|job| {
                let workflow = job.get("workflow").and_then(Value::as_str)?;
                let conclusion = job
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .or_else(|| job.get("status").and_then(Value::as_str))?;
                if matches!(conclusion, "success" | "neutral" | "skipped") {
                    None
                } else {
                    Some(format!("{workflow}:{conclusion}"))
                }
            })
            .collect::<Vec<_>>();
        if !failed_jobs.is_empty() {
            parts.push(failed_jobs.join(", "));
        }
    } else {
        if skipped > 0 {
            parts.push(format!("{skipped} skipped"));
        }
        if cancelled > 0 {
            parts.push(format!("{cancelled} cancelled"));
        }
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
    }

    if include_url {
        parts.push(string_field(payload, "url")?);
    }

    Ok(parts.join(" · "))
}

fn github_ci_action(kind: &str) -> &'static str {
    match kind {
        "github.ci-started" => "started",
        "github.ci-failed" => "failed",
        "github.ci-passed" => "passed",
        "github.ci-cancelled" => "cancelled",
        _ => "updated",
    }
}

fn github_ci_target(payload: &Value) -> Result<String> {
    let repo = string_field(payload, "repo")?;
    Ok(match optional_u64_field(payload, "number") {
        Some(number) => format!("{repo}#{number}"),
        None => repo,
    })
}

fn render_github_release(payload: &Value, kind: &str) -> Result<String> {
    let repo = string_field(payload, "repo")?;
    let tag = string_field(payload, "tag")?;
    let name = optional_string_field(payload, "name").unwrap_or_default();
    let url = optional_string_field(payload, "url").unwrap_or_default();
    let prerelease = payload
        .get("is_prerelease")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let action_label = match kind {
        "github.release-prereleased" => "prereleased",
        "github.release-edited" => "edited",
        _ => "published",
    };

    let pre_flag = if prerelease { " (prerelease)" } else { "" };
    let name_part = if name.is_empty() || name == tag {
        String::new()
    } else {
        format!(" \"{name}\"")
    };

    let mut parts = vec![format!(
        "release {action_label} · {repo} {tag}{pre_flag}{name_part}"
    )];
    if !url.is_empty() {
        parts.push(url);
    }
    Ok(parts.join(" · "))
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn git_repo_label(payload: &Value) -> Result<String> {
    let repo = string_field(payload, "repo")?;
    Ok(match worktree_display_name(payload) {
        Some(worktree) => format!("{repo}[wt:{worktree}]"),
        None => repo,
    })
}

fn worktree_display_name(payload: &Value) -> Option<String> {
    let worktree_path = optional_string_field(payload, "worktree_path")?;
    let repo_path = optional_string_field(payload, "repo_path");
    if repo_path.as_deref() == Some(worktree_path.as_str()) {
        return None;
    }

    Path::new(&worktree_path)
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .or(Some(worktree_path))
}

fn render_aggregated_git_commit(payload: &Value, format: &MessageFormat) -> Result<Option<String>> {
    let Some(commits) = payload.get("commits").and_then(Value::as_array) else {
        return Ok(None);
    };
    if commits.len() <= 1 {
        return Ok(None);
    }

    let repo = git_repo_label(payload)?;
    let branch = string_field(payload, "branch")?;
    let summaries = commits
        .iter()
        .filter_map(|commit| {
            commit
                .get("summary")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|summary| !summary.is_empty())
                .map(ToString::to_string)
        })
        .collect::<Vec<_>>();
    let commit_count = optional_u64_field(payload, "commit_count")
        .map(|count| count as usize)
        .unwrap_or(summaries.len());

    let mut lines = vec![match format {
        MessageFormat::Alert => {
            format!("🚨 git:{repo}@{branch} pushed {commit_count} commits:")
        }
        MessageFormat::Compact | MessageFormat::Inline => {
            format!("git:{repo}@{branch} pushed {commit_count} commits:")
        }
        MessageFormat::Raw => return Ok(None),
    }];

    if summaries.len() > 5 {
        for summary in summaries.iter().take(3) {
            lines.push(format!("- {summary}"));
        }
        lines.push(format!("... and {} more", commit_count.saturating_sub(5)));
        for summary in summaries.iter().skip(summaries.len().saturating_sub(2)) {
            lines.push(format!("- {summary}"));
        }
    } else {
        for summary in summaries {
            lines.push(format!("- {summary}"));
        }
    }

    Ok(Some(lines.join("\n")))
}

fn render_aggregated_tmux_keyword(
    payload: &Value,
    format: &MessageFormat,
) -> Result<Option<String>> {
    let Some(hits) = payload.get("hits").and_then(Value::as_array) else {
        return Ok(None);
    };
    if hits.len() <= 1 {
        return Ok(None);
    }

    let session = string_field(payload, "session")?;
    let hit_count = optional_u64_field(payload, "hit_count")
        .map(|count| count as usize)
        .unwrap_or(hits.len());
    let summaries = hits
        .iter()
        .filter_map(|hit| {
            let keyword = hit.get("keyword").and_then(Value::as_str)?.trim();
            let line = hit.get("line").and_then(Value::as_str)?.trim();
            if keyword.is_empty() || line.is_empty() {
                None
            } else {
                Some(format!("'{keyword}': {line}"))
            }
        })
        .collect::<Vec<_>>();

    match format {
        MessageFormat::Compact | MessageFormat::Alert => {
            let header = match format {
                MessageFormat::Alert => {
                    format!("🚨 tmux session {session} hit {hit_count} keyword matches:")
                }
                MessageFormat::Compact => {
                    format!("tmux:{session} matched {hit_count} keyword hits:")
                }
                _ => unreachable!(),
            };
            let mut lines = vec![header];
            lines.extend(summaries.into_iter().map(|summary| format!("- {summary}")));
            Ok(Some(lines.join("\n")))
        }
        MessageFormat::Inline => Ok(Some(format!("[tmux:{session}] {}", summaries.join(" · ")))),
        MessageFormat::Raw => Ok(None),
    }
}

trait ValueExt {
    fn field_u64(&self, key: &str) -> Result<u64>;
}

impl ValueExt for Value {
    fn field_u64(&self, key: &str) -> Result<u64> {
        self.get(key)
            .and_then(Value::as_u64)
            .ok_or_else(|| format!("missing integer field '{key}'").into())
    }
}

fn render_workspace_event(kind: &str, payload: &Value, format: &MessageFormat) -> Result<String> {
    let workspace = optional_string_field(payload, "workspace_name")
        .or_else(|| optional_string_field(payload, "workspace_root"))
        .unwrap_or_else(|| "workspace".to_string());
    let state_file = string_field(payload, "state_file")?;
    let tool = optional_string_field(payload, "tool")
        .or_else(|| optional_string_field(payload, "state_family"))
        .unwrap_or_else(|| "workspace".to_string());
    let summary = optional_string_field(payload, "summary").unwrap_or_else(|| kind.to_string());
    let session = optional_string_field(payload, "session_name")
        .or_else(|| optional_string_field(payload, "session_id"));
    let session_suffix = session
        .map(|value| format!(" · session={value}"))
        .unwrap_or_default();

    match format {
        MessageFormat::Compact => Ok(format!(
            "{}:{} · {} · {}{}",
            tool, workspace, state_file, summary, session_suffix
        )),
        MessageFormat::Alert => Ok(format!(
            "🚨 {}:{} · {} · {}{}",
            tool, workspace, state_file, summary, session_suffix
        )),
        MessageFormat::Inline => Ok(format!(
            "[{}:{}] {}{}",
            tool, workspace, state_file, session_suffix
        )),
        MessageFormat::Raw => serde_json::to_string_pretty(payload).map_err(Into::into),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_workspace_skill_event_compact() {
        let event = IncomingEvent::workspace(
            "workspace.skill.activated".into(),
            json!({
                "workspace_name": "repo-a",
                "state_file": "skill-active-state.json",
                "skill": "ralph",
                "summary": "workspace skill state changed"
            }),
            None,
        );

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Compact)
            .unwrap();
        assert!(rendered.contains("repo-a"));
        assert!(rendered.contains("workspace skill state changed"));
    }

    #[test]
    fn renders_git_commit_with_worktree_suffix_when_distinct() {
        let event = IncomingEvent::git_commit(
            "repo".into(),
            "main".into(),
            "1234567890abcdef".into(),
            "ship it".into(),
            None,
        )
        .with_repo_context(
            Some("/repo/root".into()),
            Some("/repo/root/.worktrees/issue-115".into()),
        );

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Compact)
            .unwrap();
        assert_eq!(rendered, "git:repo[wt:issue-115]@main 1234567 ship it");
    }

    #[test]
    fn does_not_render_worktree_suffix_for_primary_repo_path() {
        let event = IncomingEvent::git_commit(
            "repo".into(),
            "main".into(),
            "1234567890abcdef".into(),
            "ship it".into(),
            None,
        )
        .with_repo_context(Some("/repo/root".into()), Some("/repo/root".into()));

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Compact)
            .unwrap();
        assert_eq!(rendered, "git:repo@main 1234567 ship it");
    }

    #[test]
    fn renders_session_events_with_tmux_metadata() {
        let event = IncomingEvent {
            kind: "session.started".into(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: json!({
                "tool": "codex",
                "session_name": "issue-180",
                "repo_name": "clawhip",
                "tmux_session": "issue-180",
                "tmux_window": "2",
                "tmux_pane": "%11",
                "tmux_pane_tty": "/dev/pts/42",
                "tmux_attached": false,
                "tmux_client_count": 0
            }),
        };

        let compact = DefaultRenderer
            .render(&event, &MessageFormat::Compact)
            .unwrap();
        let inline = DefaultRenderer
            .render(&event, &MessageFormat::Inline)
            .unwrap();

        assert_eq!(
            compact,
            "codex issue-180 started (repo=clawhip, tmux=issue-180:2:%11, pane_tty=/dev/pts/42, clients=0, detached)"
        );
        assert_eq!(
            inline,
            "[codex issue-180] started · clawhip · tmux issue-180:2:%11 · /dev/pts/42 · 0 clients · detached"
        );
    }

    #[test]
    fn renders_release_published_compact() {
        let event = IncomingEvent::github_release(
            "published",
            "Yeachan-Heo/clawhip".into(),
            "v0.6.0".into(),
            "clawhip 0.6.0".into(),
            false,
            "https://github.com/Yeachan-Heo/clawhip/releases/tag/v0.6.0".into(),
            Some("Yeachan-Heo".into()),
            None,
        );

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Compact)
            .unwrap();
        assert!(rendered.contains("release published"));
        assert!(rendered.contains("Yeachan-Heo/clawhip"));
        assert!(rendered.contains("v0.6.0"));
        assert!(rendered.contains("clawhip 0.6.0"));
    }

    #[test]
    fn renders_release_prerelease_compact_with_flag() {
        let event = IncomingEvent::github_release(
            "prereleased",
            "Yeachan-Heo/clawhip".into(),
            "v0.6.0-rc.1".into(),
            "v0.6.0-rc.1".into(),
            true,
            "https://github.com/Yeachan-Heo/clawhip/releases/tag/v0.6.0-rc.1".into(),
            None,
            None,
        );

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Compact)
            .unwrap();
        assert!(rendered.contains("prereleased"));
        assert!(rendered.contains("(prerelease)"));
    }

    #[test]
    fn renders_release_inline_format() {
        let event = IncomingEvent::github_release(
            "published",
            "Yeachan-Heo/clawhip".into(),
            "v0.6.0".into(),
            "clawhip 0.6.0".into(),
            false,
            "https://github.com/Yeachan-Heo/clawhip/releases/tag/v0.6.0".into(),
            None,
            None,
        );

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Inline)
            .unwrap();
        assert_eq!(rendered, "[release] Yeachan-Heo/clawhip v0.6.0");
    }

    #[test]
    fn renders_release_alert_format() {
        let event = IncomingEvent::github_release(
            "published",
            "Yeachan-Heo/clawhip".into(),
            "v0.6.0".into(),
            "clawhip 0.6.0".into(),
            false,
            "https://github.com/Yeachan-Heo/clawhip/releases/tag/v0.6.0".into(),
            None,
            None,
        );

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Alert)
            .unwrap();
        assert!(rendered.starts_with("🚨"));
        assert!(rendered.contains("release published"));
    }
}
