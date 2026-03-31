pub mod body;
pub mod compat;

pub use body::{
    AgentEvent, CustomEvent, GitBranchChangedEvent, GitCommitAggregatedEvent, GitCommitEvent,
    GitHubCIEvent, GitHubIssueEvent, GitHubPREvent, GitHubPRStatusEvent,
    TmuxKeywordAggregatedEvent, TmuxKeywordEvent, TmuxStaleEvent, WorkspaceEvent,
};

use time::OffsetDateTime;
use uuid::Uuid;

use crate::events::MessageFormat;

#[derive(Debug, Clone, PartialEq)]
pub struct EventEnvelope {
    pub id: Uuid,
    pub timestamp: OffsetDateTime,
    pub source: String,
    pub body: EventBody,
    pub metadata: EventMetadata,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EventBody {
    GitCommit(GitCommitEvent),
    GitCommitAggregated(GitCommitAggregatedEvent),
    GitBranchChanged(GitBranchChangedEvent),
    GitHubIssueOpened(GitHubIssueEvent),
    GitHubIssueCommented(GitHubIssueEvent),
    GitHubIssueClosed(GitHubIssueEvent),
    GitHubPROpened(GitHubPREvent),
    GitHubPRMerged(GitHubPREvent),
    GitHubPRStatusChanged(GitHubPRStatusEvent),
    GitHubCIFailed(GitHubCIEvent),
    TmuxKeyword(TmuxKeywordEvent),
    TmuxKeywordAggregated(TmuxKeywordAggregatedEvent),
    TmuxStale(TmuxStaleEvent),
    AgentStarted(AgentEvent),
    AgentBlocked(AgentEvent),
    AgentFinished(AgentEvent),
    AgentFailed(AgentEvent),
    AgentRetryNeeded(AgentEvent),
    AgentPRCreated(AgentEvent),
    AgentTestStarted(AgentEvent),
    AgentTestFinished(AgentEvent),
    AgentTestFailed(AgentEvent),
    AgentHandoffNeeded(AgentEvent),
    WorkspaceSessionStarted(WorkspaceEvent),
    WorkspaceTurnComplete(WorkspaceEvent),
    WorkspaceSkillActivated(WorkspaceEvent),
    WorkspaceSessionBlocked(WorkspaceEvent),
    WorkspaceMetricsUpdate(WorkspaceEvent),
    Custom(CustomEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventMetadata {
    pub channel_hint: Option<String>,
    pub mention: Option<String>,
    pub format: Option<MessageFormat>,
    pub template: Option<String>,
    pub priority: EventPriority,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventPriority {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}
