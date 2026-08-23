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

            ("github.actions-status", MessageFormat::Compact) => format!(
                "GitHub {}: {} -> {} ({})",
                string_field(payload, "component")?,
                string_field(payload, "old_status")?,
                string_field(payload, "new_status")?,
                string_field(payload, "url")?
            ),
            ("github.actions-status", MessageFormat::Alert) => format!(
                "🚨 GitHub platform {}: {} -> {} ({})",
                string_field(payload, "component")?,
                string_field(payload, "old_status")?,
                string_field(payload, "new_status")?,
                string_field(payload, "url")?
            ),
            ("github.actions-status", MessageFormat::Inline) => format!(
                "[status:{}] {} -> {}",
                string_field(payload, "component")?,
                string_field(payload, "old_status")?,
                string_field(payload, "new_status")?
            ),
            ("github.actions-status", MessageFormat::Raw) => serde_json::to_string_pretty(payload)?,

            ("github.actions-incident", MessageFormat::Compact) => {
                render_github_actions_incident(payload, false)?
            }
            ("github.actions-incident", MessageFormat::Alert) => {
                format!("🚨 {}", render_github_actions_incident(payload, true)?)
            }
            ("github.actions-incident", MessageFormat::Inline) => format!(
                "[incident:{}] {} ({})",
                string_field(payload, "change")?,
                string_field(payload, "name")?,
                string_field(payload, "status")?
            ),
            ("github.actions-incident", MessageFormat::Raw) => {
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

            ("gajae.release.hold" | "gajae.merge.hold", MessageFormat::Compact) => {
                render_gajae_hold(payload, event.canonical_kind())?
            }
            ("gajae.release.hold" | "gajae.merge.hold", MessageFormat::Alert) => {
                format!("🚨 {}", render_gajae_hold(payload, event.canonical_kind())?)
            }
            ("gajae.release.hold" | "gajae.merge.hold", MessageFormat::Inline) => {
                let repo = string_field(payload, "repo")?;
                let action = string_field(payload, "action")?;
                let relevant = optional_string_field(payload, "version")
                    .or_else(|| optional_string_field(payload, "sha"))
                    .unwrap_or_default();
                format!("[gajae hold] {repo} {action} {relevant}")
            }
            ("gajae.release.hold" | "gajae.merge.hold", MessageFormat::Raw) => {
                serde_json::to_string_pretty(payload)?
            }

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
                "tmux:{} matched '{}' => {}{}",
                string_field(payload, "session")?,
                string_field(payload, "keyword")?,
                string_field(payload, "line")?,
                tmux_keyword_provenance_suffix(payload)
            ),
            ("tmux.keyword", MessageFormat::Alert) => format!(
                "🚨 tmux session {} hit keyword '{}': {}{}",
                string_field(payload, "session")?,
                string_field(payload, "keyword")?,
                string_field(payload, "line")?,
                tmux_keyword_provenance_suffix(payload)
            ),
            ("tmux.keyword", MessageFormat::Inline) => format!(
                "[tmux:{}] {}{}",
                string_field(payload, "session")?,
                string_field(payload, "line")?,
                tmux_keyword_provenance_suffix(payload)
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

            ("workflow.question", MessageFormat::Compact) => format!(
                "❓ GJC question {} · turn {} · rev {}: {}",
                optional_string_field(payload, "question_id")
                    .unwrap_or_else(|| "unknown".to_string()),
                optional_string_field(payload, "turn_id").unwrap_or_else(|| "unknown".to_string()),
                payload
                    .get("gate_revision")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                optional_string_field(payload, "summary")
                    .unwrap_or_else(|| "operator input requested".to_string())
            ),
            ("workflow.question", MessageFormat::Alert) => format!(
                "🚨 ❓ GJC question {} needs an answer · turn {} · rev {}: {}",
                optional_string_field(payload, "question_id")
                    .unwrap_or_else(|| "unknown".to_string()),
                optional_string_field(payload, "turn_id").unwrap_or_else(|| "unknown".to_string()),
                payload
                    .get("gate_revision")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                optional_string_field(payload, "summary")
                    .unwrap_or_else(|| "operator input requested".to_string())
            ),
            ("workflow.question", MessageFormat::Inline) => format!(
                "[gjc question:{}] {}",
                optional_string_field(payload, "question_id")
                    .unwrap_or_else(|| "unknown".to_string()),
                optional_string_field(payload, "summary")
                    .unwrap_or_else(|| "operator input requested".to_string())
            ),
            ("workflow.gate", MessageFormat::Compact | MessageFormat::Inline) => format!(
                "🚧 GJC gate {} blocked · turn {} · rev {}: {}",
                optional_string_field(payload, "question_id")
                    .unwrap_or_else(|| "unknown".to_string()),
                optional_string_field(payload, "turn_id").unwrap_or_else(|| "unknown".to_string()),
                payload
                    .get("gate_revision")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                optional_string_field(payload, "summary")
                    .unwrap_or_else(|| "workflow gate requires approval".to_string())
            ),
            ("workflow.gate", MessageFormat::Alert) => format!(
                "🚨 🚧 GJC gate {} blocked · turn {} · rev {}: {}",
                optional_string_field(payload, "question_id")
                    .unwrap_or_else(|| "unknown".to_string()),
                optional_string_field(payload, "turn_id").unwrap_or_else(|| "unknown".to_string()),
                payload
                    .get("gate_revision")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                optional_string_field(payload, "summary")
                    .unwrap_or_else(|| "workflow gate requires approval".to_string())
            ),

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

    if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(" · "))
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

