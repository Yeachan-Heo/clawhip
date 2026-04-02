use tokio::sync::mpsc;

use crate::Result;
use crate::events::IncomingEvent;

pub mod git;
pub mod github;
pub mod tmux;
pub mod workspace;

pub use git::GitSource;
pub use github::GitHubSource;
pub use tmux::{
    RegisteredTmuxSession, SharedTmuxRegistry, TmuxSource, list_active_tmux_registrations,
};
pub use workspace::WorkspaceSource;

#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &str;

    async fn run(&self, tx: mpsc::Sender<IncomingEvent>) -> Result<()>;
}
