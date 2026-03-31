use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommitEvent {
    pub repo: String,
    pub branch: String,
    pub sha: String,
    pub short_sha: String,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommitAggregatedEvent {
    pub repo: String,
    pub branch: String,
    pub commit_count: usize,
    pub commits: Vec<GitCommitEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitBranchChangedEvent {
    pub repo: String,
    pub old_branch: String,
    pub new_branch: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubIssueEvent {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub comments: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubPREvent {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubPRStatusEvent {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub old_status: String,
    pub new_status: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubCIEvent {
    pub repo: String,
    pub number: Option<u64>,
    pub branch: Option<String>,
    pub sha: Option<String>,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub url: Option<String>,
    pub workflow: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxKeywordEvent {
    pub session: String,
    pub keyword: String,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxKeywordAggregatedEvent {
    pub session: String,
    pub hit_count: usize,
    pub hits: Vec<TmuxKeywordEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxStaleEvent {
    pub session: String,
    pub pane: String,
    pub minutes: u64,
    pub last_line: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEvent {
    pub agent_name: String,
    pub session_name: Option<String>,
    pub status: String,
    pub normalized_event: Option<String>,
    pub session_id: Option<String>,
    pub project: Option<String>,
    pub repo_path: Option<String>,
    pub branch: Option<String>,
    pub issue_number: Option<u64>,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
    pub command: Option<String>,
    pub tool_name: Option<String>,
    pub elapsed_secs: Option<u64>,
    pub summary: Option<String>,
    pub error_summary: Option<String>,
    pub error_message: Option<String>,
    pub mention: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEvent {
    pub source_tool: String,
    pub workspace_path: String,
    pub state_file: String,
    pub session_name: Option<String>,
    pub diff_fields: Vec<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CustomEvent {
    pub kind: String,
    pub message: String,
    pub payload: Option<Value>,
}