/// Maximum Discord message content length in Unicode scalar values.
///
/// Discord rejects any `content` field exceeding 2,000 Unicode scalars with
/// HTTP 400 / `BASE_TYPE_MAX_LENGTH` (code 50035). Batched CI notifications can
/// exceed this when the job list is long, so the renderer must bound its output.
/// The final composed-content cap (including mention/alert prefixes) is enforced
/// in `Router::render_delivery`, not here — the renderer bounds only its own body.
const DISCORD_MAX_CONTENT_SCALARS: usize = 2_000;

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

    let header = match kind {
        "github.ci-passed" => format!(
            "✅ CI passed · {} · {passed}/{total} passed",
            github_ci_target(payload)?
        ),
        "github.ci-failed" => format!("❌ CI failed · {}", github_ci_target(payload)?),
        "github.ci-cancelled" => format!("🟡 CI cancelled · {}", github_ci_target(payload)?),
        _ => format!("⏳ CI running · {}", github_ci_target(payload)?),
    };

    // Expandable parts — workflow names (borrowed, no per-job allocation) and
    // failed-job labels (owned, composed from workflow:conclusion).
    let workflow_names: Vec<&str> = jobs
        .iter()
        .filter_map(|job| job.get("workflow").and_then(Value::as_str))
        .collect();

    let failed_job_labels: Vec<String> = if kind == "github.ci-failed" {
        jobs.iter()
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
            .collect()
    } else {
        Vec::new()
    };

    // Essential tail parts — counts and link, always included.
    let mut tail: Vec<String> = Vec::new();
    if kind != "github.ci-failed" {
        if skipped > 0 {
            tail.push(format!("{skipped} skipped"));
        }
        if cancelled > 0 {
            tail.push(format!("{cancelled} cancelled"));
        }
        if failed > 0 {
            tail.push(format!("{failed} failed"));
        }
    }
    if include_url {
        tail.push(string_field(payload, "url")?);
    }

    // First pass: build unbounded message using borrowed slices — no per-job
    // allocation on the fast path. Return immediately if within budget.
    let mut parts: Vec<String> = vec![header.clone()];
    if !workflow_names.is_empty() {
        parts.push(workflow_names.join(", "));
    }
    if !failed_job_labels.is_empty() {
        parts.push(failed_job_labels.join(", "));
    }
    parts.extend(tail.iter().cloned());
    let unbounded = parts.join(" · ");
    if unbounded.chars().count() <= DISCORD_MAX_CONTENT_SCALARS {
        return Ok(unbounded);
    }

    // Second pass: truncate expandable parts to fit the effective budget.
    // Count how many expandable slots we have for budget distribution.
    let num_expandable =
        (!workflow_names.is_empty() as usize) + (!failed_job_labels.is_empty() as usize);
    let sep_len = " · ".chars().count();
    let essential = {
        let mut e = vec![header.clone()];
        e.extend(tail.iter().cloned());
        e.join(" · ")
    };
    let essential_len = essential.chars().count();
    let overhead = num_expandable * sep_len;
    let per_list = DISCORD_MAX_CONTENT_SCALARS
        .saturating_sub(essential_len)
        .saturating_sub(overhead)
        / num_expandable.max(1);

    parts = vec![header];
    if !workflow_names.is_empty() {
        parts.push(truncate_joined_list(&workflow_names, ", ", per_list));
    }
    if !failed_job_labels.is_empty() {
        parts.push(truncate_joined_list(&failed_job_labels, ", ", per_list));
    }
    parts.extend(tail.iter().cloned());

    // Final deterministic hard cap: if essential fields (oversized repo/url)
    // consumed the entire budget, enforce the limit by truncating from the end.
    let result = parts.join(" · ");
    if result.chars().count() <= DISCORD_MAX_CONTENT_SCALARS {
        Ok(result)
    } else {
        let ellipsis = "…";
        let take = DISCORD_MAX_CONTENT_SCALARS.saturating_sub(ellipsis.chars().count());
        let truncated: String = result.chars().take(take).collect();
        Ok(format!("{truncated}{ellipsis}"))
    }
}

