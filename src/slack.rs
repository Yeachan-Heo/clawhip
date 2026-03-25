use serde_json::{Value, json};

use crate::Result;
use crate::events::MessageFormat;
use crate::sink::{SinkMessage, SinkTarget};

#[derive(Clone)]
pub struct SlackClient {
    webhook_client: reqwest::Client,
}

impl SlackClient {
    pub fn new() -> Self {
        Self {
            webhook_client: reqwest::Client::new(),
        }
    }

    pub async fn send(&self, target: &SinkTarget, message: &SinkMessage) -> Result<()> {
        match target {
            SinkTarget::SlackWebhook(webhook_url) => self.send_webhook(webhook_url, message).await,
            SinkTarget::DiscordChannel(_) | SinkTarget::DiscordWebhook(_) => {
                Err("cannot send Discord target via Slack client".into())
            }
        }
    }

    pub async fn send_webhook(&self, webhook_url: &str, message: &SinkMessage) -> Result<()> {
        let response = self
            .webhook_client
            .post(webhook_url)
            .json(&webhook_payload(message))
            .send()
            .await?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!("Slack webhook request failed with {status}: {body}").into())
    }
}

impl Default for SlackClient {
    fn default() -> Self {
        Self::new()
    }
}

fn webhook_payload(message: &SinkMessage) -> Value {
    let mut payload = json!({
        "text": message.content,
    });

    if matches!(
        message.format,
        MessageFormat::Compact | MessageFormat::Alert
    ) {
        payload["blocks"] = json!(slack_blocks(message));
    }

    payload
}

fn slack_blocks(message: &SinkMessage) -> Vec<Value> {
    let label = match message.format {
        MessageFormat::Alert => ":rotating_light: *Alert*",
        _ => ":speech_balloon: *Notification*",
    };

    vec![
        json!({
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": label,
            }
        }),
        json!({
            "type": "section",
            "text": {
                "type": "mrkdwn",
                "text": message.content,
            }
        }),
        json!({
            "type": "context",
            "elements": [
                {
                    "type": "mrkdwn",
                    "text": format!("event `{}` · format `{}`", message.event_kind, message.format.as_str()),
                }
            ]
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_payload_includes_block_kit_sections() {
        let payload = webhook_payload(&SinkMessage {
            event_kind: "tmux.keyword".into(),
            format: MessageFormat::Compact,
            content: "tmux:ops matched 'error' => boom".into(),
            payload: serde_json::json!({}),
        });

        assert_eq!(
            payload.get("text").and_then(Value::as_str),
            Some("tmux:ops matched 'error' => boom")
        );
        let blocks = payload
            .get("blocks")
            .and_then(Value::as_array)
            .expect("blocks");
        assert_eq!(blocks.len(), 3);
        assert_eq!(
            blocks[0]["text"]["text"].as_str(),
            Some(":speech_balloon: *Notification*")
        );
    }

    #[test]
    fn alert_payload_uses_alert_label() {
        let payload = webhook_payload(&SinkMessage {
            event_kind: "github.ci-failed".into(),
            format: MessageFormat::Alert,
            content: "🚨 deploy <failed> & paging".into(),
            payload: serde_json::json!({}),
        });

        let blocks = payload
            .get("blocks")
            .and_then(Value::as_array)
            .expect("blocks");
        assert_eq!(
            blocks[0]["text"]["text"].as_str(),
            Some(":rotating_light: *Alert*")
        );
        assert_eq!(
            blocks[1]["text"]["text"].as_str(),
            Some("🚨 deploy <failed> & paging")
        );
    }
}
