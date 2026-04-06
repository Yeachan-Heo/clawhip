pub mod discord;
pub mod slack;
pub mod telegram;

use async_trait::async_trait;

use crate::Result;
use crate::events::MessageFormat;
use serde_json::Value;

pub use discord::DiscordSink;
pub use slack::SlackSink;
pub use telegram::TelegramSink;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SinkTarget {
    DiscordChannel(String),
    DiscordWebhook(String),
    SlackWebhook(String),
    TelegramChat(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkMessage {
    pub event_kind: String,
    pub format: MessageFormat,
    pub content: String,
    pub payload: Value,
}

#[async_trait]
pub trait Sink: Send + Sync {
    async fn send(&self, target: &SinkTarget, message: &SinkMessage) -> Result<()>;
}
