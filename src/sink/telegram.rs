use std::sync::Arc;

use async_trait::async_trait;

use crate::Result;
use crate::config::AppConfig;
use crate::telegram::TelegramClient;

use super::{Sink, SinkMessage, SinkTarget};

#[derive(Clone)]
pub struct TelegramSink {
    client: TelegramClient,
}

impl TelegramSink {
    pub fn from_config(config: Arc<AppConfig>) -> Result<Self> {
        Ok(Self {
            client: TelegramClient::from_config(config)?,
        })
    }
}

#[async_trait]
impl Sink for TelegramSink {
    async fn send(&self, target: &SinkTarget, message: &SinkMessage) -> Result<()> {
        self.client.send(target, message).await
    }
}
