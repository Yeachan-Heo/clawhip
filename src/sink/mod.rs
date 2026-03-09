pub mod discord;
pub mod slack;

use async_trait::async_trait;

use crate::Result;
use crate::events::MessageFormat;

pub use discord::DiscordSink;
pub use slack::SlackSink;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SinkTarget {
    DiscordChannel(String),
    DiscordWebhook(String),
    SlackWebhook(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkMessage {
    pub event_kind: String,
    pub format: MessageFormat,
    pub content: String,
}

#[async_trait]
pub trait Sink: Send + Sync {
    async fn send(&self, target: &SinkTarget, message: &SinkMessage) -> Result<()>;
}
