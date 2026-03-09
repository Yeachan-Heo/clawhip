use std::sync::Arc;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::json;

use crate::Result;
use crate::config::AppConfig;
use crate::sink::{SinkMessage, SinkTarget};

#[derive(Clone)]
pub struct DiscordClient {
    bot_client: Option<reqwest::Client>,
    webhook_client: reqwest::Client,
    api_base: String,
}

impl DiscordClient {
    pub fn from_config(config: Arc<AppConfig>) -> Result<Self> {
        let bot_client = if let Some(token) = config.effective_token() {
            let mut headers = HeaderMap::new();
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bot {token}"))?,
            );
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

            Some(
                reqwest::Client::builder()
                    .default_headers(headers)
                    .build()?,
            )
        } else {
            None
        };
        let api_base = std::env::var("CLAWHIP_DISCORD_API_BASE")
            .unwrap_or_else(|_| "https://discord.com/api/v10".to_string());
        let webhook_client = reqwest::Client::new();

        Ok(Self {
            bot_client,
            webhook_client,
            api_base,
        })
    }

    pub async fn send(&self, target: &SinkTarget, message: &SinkMessage) -> Result<()> {
        match target {
            SinkTarget::DiscordChannel(channel_id) => {
                self.send_message(channel_id, &message.content).await
            }
            SinkTarget::DiscordWebhook(webhook_url) => {
                self.send_webhook(webhook_url, &message.content).await
            }
            SinkTarget::SlackWebhook(_) => {
                Err("cannot send Slack webhook via Discord client".into())
            }
        }
    }

    pub async fn send_message(&self, channel_id: &str, content: &str) -> Result<()> {
        let url = format!(
            "{}/channels/{}/messages",
            self.api_base.trim_end_matches('/'),
            channel_id
        );
        let client = self.bot_client.as_ref().ok_or_else(|| {
            "missing Discord bot token for channel delivery; configure [providers.discord].token (or legacy [discord].token) or use a route webhook"
                .to_string()
        })?;
        let response = self
            .client_for_channel(client)
            .post(url)
            .json(&json!({ "content": content }))
            .send()
            .await?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!("Discord API request failed with {status}: {body}").into())
    }

    pub async fn send_webhook(&self, webhook_url: &str, content: &str) -> Result<()> {
        let response = self
            .webhook_client
            .post(webhook_url_with_wait(webhook_url))
            .json(&json!({ "content": content }))
            .send()
            .await?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!("Discord webhook request failed with {status}: {body}").into())
    }

    fn client_for_channel<'a>(&'a self, client: &'a reqwest::Client) -> &'a reqwest::Client {
        client
    }
}

fn webhook_url_with_wait(webhook_url: &str) -> String {
    if webhook_url.contains("wait=") {
        webhook_url.to_string()
    } else if webhook_url.contains('?') {
        format!("{webhook_url}&wait=true")
    } else {
        format!("{webhook_url}?wait=true")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_urls_gain_wait_true_by_default() {
        assert_eq!(
            webhook_url_with_wait("https://discord.com/api/webhooks/1/abc"),
            "https://discord.com/api/webhooks/1/abc?wait=true"
        );
        assert_eq!(
            webhook_url_with_wait("https://discord.com/api/webhooks/1/abc?thread_id=7"),
            "https://discord.com/api/webhooks/1/abc?thread_id=7&wait=true"
        );
        assert_eq!(
            webhook_url_with_wait("https://discord.com/api/webhooks/1/abc?wait=false"),
            "https://discord.com/api/webhooks/1/abc?wait=false"
        );
    }
}
