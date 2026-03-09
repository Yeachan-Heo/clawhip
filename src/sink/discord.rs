use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::config::AppConfig;
use crate::discord::DiscordClient;

use super::{Sink, SinkMessage, SinkTarget};

#[derive(Clone)]
pub struct DiscordSink {
    client: DiscordClient,
}

impl DiscordSink {
    pub fn new(client: DiscordClient) -> Self {
        Self { client }
    }

    pub fn from_config(config: Arc<AppConfig>) -> Result<Self> {
        Ok(Self::new(DiscordClient::from_config(config)?))
    }
}

#[async_trait]
impl Sink for DiscordSink {
    async fn send(&self, target: &SinkTarget, message: &SinkMessage) -> Result<()> {
        self.client.send(target, message).await
    }
}
