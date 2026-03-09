use std::sync::Arc;

use crate::Result;
use crate::config::{AppConfig, RouteRule};
use crate::discord::DiscordClient;
use crate::dynamic_tokens;
use crate::events::{IncomingEvent, MessageFormat};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryTarget {
    Channel(String),
    Webhook(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryPreview {
    pub target: DeliveryTarget,
    pub format: MessageFormat,
    pub content: String,
}

pub struct Router {
    config: Arc<AppConfig>,
}

impl Router {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }

    pub async fn dispatch(&self, event: &IncomingEvent, discord: &DiscordClient) -> Result<()> {
        let delivery = self.preview_delivery(event).await?;
        discord.send(&delivery.target, &delivery.content).await
    }

    pub async fn preview_delivery(&self, event: &IncomingEvent) -> Result<DeliveryPreview> {
        let route = self.route_for(event);
        let target = self.target_for(event, route)?;
        let format = event
            .format
            .clone()
            .or_else(|| route.and_then(|route| route.format.clone()))
            .unwrap_or_else(|| self.config.defaults.format.clone());
        let allow_dynamic_tokens = self.allow_dynamic_tokens_for(event, route);
        let content = if let Some(template) = event
            .template
            .as_deref()
            .or_else(|| route.and_then(|route| route.template.as_deref()))
        {
            dynamic_tokens::render_template(
                template,
                &event.template_context(),
                allow_dynamic_tokens,
            )
            .await
        } else {
            let rendered = event.render_default(&format)?;
            if allow_dynamic_tokens {
                dynamic_tokens::render_template(&rendered, &event.template_context(), true).await
            } else {
                rendered
            }
        };
        let mention = route
            .and_then(|route| route.mention.as_deref())
            .or(event.mention.as_deref());
        let content = match mention {
            Some(mention) if !mention.trim().is_empty() => {
                format!("{} {}", mention.trim(), content)
            }
            _ => content,
        };

        Ok(DeliveryPreview {
            target,
            format,
            content,
        })
    }

    #[cfg(test)]
    pub async fn preview(&self, event: &IncomingEvent) -> Result<(String, MessageFormat, String)> {
        let delivery = self.preview_delivery(event).await?;
        match delivery.target {
            DeliveryTarget::Channel(channel) => Ok((channel, delivery.format, delivery.content)),
            DeliveryTarget::Webhook(_) => {
                Err("matched route uses a Discord webhook instead of a channel".into())
            }
        }
    }

    fn allow_dynamic_tokens_for(&self, event: &IncomingEvent, route: Option<&RouteRule>) -> bool {
        if let Some(route) = route {
            return route.allow_dynamic_tokens;
        }

        if event.canonical_kind() == "custom"
            && let Some(channel) = event.channel.as_deref()
        {
            return self.config.routes.iter().any(|route| {
                route.allow_dynamic_tokens && route.channel.as_deref() == Some(channel)
            });
        }

        false
    }

    fn route_for<'a>(&'a self, event: &IncomingEvent) -> Option<&'a RouteRule> {
        let context = event.template_context();
        let candidates = route_candidates(event.canonical_kind());
        self.config.routes.iter().find(|route| {
            candidates
                .iter()
                .any(|candidate| glob_match(&route.event, candidate))
                && route.filter.iter().all(|(key, expected)| {
                    context
                        .get(key)
                        .map(|actual| glob_match(expected, actual))
                        .unwrap_or(false)
                })
        })
    }

    fn target_for(
        &self,
        event: &IncomingEvent,
        route: Option<&RouteRule>,
    ) -> Result<DeliveryTarget> {
        if let Some(webhook) = route
            .and_then(|route| route.webhook.as_deref())
            .map(str::trim)
            .filter(|webhook| !webhook.is_empty())
        {
            return Ok(DeliveryTarget::Webhook(webhook.to_string()));
        }

        let channel = event
            .channel
            .clone()
            .or_else(|| route.and_then(|route| route.channel.clone()))
            .or_else(|| self.config.defaults.channel.clone())
            .ok_or_else(|| format!("no channel configured for event {}", event.canonical_kind()))?;

        Ok(DeliveryTarget::Channel(channel))
    }
}

fn route_candidates(kind: &str) -> Vec<&str> {
    match kind {
        "git.commit" => vec!["git.commit", "github.commit"],
        "git.branch-changed" => vec!["git.branch-changed", "github.branch-changed"],
        "agent.started" | "agent.blocked" | "agent.finished" | "agent.failed" => {
            vec![kind, "agent.*"]
        }
        other => vec![other],
    }
}

fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern == value {
        return true;
    }
    if !pattern.contains('*') {
        return false;
    }

    let mut remainder = value;
    let parts: Vec<&str> = pattern.split('*').collect();
    let starts_with_wildcard = pattern.starts_with('*');
    let ends_with_wildcard = pattern.ends_with('*');

    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }

        if index == 0 && !starts_with_wildcard {
            if !remainder.starts_with(part) {
                return false;
            }
            remainder = &remainder[part.len()..];
            continue;
        }

        if index == parts.len() - 1 && !ends_with_wildcard {
            return remainder.ends_with(part);
        }

        if let Some(position) = remainder.find(part) {
            remainder = &remainder[(position + part.len())..];
        } else {
            return false;
        }
    }

    ends_with_wildcard || remainder.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DefaultsConfig, RouteRule};

    #[tokio::test]
    async fn preview_uses_filtered_route_overrides() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.*".into(),
                filter: [("session".to_string(), "issue-*".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("route".into()),
                webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Alert),
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event =
            IncomingEvent::tmux_keyword("issue-1440".into(), "error".into(), "boom".into(), None);

        let (channel, format, content) = router.preview(&event).await.unwrap();
        assert_eq!(channel, "route");
        assert_eq!(format, MessageFormat::Alert);
        assert_eq!(
            content,
            "🚨 tmux session issue-1440 hit keyword 'error': boom"
        );
    }

    #[tokio::test]
    async fn route_level_mention_is_prepended_for_custom() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "custom".into(),
                filter: Default::default(),
                channel: Some("route".into()),
                webhook: None,
                mention: Some("<@1465264645320474637>".into()),
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Compact),
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = IncomingEvent::custom(None, "wake up".into());
        let (channel, _, content) = router.preview(&event).await.unwrap();
        assert_eq!(channel, "route");
        assert_eq!(content, "<@1465264645320474637> wake up");
    }

    #[tokio::test]
    async fn route_level_mention_is_prepended_for_github_and_tmux() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![
                RouteRule {
                    event: "github.*".into(),
                    filter: [("repo".to_string(), "clawhip".to_string())]
                        .into_iter()
                        .collect(),
                    channel: Some("gh-route".into()),
                    webhook: None,
                    mention: Some("<@botid>".into()),
                    allow_dynamic_tokens: false,
                    format: Some(MessageFormat::Alert),
                    template: None,
                },
                RouteRule {
                    event: "tmux.*".into(),
                    filter: [("session".to_string(), "issue-*".to_string())]
                        .into_iter()
                        .collect(),
                    channel: Some("tmux-route".into()),
                    webhook: None,
                    mention: Some("<@botid>".into()),
                    allow_dynamic_tokens: false,
                    format: Some(MessageFormat::Alert),
                    template: None,
                },
            ],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));

        let github_event =
            IncomingEvent::github_issue_opened("clawhip".into(), 5, "boom".into(), None);
        let (_, _, github_content) = router.preview(&github_event).await.unwrap();
        assert!(github_content.starts_with("<@botid> "));
        assert!(github_content.contains("boom"));

        let tmux_event =
            IncomingEvent::tmux_keyword("issue-1440".into(), "error".into(), "failed".into(), None);
        let (_, _, tmux_content) = router.preview(&tmux_event).await.unwrap();
        assert!(tmux_content.starts_with("<@botid> "));
        assert!(tmux_content.contains("failed"));
    }

    #[tokio::test]
    async fn custom_send_can_inherit_dynamic_token_opt_in_from_channel_route() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.*".into(),
                filter: Default::default(),
                channel: Some("dynamic-route".into()),
                webhook: None,
                mention: None,
                allow_dynamic_tokens: true,
                format: None,
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = IncomingEvent::custom(Some("dynamic-route".into()), "{now}".into());
        let (_, _, content) = router.preview(&event).await.unwrap();
        assert_ne!(content, "{now}");
    }

    #[tokio::test]
    async fn custom_send_does_not_inherit_dynamic_tokens_without_channel_match() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.*".into(),
                filter: Default::default(),
                channel: Some("dynamic-route".into()),
                webhook: None,
                mention: None,
                allow_dynamic_tokens: true,
                format: None,
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = IncomingEvent::custom(None, "ignored".into());
        let (_, _, content) = router.preview(&event).await.unwrap();
        assert_eq!(content, "ignored");
    }

    #[tokio::test]
    async fn event_level_mention_is_used_when_route_mention_is_not_set() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.*".into(),
                filter: [("session".to_string(), "issue-*".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("tmux-route".into()),
                webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Alert),
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let mut event =
            IncomingEvent::tmux_keyword("issue-1440".into(), "error".into(), "failed".into(), None);
        event.mention = Some("<@event>".into());

        let (channel, format, content) = router.preview(&event).await.unwrap();
        assert_eq!(channel, "tmux-route");
        assert_eq!(format, MessageFormat::Alert);
        assert!(content.starts_with("<@event> "));
        assert!(content.contains("failed"));
    }

    #[tokio::test]
    async fn route_mention_takes_precedence_over_event_mention() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.*".into(),
                filter: Default::default(),
                channel: Some("tmux-route".into()),
                webhook: None,
                mention: Some("<@route>".into()),
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Compact),
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let mut event =
            IncomingEvent::tmux_keyword("issue-1440".into(), "error".into(), "failed".into(), None);
        event.mention = Some("<@event>".into());

        let (_, _, content) = router.preview(&event).await.unwrap();
        assert!(content.starts_with("<@route> "));
        assert!(!content.starts_with("<@event> "));
    }

    #[tokio::test]
    async fn git_commit_can_use_github_route_family_and_mention() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "github.*".into(),
                filter: [("repo".to_string(), "clawhip".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("route-channel".into()),
                webhook: None,
                mention: Some("<@route>".into()),
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Compact),
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = IncomingEvent::git_commit(
            "clawhip".into(),
            "main".into(),
            "1234567890abcdef".into(),
            "ship it".into(),
            None,
        );
        let (channel, _, content) = router.preview(&event).await.unwrap();
        assert_eq!(channel, "route-channel");
        assert!(content.starts_with("<@route> "));
        assert!(content.contains("ship it"));
    }

    #[tokio::test]
    async fn aggregated_git_commit_can_use_github_route_family_and_mention() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "github.*".into(),
                filter: [("repo".to_string(), "clawhip".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("route-channel".into()),
                webhook: None,
                mention: Some("<@route>".into()),
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Compact),
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = IncomingEvent::git_commit_events(
            "clawhip".into(),
            "main".into(),
            vec![
                ("1234567890abcdef".into(), "ship it".into()),
                ("234567890abcdef1".into(), "follow up".into()),
            ],
            None,
        )
        .into_iter()
        .next()
        .unwrap();

        let (channel, _, content) = router.preview(&event).await.unwrap();
        assert_eq!(channel, "route-channel");
        assert!(content.starts_with("<@route> "));
        assert!(content.contains("pushed 2 commits"));
        assert!(content.contains("- ship it"));
        assert!(content.contains("- follow up"));
    }

    #[tokio::test]
    async fn agent_family_route_matches_all_agent_events() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "agent.*".into(),
                filter: [("project".to_string(), "clawhip".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("agent-route".into()),
                webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Alert),
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));

        let started = IncomingEvent::agent_started(
            "worker-1".into(),
            Some("sess-123".into()),
            Some("clawhip".into()),
            None,
            Some("booted".into()),
            None,
            None,
        );
        let finished = IncomingEvent::agent_finished(
            "worker-1".into(),
            Some("sess-123".into()),
            Some("clawhip".into()),
            Some(300),
            Some("PR created".into()),
            None,
            None,
        );

        let (started_channel, started_format, started_content) =
            router.preview(&started).await.unwrap();
        let (finished_channel, finished_format, finished_content) =
            router.preview(&finished).await.unwrap();

        assert_eq!(started_channel, "agent-route");
        assert_eq!(finished_channel, "agent-route");
        assert_eq!(started_format, MessageFormat::Alert);
        assert_eq!(finished_format, MessageFormat::Alert);
        assert!(started_content.contains("worker-1"));
        assert!(started_content.contains("started"));
        assert!(finished_content.contains("worker-1"));
        assert!(finished_content.contains("finished"));
    }

    #[tokio::test]
    async fn filter_can_route_same_event_type_by_repo() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![
                RouteRule {
                    event: "github.*".into(),
                    filter: [("repo".to_string(), "oh-my-claudecode".to_string())]
                        .into_iter()
                        .collect(),
                    channel: Some("repo-a".into()),
                    webhook: None,
                    mention: None,
                    allow_dynamic_tokens: false,
                    format: None,
                    template: None,
                },
                RouteRule {
                    event: "github.*".into(),
                    filter: [("repo".to_string(), "clawhip".to_string())]
                        .into_iter()
                        .collect(),
                    channel: Some("repo-b".into()),
                    webhook: None,
                    mention: None,
                    allow_dynamic_tokens: false,
                    format: None,
                    template: None,
                },
            ],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = IncomingEvent::github_issue_opened("clawhip".into(), 7, "bug".into(), None);
        let (channel, _, _) = router.preview(&event).await.unwrap();
        assert_eq!(channel, "repo-b");
    }

    #[tokio::test]
    async fn webhook_route_is_used_as_delivery_target() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                filter: Default::default(),
                channel: None,
                webhook: Some("https://discord.com/api/webhooks/123/abc".into()),
                mention: None,
                allow_dynamic_tokens: false,
                format: None,
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event =
            IncomingEvent::tmux_keyword("issue-25".into(), "error".into(), "boom".into(), None);

        let delivery = router.preview_delivery(&event).await.unwrap();
        assert_eq!(
            delivery.target,
            DeliveryTarget::Webhook("https://discord.com/api/webhooks/123/abc".into())
        );
        assert!(delivery.content.contains("boom"));
    }

    #[tokio::test]
    async fn webhook_route_takes_precedence_over_event_channel() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                filter: Default::default(),
                channel: None,
                webhook: Some("https://discord.com/api/webhooks/123/abc".into()),
                mention: None,
                allow_dynamic_tokens: false,
                format: None,
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = IncomingEvent::tmux_keyword(
            "issue-25".into(),
            "error".into(),
            "boom".into(),
            Some("explicit-channel".into()),
        );

        let delivery = router.preview_delivery(&event).await.unwrap();
        assert_eq!(
            delivery.target,
            DeliveryTarget::Webhook("https://discord.com/api/webhooks/123/abc".into())
        );
    }
}
