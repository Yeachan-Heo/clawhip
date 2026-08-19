use tokio::sync::mpsc;

use crate::Result;
use crate::events::IncomingEvent;

pub mod git;
pub mod github;
pub mod github_status;
pub mod subscription;
pub mod tmux;
pub mod workspace;

pub use git::{
    GitMonitorLifecycleCounts, GitSource, SharedGitMonitorDiagnostics,
    new_shared_git_monitor_diagnostics, snapshot_git_monitor_diagnostics,
};
pub use github::{GitHubSource, default_github_ci_baseline_path};
pub use github_status::GitHubStatusSource;
pub use subscription::{SubscriptionSnapshot, SubscriptionState, SubscriptionWorker};
pub use tmux::{
    RegisteredTmuxSession, SharedTmuxRegistry, TmuxSource, default_registry_state_path,
    inspect_tmux_registry_state, list_active_tmux_registrations, load_tmux_registry_state,
    register_runtime_tmux_registration, tmux_registry_diagnostics,
};
pub use workspace::WorkspaceSource;

#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &str;

    async fn run(&self, tx: mpsc::Sender<IncomingEvent>) -> Result<()>;
}