/// Join `items` with `separator`, truncating from the end with a count indicator
/// if the result would exceed `budget` Unicode scalar values.
///
/// Uses cumulative scalar lengths for O(n) behavior: each item's scalar count
/// and the separator width are computed once, then a single forward pass finds
/// the split point without repeatedly re-joining prefixes.
fn truncate_joined_list<S: AsRef<str>>(items: &[S], separator: &str, budget: usize) -> String {
    if items.is_empty() || budget == 0 {
        return String::new();
    }
    let refs: Vec<&str> = items.iter().map(|s| s.as_ref()).collect();

    // Pre-compute cumulative scalar count for the joined prefix ending at each
    // item index (inclusive). cumulative[i] = total scalars if we join items[0..=i].
    let sep_len = separator.chars().count();
    let item_lens: Vec<usize> = refs.iter().map(|s| s.chars().count()).collect();
    let mut cumulative = Vec::with_capacity(refs.len());
    let mut total = 0usize;
    for (i, &len) in item_lens.iter().enumerate() {
        if i > 0 {
            total += sep_len;
        }
        total += len;
        cumulative.push(total);
    }

    // If the full join fits, return it directly.
    if *cumulative.last().unwrap() <= budget {
        return refs.join(separator);
    }

    // Find the largest `kept` such that cumulative[kept-1] + marker_len <= budget.
    // The marker is "{separator}… +{omitted}" where omitted = len - kept.
    // Since marker_len changes with kept, do a single reverse pass — but each
    // iteration only counts the marker string, no re-joining.
    for kept in (1..refs.len()).rev() {
        let omitted = refs.len() - kept;
        let marker_len = sep_len + 1 + 2 + omitted.to_string().chars().count(); // "… +N"
        if cumulative[kept - 1] + marker_len <= budget {
            let candidate = refs[..kept].join(separator);
            let marker = format!("{separator}… +{omitted}");
            return format!("{candidate}{marker}");
        }
    }

    // Even a single item + marker exceeds budget: hard-truncate the first item.
    let marker = format!("… +{}", refs.len());
    let marker_len = marker.chars().count();
    let content_budget = budget.saturating_sub(marker_len);
    let truncated: String = refs[0].chars().take(content_budget).collect();
    format!("{truncated}{marker}")
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

fn render_github_actions_incident(payload: &Value, include_body: bool) -> Result<String> {
    let name = string_field(payload, "name")?;
    let change = string_field(payload, "change")?;
    let status = string_field(payload, "status")?;
    let impact = string_field(payload, "impact")?;
    let url = optional_string_field(payload, "url").unwrap_or_default();
    let mut parts = vec![format!(
        "GitHub incident {change}: {name} ({status}, impact={impact})"
    )];
    if include_body && let Some(body) = optional_string_field(payload, "update_body") {
        let trimmed = if body.chars().count() > 180 {
            let clipped: String = body.chars().take(177).collect();
            format!("{clipped}...")
        } else {
            body
        };
        parts.push(trimmed);
    }
    if !url.is_empty() {
        parts.push(url);
    }
    Ok(parts.join(" · "))
}

fn render_gajae_hold(payload: &Value, kind: &str) -> Result<String> {
    let repo = string_field(payload, "repo")?;
    let action = string_field(payload, "action")?;
    let disallowed_action = string_field(payload, "disallowed_action")?;
    let why = string_field(payload, "why_autonomous_disallowed")?;
    let boundary = match kind {
        "gajae.release.hold" => "release boundary hold",
        "gajae.merge.hold" => "main-merge boundary hold",
        _ => "GAJAE boundary hold",
    };
    let relevant = optional_string_field(payload, "version")
        .or_else(|| optional_string_field(payload, "sha"))
        .unwrap_or_default();
    let relevant_part = if relevant.is_empty() {
        String::new()
    } else {
        format!(" · {relevant}")
    };

    Ok(format!(
        "{boundary} · {repo} · {action}{relevant_part} · blocked action: {disallowed_action} · autonomous execution disallowed: {why}"
    ))
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

fn tmux_keyword_provenance_suffix(payload: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(pane_id) = payload.get("pane_id").and_then(Value::as_str) {
        let pane_name = payload.get("pane_name").and_then(Value::as_str);
        match pane_name {
            Some(pane_name) if !pane_name.is_empty() => {
                parts.push(format!("pane {pane_id}/{pane_name}"));
            }
            _ => parts.push(format!("pane {pane_id}")),
        }
    }
    if let Some(cursor) = payload.get("cursor").and_then(Value::as_u64) {
        parts.push(format!("cursor {cursor}"));
    }
    if let Some(source) = payload.get("source").and_then(Value::as_str) {
        parts.push(source.to_string());
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
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
                Some(format!(
                    "'{keyword}': {line}{}",
                    tmux_keyword_provenance_suffix(hit)
                ))
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
    fn renders_tmux_keyword_provenance_when_present() {
        let mut event = IncomingEvent::tmux_keyword(
            "issue-220".into(),
            "ERROR_READY".into(),
            "ERROR_READY".into(),
            None,
        );
        event.payload["pane_id"] = json!("%3");
        event.payload["pane_name"] = json!("0.1");
        event.payload["cursor"] = json!(42);
        event.payload["source"] = json!("fresh-output");

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Alert)
            .unwrap();

        assert_eq!(
            rendered,
            "🚨 tmux session issue-220 hit keyword 'ERROR_READY': ERROR_READY (pane %3/0.1, cursor 42, fresh-output)"
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

    #[test]
    fn renders_gajae_hold_with_blocked_action_and_reason() {
        let event = IncomingEvent::gajae_merge_hold(
            "Yeachan-Heo/clawhip".into(),
            "owner-maintainer".into(),
            "merge-to-main".into(),
            "0123456789abcdef".into(),
            "merge pull request #252 into main".into(),
            "main branch merge boundaries require owner/maintainer approval".into(),
            Some("maintainer".into()),
        );

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Compact)
            .unwrap();

        assert!(rendered.contains("main-merge boundary hold"));
        assert!(rendered.contains("blocked action: merge pull request #252 into main"));
        assert!(rendered.contains("autonomous execution disallowed"));
        assert!(rendered.contains("owner/maintainer approval"));
    }

    #[test]
    fn renders_github_actions_status_and_incident() {
        let status = IncomingEvent::github_actions_status(
            "Actions".into(),
            "operational".into(),
            "degraded_performance".into(),
            "https://www.githubstatus.com".into(),
            Some("dev".into()),
        );
        let rendered = DefaultRenderer
            .render(&status, &MessageFormat::Alert)
            .unwrap();
        assert!(rendered.starts_with("🚨"));
        assert!(rendered.contains("Actions"));
        assert!(rendered.contains("degraded_performance"));

        let incident = IncomingEvent::github_actions_incident(
            "inc-1".into(),
            "Incident with Actions".into(),
            "monitoring".into(),
            "critical".into(),
            "updated".into(),
            Some("investigating".into()),
            Some("upd-2".into()),
            Some("monitoring".into()),
            Some("Mitigated; monitoring".into()),
            vec!["Actions".into()],
            "https://stspg.io/example".into(),
            Some("dev".into()),
        );
        let rendered = DefaultRenderer
            .render(&incident, &MessageFormat::Compact)
            .unwrap();
        assert!(rendered.contains("Incident with Actions"));
        assert!(rendered.contains("updated"));
        assert!(rendered.contains("impact=critical"));
    }
    #[test]
    fn batched_ci_oversized_compact_truncates_to_discord_limit() {
        // Reproduce the Issue #313 scenario: a batched CI notification whose
        // workflow-name list and job list push content past Discord's 2,000
        // Unicode scalar limit. The renderer must bound the output while
        // preserving repo/PR/status/count/link context.
        // Renderer bounds to DISCORD_MAX_CONTENT_SCALARS; the composed cap
        // (mention + body) is enforced by Router::render_delivery.
        let long_workflow = format!("CI / very-long-workflow-name-{:04}", 0);
        let mut jobs = Vec::new();
        for i in 0..80 {
            let wf = if i == 0 {
                long_workflow.clone()
            } else {
                format!("CI / very-long-workflow-name-{i:04}")
            };
            jobs.push(json!({
                "workflow": wf,
                "status": "in_progress",
                "conclusion": null,
                "url": "https://github.com/Yeachan-Heo/clawhip/actions/runs/1",
            }));
        }

        let event = IncomingEvent {
            kind: "github.ci-started".into(),
            channel: None,
            mention: None,
            format: Some(MessageFormat::Compact),
            template: None,
            payload: json!({
                "repo": "Yeachan-Heo/clawhip",
                "number": 58,
                "branch": "main",
                "sha": "abcdef1234567890",
                "url": "https://github.com/Yeachan-Heo/clawhip/actions/runs/1",
                "batched": true,
                "total_count": 80,
                "passed_count": 0,
                "skipped_count": 0,
                "failed_count": 0,
                "cancelled_count": 0,
                "jobs": jobs,
            }),
        };

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Compact)
            .unwrap();

        let len = rendered.chars().count();
        assert!(
            len <= DISCORD_MAX_CONTENT_SCALARS,
            "rendered batched CI content is {len} scalars, exceeds {} limit",
            DISCORD_MAX_CONTENT_SCALARS
        );

        // Essential context must remain visible after bounding.
        assert!(
            rendered.contains("Yeachan-Heo/clawhip#58"),
            "repo/PR context missing from bounded output"
        );
        assert!(
            rendered.contains("CI running"),
            "status missing from bounded output"
        );
        assert!(
            rendered.contains("https://github.com/Yeachan-Heo/clawhip/actions/runs/1"),
            "link missing from bounded output"
        );
        // Truncation indicator should be present since the unbounded output
        // would exceed the effective budget.
        assert!(
            rendered.contains("… +"),
            "truncation indicator missing from bounded output: {rendered}"
        );
    }

    #[test]
    fn batched_ci_oversized_failed_compact_preserves_failed_detail_and_limit() {
        // Failed batch with long failed-job detail list — the failed-job labels
        // are expandable and must be truncated while keeping repo/status/link.
        // Renderer bounds to DISCORD_MAX_CONTENT_SCALARS.
        let mut jobs = Vec::new();
        for i in 0..60 {
            jobs.push(json!({
                "workflow": format!("CI / build-matrix-job-{i:04}-extremely-long-workflow"),
                "status": "completed",
                "conclusion": "failure",
                "url": "https://github.com/Yeachan-Heo/clawhip/actions/runs/2",
            }));
        }

        let event = IncomingEvent {
            kind: "github.ci-failed".into(),
            channel: None,
            mention: None,
            format: Some(MessageFormat::Compact),
            template: None,
            payload: json!({
                "repo": "Yeachan-Heo/clawhip",
                "number": 59,
                "branch": "fix/issue-313",
                "sha": "abcdef1234567890",
                "url": "https://github.com/Yeachan-Heo/clawhip/actions/runs/2",
                "batched": true,
                "total_count": 60,
                "passed_count": 0,
                "skipped_count": 0,
                "failed_count": 60,
                "cancelled_count": 0,
                "jobs": jobs,
            }),
        };

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Compact)
            .unwrap();

        let len = rendered.chars().count();
        assert!(
            len <= DISCORD_MAX_CONTENT_SCALARS,
            "rendered batched CI failed content is {len} scalars, exceeds {} limit",
            DISCORD_MAX_CONTENT_SCALARS
        );

        assert!(rendered.contains("CI failed"));
        assert!(rendered.contains("Yeachan-Heo/clawhip#59"));
        assert!(
            rendered.contains("https://github.com/Yeachan-Heo/clawhip/actions/runs/2"),
            "link missing from bounded failed output"
        );
        assert!(
            rendered.contains("… +"),
            "truncation indicator missing from bounded failed output"
        );
    }

    #[test]
    fn batched_ci_oversized_alert_format_respects_composed_limit() {
        // P1: Alert format adds "🚨 " prefix AFTER the renderer bounds the body.
        // The composed output (prefix + body) must still respect the 2,000 limit.
        let mut jobs = Vec::new();
        for i in 0..80 {
            jobs.push(json!({
                "workflow": format!("CI / workflow-{i:04}-with-a-long-descriptive-name"),
                "status": "in_progress",
                "conclusion": null,
                "url": "https://github.com/Yeachan-Heo/clawhip/actions/runs/4",
            }));
        }

        let event = IncomingEvent {
            kind: "github.ci-started".into(),
            channel: None,
            mention: None,
            format: Some(MessageFormat::Alert),
            template: None,
            payload: json!({
                "repo": "Yeachan-Heo/clawhip",
                "number": 61,
                "branch": "main",
                "sha": "abcdef1234567890",
                "url": "https://github.com/Yeachan-Heo/clawhip/actions/runs/4",
                "batched": true,
                "total_count": 80,
                "passed_count": 0,
                "skipped_count": 0,
                "failed_count": 0,
                "cancelled_count": 0,
                "jobs": jobs,
            }),
        };

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Alert)
            .unwrap();

        let len = rendered.chars().count();
        assert!(
            len <= DISCORD_MAX_CONTENT_SCALARS,
            "composed Alert output is {len} scalars, exceeds {} limit",
            DISCORD_MAX_CONTENT_SCALARS
        );
        assert!(rendered.starts_with("🚨 "));
        assert!(rendered.contains("CI running"));
    }

    #[test]
    fn batched_ci_oversized_essential_fields_enforce_hard_cap() {
        // P2: When repo/URL are themselves extremely long, essential_len can
        // consume the entire budget. The deterministic final hard cap must
        // still bound the output.
        let huge_repo = "x".repeat(1_000);
        let huge_url = format!(
            "https://github.com/Yeachan-Heo/clawhip/actions/runs/{}",
            "y".repeat(1_000)
        );

        let jobs = vec![
            json!({
                "workflow": "CI / test",
                "status": "in_progress",
                "conclusion": null,
                "url": &huge_url,
            }),
            json!({
                "workflow": "CI / lint",
                "status": "in_progress",
                "conclusion": null,
                "url": &huge_url,
            }),
        ];

        let event = IncomingEvent {
            kind: "github.ci-started".into(),
            channel: None,
            mention: None,
            format: Some(MessageFormat::Compact),
            template: None,
            payload: json!({
                "repo": &huge_repo,
                "number": 999,
                "branch": "main",
                "sha": "abcdef1234567890",
                "url": &huge_url,
                "batched": true,
                "total_count": 2,
                "passed_count": 0,
                "skipped_count": 0,
                "failed_count": 0,
                "cancelled_count": 0,
                "jobs": jobs,
            }),
        };

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Compact)
            .unwrap();

        let len = rendered.chars().count();
        assert!(
            len <= DISCORD_MAX_CONTENT_SCALARS,
            "oversized essential fields output is {len} scalars, exceeds {} limit",
            DISCORD_MAX_CONTENT_SCALARS
        );
        // The hard cap should have kicked in with an ellipsis.
        assert!(
            rendered.ends_with('…'),
            "hard cap ellipsis missing from oversized essential fields output"
        );
    }

    #[test]
    fn batched_ci_small_batch_is_not_truncated() {
        // Small batches should pass through unchanged.
        let event = IncomingEvent {
            kind: "github.ci-passed".into(),
            channel: None,
            mention: None,
            format: Some(MessageFormat::Compact),
            template: None,
            payload: json!({
                "repo": "clawhip",
                "number": 1,
                "branch": "main",
                "sha": "abcdef1",
                "url": "https://github.com/Yeachan-Heo/clawhip/actions/runs/3",
                "batched": true,
                "total_count": 2,
                "passed_count": 2,
                "skipped_count": 0,
                "failed_count": 0,
                "cancelled_count": 0,
                "jobs": [
                    {"workflow": "CI / test", "status": "completed", "conclusion": "success", "url": "https://github.com/Yeachan-Heo/clawhip/actions/runs/3"},
                    {"workflow": "CI / lint", "status": "completed", "conclusion": "success", "url": "https://github.com/Yeachan-Heo/clawhip/actions/runs/3"},
                ],
            }),
        };

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Compact)
            .unwrap();

        assert!(
            !rendered.contains("… +"),
            "small batch should not be truncated: {rendered}"
        );
        assert!(rendered.contains("CI passed"));
        assert!(rendered.contains("clawhip#1"));
        assert!(rendered.contains("CI / test, CI / lint"));
    }

    #[test]
    fn batched_ci_large_job_list_truncates_efficiently() {
        // Guard against O(n²) regression in truncate_joined_list: a large job
        // list (500+ entries) must truncate without pathological cost.
        let mut jobs = Vec::new();
        for i in 0..500 {
            jobs.push(json!({
                "workflow": format!("CI / matrix-job-{i:04}"),
                "status": "in_progress",
                "conclusion": null,
                "url": "https://github.com/Yeachan-Heo/clawhip/actions/runs/5",
            }));
        }

        let event = IncomingEvent {
            kind: "github.ci-started".into(),
            channel: None,
            mention: None,
            format: Some(MessageFormat::Compact),
            template: None,
            payload: json!({
                "repo": "Yeachan-Heo/clawhip",
                "number": 80,
                "branch": "main",
                "sha": "abcdef1234567890",
                "url": "https://github.com/Yeachan-Heo/clawhip/actions/runs/5",
                "batched": true,
                "total_count": 500,
                "passed_count": 0,
                "skipped_count": 0,
                "failed_count": 0,
                "cancelled_count": 0,
                "jobs": jobs,
            }),
        };

        let rendered = DefaultRenderer
            .render(&event, &MessageFormat::Compact)
            .unwrap();

        assert!(
            rendered.chars().count() <= DISCORD_MAX_CONTENT_SCALARS,
            "large job list exceeded {} scalars",
            DISCORD_MAX_CONTENT_SCALARS
        );
        assert!(rendered.contains("CI running"));
        assert!(rendered.contains("Yeachan-Heo/clawhip#80"));
        assert!(
            rendered.contains("… +"),
            "large job list should show truncation indicator"
        );
    }
}
