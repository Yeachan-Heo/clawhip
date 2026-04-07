use std::sync::Arc;

use serde_json::json;

use crate::Result;
use crate::config::{AppConfig, RouteRule, default_sink_name};
use crate::dynamic_tokens;
use crate::events::{IncomingEvent, MessageFormat, RoutingMetadata};
#[cfg(test)]
use crate::render::DefaultRenderer;
use crate::render::Renderer;
#[cfg(test)]
use crate::sink::Sink;
#[cfg(test)]
use crate::sink::SinkMessage;
use crate::sink::SinkTarget;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDelivery {
    pub sink: String,
    pub target: SinkTarget,
    pub format: MessageFormat,
    pub mention: Option<String>,
    pub template: Option<String>,
    pub allow_dynamic_tokens: bool,
}

pub struct Router {
    config: Arc<AppConfig>,
}

impl Router {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }

    #[cfg(test)]
    pub async fn dispatch<S>(&self, event: &IncomingEvent, sink: &S) -> Result<()>
    where
        S: Sink + ?Sized,
    {
        let renderer = DefaultRenderer;
        for delivery in self.resolve(event).await? {
            let content = self.render_delivery(event, &delivery, &renderer).await?;
            let message = SinkMessage {
                event_kind: event.canonical_kind().to_string(),
                format: delivery.format.clone(),
                content,
                payload: event.payload.clone(),
            };
            if let Err(error) = sink.send(&delivery.target, &message).await {
                eprintln!(
                    "clawhip router delivery failed to {:?}: {error}",
                    delivery.target
                );
            }
        }

        Ok(())
    }

    pub async fn resolve(&self, event: &IncomingEvent) -> Result<Vec<ResolvedDelivery>> {
        let routes = self.routes_for(event);
        let routes = if routes.is_empty() {
            vec![None]
        } else {
            routes.into_iter().map(Some).collect()
        };
        let mut deliveries = Vec::with_capacity(routes.len());

        for route in routes {
            deliveries.push(self.resolve_delivery(event, route)?);
        }

        Ok(deliveries)
    }

    #[cfg(test)]
    pub async fn preview_delivery(&self, event: &IncomingEvent) -> Result<ResolvedDelivery> {
        let mut deliveries = self.resolve(event).await?;
        if deliveries.len() != 1 {
            return Err(format!("expected exactly one delivery, got {}", deliveries.len()).into());
        }

        Ok(deliveries.remove(0))
    }

    fn resolve_delivery(
        &self,
        event: &IncomingEvent,
        route: Option<&RouteRule>,
    ) -> Result<ResolvedDelivery> {
        let sink = route
            .map(RouteRule::effective_sink)
            .map(ToString::to_string)
            .unwrap_or_else(default_sink_name);
        let target = self.target_for(event, route, &sink)?;
        let format = event
            .format
            .clone()
            .or_else(|| route.and_then(|route| route.format.clone()))
            .unwrap_or_else(|| self.config.defaults.format.clone());

        Ok(ResolvedDelivery {
            sink,
            target,
            format,
            mention: route
                .and_then(|route| route.mention.clone())
                .or_else(|| event.mention.clone()),
            template: event
                .template
                .clone()
                .or_else(|| route.and_then(|route| route.template.clone())),
            allow_dynamic_tokens: self.allow_dynamic_tokens_for(event, route),
        })
    }

    pub async fn render_delivery<R: Renderer + ?Sized>(
        &self,
        event: &IncomingEvent,
        delivery: &ResolvedDelivery,
        renderer: &R,
    ) -> Result<String> {
        let content = self.render_delivery_body(event, delivery, renderer).await?;

        match delivery.mention.as_deref().map(str::trim) {
            Some(mention) if !mention.is_empty() => Ok(format!("{mention} {content}")),
            _ => Ok(content),
        }
    }

    pub async fn render_delivery_body<R: Renderer + ?Sized>(
        &self,
        event: &IncomingEvent,
        delivery: &ResolvedDelivery,
        renderer: &R,
    ) -> Result<String> {
        if let Some(template) = delivery.template.as_deref() {
            return Ok(dynamic_tokens::render_template(
                template,
                &event.template_context(),
                delivery.allow_dynamic_tokens,
            )
            .await);
        }

        let rendered = renderer.render(event, &delivery.format)?;
        if delivery.allow_dynamic_tokens {
            Ok(dynamic_tokens::render_template(&rendered, &event.template_context(), true).await)
        } else {
            Ok(rendered)
        }
    }

    #[cfg(test)]
    pub async fn preview(&self, event: &IncomingEvent) -> Result<(String, MessageFormat, String)> {
        let delivery = self.preview_delivery(event).await?;
        let content = self
            .render_delivery(event, &delivery, &DefaultRenderer)
            .await?;
        match delivery.target {
            SinkTarget::DiscordChannel(channel) => Ok((channel, delivery.format, content)),
            SinkTarget::DiscordWebhook(_) | SinkTarget::SlackWebhook(_) => {
                Err("matched route uses a webhook instead of a channel".into())
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

    fn routes_for<'a>(&'a self, event: &IncomingEvent) -> Vec<&'a RouteRule> {
        let context = event.template_context();
        matching_routes_for(&self.config.routes, event.canonical_kind(), &context)
    }

    fn target_for(
        &self,
        event: &IncomingEvent,
        route: Option<&RouteRule>,
        sink: &str,
    ) -> Result<SinkTarget> {
        match sink {
            "discord" => {
                if let Some(webhook) = route.and_then(RouteRule::discord_webhook_target) {
                    return Ok(SinkTarget::DiscordWebhook(webhook.to_string()));
                }

                // For custom events (e.g. `clawhip send --channel X`), the
                // event-level channel represents explicit user intent and must
                // take highest priority — above both route and default channels.
                let channel = if event.canonical_kind() == "custom" {
                    event
                        .channel
                        .clone()
                        .or_else(|| route.and_then(|route| route.channel.clone()))
                        .or_else(|| self.config.defaults.channel.clone())
                } else {
                    route
                        .and_then(|route| route.channel.clone())
                        .or_else(|| event.channel.clone())
                        .or_else(|| self.config.defaults.channel.clone())
                }
                .ok_or_else(|| {
                    format!("no channel configured for event {}", event.canonical_kind())
                })?;

                Ok(SinkTarget::DiscordChannel(channel))
            }
            "slack" => route
                .and_then(RouteRule::slack_webhook_target)
                .map(|webhook| SinkTarget::SlackWebhook(webhook.to_string()))
                .ok_or_else(|| {
                    format!(
                        "no Slack webhook configured for event {}",
                        event.canonical_kind()
                    )
                    .into()
                }),
            other => Err(format!(
                "unsupported sink '{other}' for event {}",
                event.canonical_kind()
            )
            .into()),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn resolve_tmux_session_channel(
    config: &AppConfig,
    session_name: &str,
) -> Option<String> {
    resolve_tmux_session_channel_with_metadata(config, session_name, &RoutingMetadata::default())
}

pub(crate) fn resolve_tmux_session_channel_with_metadata(
    config: &AppConfig,
    session_name: &str,
    routing: &RoutingMetadata,
) -> Option<String> {
    let tmux_context =
        IncomingEvent::tmux_keyword(session_name.to_string(), String::new(), String::new(), None)
            .with_routing_metadata(routing)
            .template_context();
    let session_context = IncomingEvent {
        kind: "session.started".to_string(),
        channel: None,
        mention: None,
        format: None,
        template: None,
        payload: json!({
            "session_name": session_name,
            "session": session_name,
            "tool": "tmux",
        }),
    }
    .with_routing_metadata(routing)
    .template_context();
    let prefer_metadata = prefers_metadata_first_routing("tmux.keyword", &tmux_context)
        || prefers_metadata_first_routing("session.started", &session_context);
    let mut preferred = Vec::new();
    let mut heuristic = Vec::new();

    for route in config.routes.iter().filter(|route| {
        route_matches(route, "tmux.keyword", &tmux_context)
            || route_matches(route, "session.started", &session_context)
    }) {
        if prefer_metadata && route_uses_session_name_prefix_heuristics(route) {
            heuristic.push(route);
        } else {
            preferred.push(route);
        }
    }

    if !prefer_metadata {
        preferred.extend(heuristic);
    }

    for route in preferred {
        if route.effective_sink() != "discord" {
            continue;
        }
        if let Some(channel) = route_channel(route) {
            return Some(channel.to_string());
        }
    }

    config.defaults.channel.clone()
}

fn route_candidates(kind: &str) -> Vec<&str> {
    match kind {
        "git.commit" => vec!["git.commit", "github.commit"],
        "git.branch-changed" => vec!["git.branch-changed", "github.branch-changed"],
        "agent.started" | "agent.blocked" | "agent.finished" | "agent.failed" => {
            vec![kind, "agent.*", "session.*"]
        }
        "session.started" | "session.blocked" | "session.finished" | "session.failed" => {
            vec![kind, "session.*", "agent.*"]
        }
        "session.retry-needed"
        | "session.pr-created"
        | "session.test-started"
        | "session.test-finished"
        | "session.test-failed"
        | "session.handoff-needed" => {
            vec![kind, "session.*"]
        }
        "tmux.content_changed" | "tmux.heartbeat" => {
            vec![kind, "tmux.*"]
        }
        other => vec![other],
    }
}

fn route_matches(
    route: &RouteRule,
    canonical_kind: &str,
    context: &std::collections::BTreeMap<String, String>,
) -> bool {
    route_candidates(canonical_kind)
        .iter()
        .any(|candidate| glob_match(&route.event, candidate))
        && route.filter.iter().all(|(key, expected)| {
            context
                .get(key)
                .map(|actual| glob_match(expected, actual))
                .unwrap_or(false)
        })
}

fn matching_routes_for<'a>(
    routes: &'a [RouteRule],
    canonical_kind: &str,
    context: &std::collections::BTreeMap<String, String>,
) -> Vec<&'a RouteRule> {
    let prefer_metadata = prefers_metadata_first_routing(canonical_kind, context);
    let mut preferred = Vec::new();
    let mut heuristic = Vec::new();

    for route in routes
        .iter()
        .filter(|route| route_matches(route, canonical_kind, context))
    {
        if prefer_metadata && route_uses_session_name_prefix_heuristics(route) {
            heuristic.push(route);
        } else {
            preferred.push(route);
        }
    }

    if !prefer_metadata {
        preferred.extend(heuristic);
    }

    preferred
}

fn prefers_metadata_first_routing(
    canonical_kind: &str,
    context: &std::collections::BTreeMap<String, String>,
) -> bool {
    if !(canonical_kind.starts_with("session.") || canonical_kind.starts_with("tmux.")) {
        return false;
    }

    [
        "project",
        "repo_name",
        "repo_path",
        "worktree_path",
        "session_id",
    ]
    .into_iter()
    .filter_map(|key| context.get(key))
    .any(|value| !value.trim().is_empty())
}

fn route_uses_session_name_prefix_heuristics(route: &RouteRule) -> bool {
    !route.filter.is_empty()
        && route.filter.iter().all(|(key, expected)| {
            matches!(key.as_str(), "session" | "session_name") && expected.contains('*')
        })
}

fn route_channel(route: &RouteRule) -> Option<&str> {
    route
        .channel
        .as_deref()
        .map(str::trim)
        .filter(|channel| !channel.is_empty())
}

pub(crate) fn glob_match(pattern: &str, value: &str) -> bool {
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
    use crate::events::{RoutingMetadata, normalize_event};
    use crate::render::DefaultRenderer;
    use crate::sink::{DiscordSink, SlackSink};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn resolve_returns_all_matching_deliveries_in_route_order() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![
                RouteRule {
                    event: "tmux.keyword".into(),
                    sink: "discord".into(),
                    filter: Default::default(),
                    channel: Some("ops".into()),
                    webhook: None,
                    slack_webhook: None,
                    mention: Some("@ops".into()),
                    allow_dynamic_tokens: false,
                    format: Some(MessageFormat::Alert),
                    template: None,
                },
                RouteRule {
                    event: "tmux.*".into(),
                    sink: "discord".into(),
                    filter: Default::default(),
                    channel: Some("eng".into()),
                    webhook: None,
                    slack_webhook: None,
                    mention: Some("@eng".into()),
                    allow_dynamic_tokens: false,
                    format: Some(MessageFormat::Compact),
                    template: Some("duplicate: {line}".into()),
                },
            ],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event =
            IncomingEvent::tmux_keyword("issue-24".into(), "error".into(), "boom".into(), None);

        let deliveries = router.resolve(&event).await.unwrap();

        assert_eq!(deliveries.len(), 2);
        assert_eq!(
            deliveries[0].target,
            SinkTarget::DiscordChannel("ops".into())
        );
        assert_eq!(deliveries[0].format, MessageFormat::Alert);
        let first = router
            .render_delivery(&event, &deliveries[0], &DefaultRenderer)
            .await
            .unwrap();
        assert!(first.starts_with("@ops "));
        assert!(first.contains("boom"));
        assert_eq!(
            deliveries[1].target,
            SinkTarget::DiscordChannel("eng".into())
        );
        assert_eq!(deliveries[1].format, MessageFormat::Compact);
        let second = router
            .render_delivery(&event, &deliveries[1], &DefaultRenderer)
            .await
            .unwrap();
        assert_eq!(second, "@eng duplicate: boom");
    }

    #[tokio::test]
    async fn resolve_uses_defaults_when_no_routes_match() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("fallback".into()),
                format: MessageFormat::Alert,
            },
            routes: vec![RouteRule {
                event: "github.*".into(),
                sink: "discord".into(),
                filter: Default::default(),
                channel: Some("github".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Compact),
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = IncomingEvent::custom(None, "wake up".into());

        let deliveries = router.resolve(&event).await.unwrap();

        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].sink, default_sink_name());
        assert_eq!(
            deliveries[0].target,
            SinkTarget::DiscordChannel("fallback".into())
        );
        assert_eq!(deliveries[0].format, MessageFormat::Alert);
        assert_eq!(
            router
                .render_delivery(&event, &deliveries[0], &DefaultRenderer)
                .await
                .unwrap(),
            "🚨 wake up"
        );
    }

    #[tokio::test]
    async fn dispatch_best_effort_continues_after_webhook_failure() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::time::{Duration, timeout};

        async fn spawn_webhook(status: &str) -> (String, tokio::task::JoinHandle<String>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let status_line = status.to_string();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0_u8; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let response = format!("HTTP/1.1 {status_line}\r\ncontent-length: 0\r\n\r\n");
                stream.write_all(response.as_bytes()).await.unwrap();
                req
            });

            (format!("http://{addr}/webhook"), server)
        }

        let (failing_webhook, failing_server) = spawn_webhook("500 Internal Server Error").await;
        let (successful_webhook, successful_server) = spawn_webhook("204 No Content").await;
        let config = AppConfig {
            routes: vec![
                RouteRule {
                    event: "tmux.keyword".into(),
                    sink: "discord".into(),
                    filter: Default::default(),
                    channel: None,
                    webhook: Some(failing_webhook),
                    slack_webhook: None,
                    mention: None,
                    allow_dynamic_tokens: false,
                    format: None,
                    template: Some("first".into()),
                },
                RouteRule {
                    event: "tmux.keyword".into(),
                    sink: "discord".into(),
                    filter: Default::default(),
                    channel: None,
                    webhook: Some(successful_webhook),
                    slack_webhook: None,
                    mention: None,
                    allow_dynamic_tokens: false,
                    format: None,
                    template: Some("second".into()),
                },
            ],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let discord = DiscordSink::from_config(Arc::new(AppConfig::default())).unwrap();
        let event =
            IncomingEvent::tmux_keyword("issue-24".into(), "error".into(), "boom".into(), None);

        router.dispatch(&event, &discord).await.unwrap();

        let failing_request = timeout(Duration::from_secs(2), failing_server)
            .await
            .unwrap()
            .unwrap();
        let successful_request = timeout(Duration::from_secs(2), successful_server)
            .await
            .unwrap()
            .unwrap();
        assert!(failing_request.contains("\"content\":\"first\""));
        assert!(successful_request.contains("\"content\":\"second\""));
    }

    #[tokio::test]
    async fn preview_uses_filtered_route_overrides() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.*".into(),
                sink: "discord".into(),
                filter: [("session".to_string(), "issue-*".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("route".into()),
                webhook: None,
                slack_webhook: None,
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
    async fn preview_matches_git_routes_on_worktree_path() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "git.commit".into(),
                sink: "discord".into(),
                filter: [("worktree_path".to_string(), "*/issue-115".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("worktrees".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Compact),
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = IncomingEvent::git_commit(
            "clawhip".into(),
            "feat/issue-115".into(),
            "1234567890abcdef".into(),
            "ship it".into(),
            None,
        )
        .with_repo_context(
            Some("/repo/clawhip".into()),
            Some("/repo/.worktrees/issue-115".into()),
        );

        let (channel, format, content) = router.preview(&event).await.unwrap();
        assert_eq!(channel, "worktrees");
        assert_eq!(format, MessageFormat::Compact);
        assert_eq!(
            content,
            "git:clawhip[wt:issue-115]@feat/issue-115 1234567 ship it"
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
                sink: "discord".into(),
                filter: Default::default(),
                channel: Some("route".into()),
                webhook: None,
                slack_webhook: None,
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
                    sink: "discord".into(),
                    filter: [("repo".to_string(), "clawhip".to_string())]
                        .into_iter()
                        .collect(),
                    channel: Some("gh-route".into()),
                    webhook: None,
                    slack_webhook: None,
                    mention: Some("<@botid>".into()),
                    allow_dynamic_tokens: false,
                    format: Some(MessageFormat::Alert),
                    template: None,
                },
                RouteRule {
                    event: "tmux.*".into(),
                    sink: "discord".into(),
                    filter: [("session".to_string(), "issue-*".to_string())]
                        .into_iter()
                        .collect(),
                    channel: Some("tmux-route".into()),
                    webhook: None,
                    slack_webhook: None,
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
                sink: "discord".into(),
                filter: Default::default(),
                channel: Some("dynamic-route".into()),
                webhook: None,
                slack_webhook: None,
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
                sink: "discord".into(),
                filter: Default::default(),
                channel: Some("dynamic-route".into()),
                webhook: None,
                slack_webhook: None,
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
                sink: "discord".into(),
                filter: [("session".to_string(), "issue-*".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("tmux-route".into()),
                webhook: None,
                slack_webhook: None,
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
                sink: "discord".into(),
                filter: Default::default(),
                channel: Some("tmux-route".into()),
                webhook: None,
                slack_webhook: None,
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
                sink: "discord".into(),
                filter: [("repo".to_string(), "clawhip".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("route-channel".into()),
                webhook: None,
                slack_webhook: None,
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
                sink: "discord".into(),
                filter: [("repo".to_string(), "clawhip".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("route-channel".into()),
                webhook: None,
                slack_webhook: None,
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
                sink: "discord".into(),
                filter: [("project".to_string(), "clawhip".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("agent-route".into()),
                webhook: None,
                slack_webhook: None,
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
    async fn legacy_agent_events_match_session_routes() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "session.*".into(),
                sink: "discord".into(),
                filter: [
                    ("tool".to_string(), "omx".to_string()),
                    ("project".to_string(), "clawhip".to_string()),
                ]
                .into_iter()
                .collect(),
                channel: Some("session-route".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Compact),
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = normalize_event(IncomingEvent::agent_finished(
            "omx".into(),
            Some("issue-65".into()),
            Some("clawhip".into()),
            Some(42),
            Some("PR created".into()),
            None,
            None,
        ));

        let (channel, format, content) = router.preview(&event).await.unwrap();

        assert_eq!(channel, "session-route");
        assert_eq!(format, MessageFormat::Compact);
        assert!(content.contains("agent omx"));
        assert!(content.contains("finished"));
    }

    #[tokio::test]
    async fn native_omc_session_events_match_session_routes() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "session.*".into(),
                sink: "discord".into(),
                filter: [
                    ("tool".to_string(), "omc".to_string()),
                    ("repo_name".to_string(), "clawhip".to_string()),
                ]
                .into_iter()
                .collect(),
                channel: Some("session-route".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Compact),
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = normalize_event(IncomingEvent {
            kind: "post-tool-use".into(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: json!({
                "timestamp": "2026-03-09T18:01:58.000Z",
                "signal": {
                    "routeKey": "pull-request.created",
                    "phase": "finished",
                    "summary": "https://github.com/Yeachan-Heo/clawhip/pull/67"
                },
                "context": {
                    "sessionId": "issue-65",
                    "projectPath": "/repo/clawhip-worktrees/issue-65",
                    "projectName": "clawhip"
                }
            }),
        });

        let (channel, format, content) = router.preview(&event).await.unwrap();

        assert_eq!(channel, "session-route");
        assert_eq!(format, MessageFormat::Compact);
        assert!(content.contains("omc issue-65 pr-created"));
        assert!(content.contains("repo=clawhip"));
        assert!(content.contains("pr=#67"));
    }

    #[tokio::test]
    async fn session_lifecycle_events_match_existing_agent_routes() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "agent.*".into(),
                sink: "discord".into(),
                filter: [
                    ("tool".to_string(), "omx".to_string()),
                    ("repo_name".to_string(), "clawhip".to_string()),
                ]
                .into_iter()
                .collect(),
                channel: Some("agent-route".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: Some(MessageFormat::Compact),
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = normalize_event(IncomingEvent {
            kind: "finished".into(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: json!({
                "context": {
                    "normalized_event": "finished",
                    "session_name": "issue-65",
                    "repo_name": "clawhip"
                }
            }),
        });

        let (channel, format, content) = router.preview(&event).await.unwrap();

        assert_eq!(channel, "agent-route");
        assert_eq!(format, MessageFormat::Compact);
        assert!(content.contains("omx issue-65 finished"));
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
                    sink: "discord".into(),
                    filter: [("repo".to_string(), "oh-my-claudecode".to_string())]
                        .into_iter()
                        .collect(),
                    channel: Some("repo-a".into()),
                    webhook: None,
                    slack_webhook: None,
                    mention: None,
                    allow_dynamic_tokens: false,
                    format: None,
                    template: None,
                },
                RouteRule {
                    event: "github.*".into(),
                    sink: "discord".into(),
                    filter: [("repo".to_string(), "clawhip".to_string())]
                        .into_iter()
                        .collect(),
                    channel: Some("repo-b".into()),
                    webhook: None,
                    slack_webhook: None,
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
    async fn git_and_github_routes_can_filter_on_repo_name_alias() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "git.commit".into(),
                sink: "discord".into(),
                filter: [("repo_name".to_string(), "clawhip".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("repo-name-route".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: None,
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

        let (channel, _, _) = router.preview(&event).await.unwrap();
        assert_eq!(channel, "repo-name-route");
    }

    #[tokio::test]
    async fn tmux_and_session_routes_share_session_alias_filters() {
        let tmux_config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                sink: "discord".into(),
                filter: [("session_name".to_string(), "issue-*".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("tmux-session-name".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: None,
                template: None,
            }],
            ..AppConfig::default()
        };
        let tmux_router = Router::new(Arc::new(tmux_config));
        let tmux_event =
            IncomingEvent::tmux_keyword("issue-132".into(), "error".into(), "boom".into(), None);
        let (tmux_channel, _, _) = tmux_router.preview(&tmux_event).await.unwrap();
        assert_eq!(tmux_channel, "tmux-session-name");

        let session_config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "session.started".into(),
                sink: "discord".into(),
                filter: [("session".to_string(), "issue-*".to_string())]
                    .into_iter()
                    .collect(),
                channel: Some("session-alias-route".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: None,
                template: None,
            }],
            ..AppConfig::default()
        };
        let session_router = Router::new(Arc::new(session_config));
        let session_event = IncomingEvent {
            kind: "session.started".into(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: json!({
                "session_name": "issue-132",
                "tool": "omx"
            }),
        };

        let (session_channel, _, _) = session_router.preview(&session_event).await.unwrap();
        assert_eq!(session_channel, "session-alias-route");
    }

    #[tokio::test]
    async fn tmux_routes_prefer_repo_metadata_over_session_prefix_heuristics() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![
                RouteRule {
                    event: "tmux.*".into(),
                    sink: "discord".into(),
                    filter: [("session_name".to_string(), "clawhip-*".to_string())]
                        .into_iter()
                        .collect(),
                    channel: Some("heuristic-route".into()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "tmux.*".into(),
                    sink: "discord".into(),
                    filter: [("repo_name".to_string(), "clawhip".to_string())]
                        .into_iter()
                        .collect(),
                    channel: Some("metadata-route".into()),
                    ..RouteRule::default()
                },
            ],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = IncomingEvent::tmux_keyword(
            "clawhip-issue-152".into(),
            "error".into(),
            "boom".into(),
            None,
        )
        .with_routing_metadata(&RoutingMetadata {
            repo_name: Some("clawhip".into()),
            project: Some("clawhip".into()),
            worktree_path: Some("/repo/clawhip.worktrees/issue-152".into()),
            ..RoutingMetadata::default()
        });

        let deliveries = router.resolve(&event).await.unwrap();
        assert_eq!(
            deliveries.first().map(|delivery| &delivery.target),
            Some(&SinkTarget::DiscordChannel("metadata-route".into()))
        );
    }

    #[tokio::test]
    async fn session_routes_prefer_repo_metadata_over_session_prefix_heuristics() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![
                RouteRule {
                    event: "session.*".into(),
                    sink: "discord".into(),
                    filter: [("session_name".to_string(), "clawhip-*".to_string())]
                        .into_iter()
                        .collect(),
                    channel: Some("heuristic-route".into()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "session.*".into(),
                    sink: "discord".into(),
                    filter: [("repo_name".to_string(), "clawhip".to_string())]
                        .into_iter()
                        .collect(),
                    channel: Some("metadata-route".into()),
                    ..RouteRule::default()
                },
            ],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event = normalize_event(IncomingEvent {
            kind: "session.started".into(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: json!({
                "session_name": "clawhip-issue-152",
                "repo_name": "clawhip",
                "worktree_path": "/repo/clawhip.worktrees/issue-152",
            }),
        });

        let deliveries = router.resolve(&event).await.unwrap();
        assert_eq!(
            deliveries.first().map(|delivery| &delivery.target),
            Some(&SinkTarget::DiscordChannel("metadata-route".into()))
        );
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
                sink: "discord".into(),
                filter: Default::default(),
                channel: None,
                webhook: Some("https://discord.com/api/webhooks/123/abc".into()),
                slack_webhook: None,
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
            SinkTarget::DiscordWebhook("https://discord.com/api/webhooks/123/abc".into())
        );
        assert_eq!(
            router
                .render_delivery(&event, &delivery, &DefaultRenderer)
                .await
                .unwrap(),
            "tmux:issue-25 matched 'error' => boom"
        );
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
                sink: "discord".into(),
                filter: Default::default(),
                channel: None,
                webhook: Some("https://discord.com/api/webhooks/123/abc".into()),
                slack_webhook: None,
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
            SinkTarget::DiscordWebhook("https://discord.com/api/webhooks/123/abc".into())
        );
    }

    #[tokio::test]
    async fn route_channel_takes_precedence_over_event_channel() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                sink: "discord".into(),
                filter: Default::default(),
                channel: Some("route-channel".into()),
                webhook: None,
                slack_webhook: None,
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
            Some("launcher-channel".into()),
        );

        let delivery = router.preview_delivery(&event).await.unwrap();
        assert_eq!(
            delivery.target,
            SinkTarget::DiscordChannel("route-channel".into())
        );
    }

    #[tokio::test]
    async fn event_channel_is_used_when_matching_route_has_no_channel() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                sink: "discord".into(),
                filter: Default::default(),
                channel: None,
                webhook: None,
                slack_webhook: None,
                mention: Some("<@route>".into()),
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
            Some("monitor-channel".into()),
        );

        let delivery = router.preview_delivery(&event).await.unwrap();
        assert_eq!(
            delivery.target,
            SinkTarget::DiscordChannel("monitor-channel".into())
        );
    }

    #[tokio::test]
    async fn custom_event_channel_takes_precedence_over_route_channel() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default-ch".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "custom".into(),
                sink: "discord".into(),
                filter: Default::default(),
                channel: Some("route-ch".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: None,
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));

        // clawhip send --channel user-target --message "hello"
        let event = IncomingEvent::custom(Some("user-target".into()), "hello".into());
        let delivery = router.preview_delivery(&event).await.unwrap();
        assert_eq!(
            delivery.target,
            SinkTarget::DiscordChannel("user-target".into()),
            "custom event channel (from --channel flag) must override route channel"
        );
    }

    #[tokio::test]
    async fn custom_event_without_channel_falls_back_to_route_then_default() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default-ch".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "custom".into(),
                sink: "discord".into(),
                filter: Default::default(),
                channel: Some("route-ch".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: None,
                template: None,
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));

        // clawhip send --message "hello" (no --channel)
        let event = IncomingEvent::custom(None, "hello".into());
        let delivery = router.preview_delivery(&event).await.unwrap();
        assert_eq!(
            delivery.target,
            SinkTarget::DiscordChannel("route-ch".into()),
            "custom event without explicit channel should fall back to route channel"
        );
    }

    #[tokio::test]
    async fn slack_webhook_route_is_used_as_delivery_target() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                slack_webhook: Some("https://hooks.slack.com/services/T/B/abc".into()),
                format: Some(MessageFormat::Alert),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let event =
            IncomingEvent::tmux_keyword("issue-28".into(), "error".into(), "boom".into(), None);

        let delivery = router.preview_delivery(&event).await.unwrap();
        assert_eq!(delivery.sink, "slack");
        assert_eq!(
            delivery.target,
            SinkTarget::SlackWebhook("https://hooks.slack.com/services/T/B/abc".into())
        );
        assert_eq!(
            router
                .render_delivery(&event, &delivery, &DefaultRenderer)
                .await
                .unwrap(),
            "🚨 tmux session issue-28 hit keyword 'error': boom"
        );
    }

    #[tokio::test]
    async fn slack_sink_route_can_use_generic_webhook_field() {
        let config = AppConfig {
            routes: vec![RouteRule {
                event: "custom".into(),
                sink: "slack".into(),
                webhook: Some("https://hooks.slack.com/services/T/B/generic".into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let delivery = router
            .preview_delivery(&IncomingEvent::custom(None, "hello".into()))
            .await
            .unwrap();

        assert_eq!(delivery.sink, "slack");
        assert_eq!(
            delivery.target,
            SinkTarget::SlackWebhook("https://hooks.slack.com/services/T/B/generic".into())
        );
    }

    #[test]
    fn resolve_tmux_session_channel_prefers_matching_tmux_route() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.*".into(),
                filter: BTreeMap::from([("session".into(), "xeroclaw-*".into())]),
                sink: "discord".into(),
                channel: Some("xeroclaw-dev".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: None,
                template: None,
            }],
            ..AppConfig::default()
        };

        assert_eq!(
            resolve_tmux_session_channel(&config, "xeroclaw-42").as_deref(),
            Some("xeroclaw-dev")
        );
    }

    #[test]
    fn resolve_tmux_session_channel_supports_session_routes() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "session.*".into(),
                filter: BTreeMap::from([("session_name".into(), "xeroclaw-*".into())]),
                sink: "discord".into(),
                channel: Some("xeroclaw-dev".into()),
                webhook: None,
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: None,
                template: None,
            }],
            ..AppConfig::default()
        };

        assert_eq!(
            resolve_tmux_session_channel(&config, "xeroclaw-42").as_deref(),
            Some("xeroclaw-dev")
        );
    }

    #[test]
    fn resolve_tmux_session_channel_with_metadata_prefers_repo_route_over_prefix_heuristic() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![
                RouteRule {
                    event: "tmux.*".into(),
                    filter: BTreeMap::from([("session".into(), "clawhip-*".into())]),
                    sink: "discord".into(),
                    channel: Some("heuristic-route".into()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "tmux.*".into(),
                    filter: BTreeMap::from([("repo_name".into(), "clawhip".into())]),
                    sink: "discord".into(),
                    channel: Some("metadata-route".into()),
                    ..RouteRule::default()
                },
            ],
            ..AppConfig::default()
        };

        assert_eq!(
            resolve_tmux_session_channel_with_metadata(
                &config,
                "clawhip-issue-152",
                &RoutingMetadata {
                    repo_name: Some("clawhip".into()),
                    project: Some("clawhip".into()),
                    worktree_path: Some("/repo/clawhip.worktrees/issue-152".into()),
                    ..RoutingMetadata::default()
                }
            )
            .as_deref(),
            Some("metadata-route")
        );
    }

    #[test]
    fn resolve_tmux_session_channel_with_metadata_does_not_fallback_to_prefix_heuristics() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.*".into(),
                filter: BTreeMap::from([("session".into(), "clawhip-*".into())]),
                sink: "discord".into(),
                channel: Some("heuristic-route".into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };

        assert_eq!(
            resolve_tmux_session_channel_with_metadata(
                &config,
                "clawhip-issue-152",
                &RoutingMetadata {
                    repo_name: Some("clawhip".into()),
                    project: Some("clawhip".into()),
                    worktree_path: Some("/repo/clawhip.worktrees/issue-152".into()),
                    ..RoutingMetadata::default()
                }
            )
            .as_deref(),
            Some("default")
        );
    }

    #[test]
    fn resolve_tmux_session_channel_skips_webhooks_and_falls_back_to_defaults() {
        let config = AppConfig {
            defaults: DefaultsConfig {
                channel: Some("default".into()),
                format: MessageFormat::Compact,
            },
            routes: vec![RouteRule {
                event: "tmux.*".into(),
                filter: BTreeMap::from([("session".into(), "xeroclaw-*".into())]),
                sink: "discord".into(),
                channel: None,
                webhook: Some("https://discord.com/api/webhooks/123/abc".into()),
                slack_webhook: None,
                mention: None,
                allow_dynamic_tokens: false,
                format: None,
                template: None,
            }],
            ..AppConfig::default()
        };

        assert_eq!(
            resolve_tmux_session_channel(&config, "xeroclaw-42").as_deref(),
            Some("default")
        );
    }

    #[tokio::test]
    async fn slack_dispatch_posts_block_kit_payload() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0_u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let response = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
            stream.write_all(response.as_bytes()).await.unwrap();
            req
        });

        let config = AppConfig {
            routes: vec![RouteRule {
                event: "custom".into(),
                slack_webhook: Some(format!("http://{addr}/webhook")),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };
        let router = Router::new(Arc::new(config));
        let slack = SlackSink::default();

        router
            .dispatch(
                &IncomingEvent::custom(None, "hello from clawhip".into()),
                &slack,
            )
            .await
            .unwrap();

        let request = timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(request.contains("\"text\":\"hello from clawhip\""));
        assert!(request.contains("\"blocks\""));
    }
}
