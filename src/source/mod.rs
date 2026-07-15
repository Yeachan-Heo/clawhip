use tokio::sync::mpsc;

use crate::Result;
use crate::events::IncomingEvent;

pub mod git;
pub mod github;
pub mod subscription;
pub mod tmux;
pub mod workspace;

pub use git::GitSource;
pub use github::GitHubSource;
pub use subscription::{SubscriptionSnapshot, SubscriptionState, SubscriptionWorker};
pub use tmux::{
    RegisteredTmuxSession, SharedTmuxRegistry, TmuxSource, default_registry_state_path,
    inspect_tmux_registry_state, list_active_tmux_registrations, load_tmux_registry_state,
    register_runtime_tmux_registration, remove_tmux_registrations, tmux_registry_diagnostics,
};
pub use workspace::WorkspaceSource;

#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &str;

    async fn run(&self, tx: mpsc::Sender<IncomingEvent>) -> Result<()>;
}
