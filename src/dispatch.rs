use std::collections::HashMap;

use tokio::sync::mpsc;

use crate::Result;
use crate::events::IncomingEvent;
use crate::render::Renderer;
use crate::router::Router;
use crate::sink::{Sink, SinkMessage};

pub struct Dispatcher {
    rx: mpsc::Receiver<IncomingEvent>,
    router: Router,
    renderer: Box<dyn Renderer>,
    sinks: HashMap<String, Box<dyn Sink>>,
}

impl Dispatcher {
    pub fn new(
        rx: mpsc::Receiver<IncomingEvent>,
        router: Router,
        renderer: Box<dyn Renderer>,
        sinks: HashMap<String, Box<dyn Sink>>,
    ) -> Self {
        Self {
            rx,
            router,
            renderer,
            sinks,
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        while let Some(event) = self.rx.recv().await {
            let deliveries = match self.router.resolve(&event).await {
                Ok(deliveries) => deliveries,
                Err(error) => {
                    eprintln!(
                        "clawhip dispatcher failed to resolve {}: {error}",
                        event.canonical_kind()
                    );
                    continue;
                }
            };

            for delivery in deliveries {
                let Some(sink) = self.sinks.get(delivery.sink.as_str()) else {
                    eprintln!(
                        "clawhip dispatcher missing sink '{}' for target {:?}",
                        delivery.sink, delivery.target
                    );
                    continue;
                };

                let content = match self
                    .router
                    .render_delivery(&event, &delivery, self.renderer.as_ref())
                    .await
                {
                    Ok(content) => content,
                    Err(error) => {
                        eprintln!(
                            "clawhip dispatcher failed to render {} for {}/ {:?}: {error}",
                            event.canonical_kind(),
                            delivery.sink,
                            delivery.target
                        );
                        continue;
                    }
                };

                let message = SinkMessage {
                    event_kind: event.canonical_kind().to_string(),
                    format: delivery.format.clone(),
                    content,
                    payload: event.payload.clone(),
                };

                if let Err(error) = sink.send(&delivery.target, &message).await {
                    eprintln!(
                        "clawhip dispatcher delivery failed to {}/ {:?}: {error}",
                        delivery.sink, delivery.target
                    );
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use crate::config::{AppConfig, RouteRule};
    use crate::render::DefaultRenderer;
    use crate::sink::{DiscordSink, SlackSink};

    fn test_dispatcher(rx: mpsc::Receiver<IncomingEvent>, router: Router) -> Dispatcher {
        let mut sinks: HashMap<String, Box<dyn Sink>> = HashMap::new();
        sinks.insert(
            "discord".into(),
            Box::new(DiscordSink::from_config(Arc::new(AppConfig::default())).unwrap()),
        );
        sinks.insert("slack".into(), Box::new(SlackSink::default()));
        Dispatcher::new(rx, router, Box::new(DefaultRenderer), sinks)
    }

    #[tokio::test]
    async fn dispatcher_stops_cleanly_when_channel_closes() {
        let (tx, rx) = mpsc::channel(1);
        drop(tx);
        let router = Router::new(Arc::new(AppConfig::default()));
        let mut dispatcher = test_dispatcher(rx, router);

        dispatcher.run().await.unwrap();
    }

    #[tokio::test]
    async fn dispatcher_continues_after_webhook_failure() {
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
        let (tx, rx) = mpsc::channel(1);
        let router = Router::new(Arc::new(config));
        let mut dispatcher = test_dispatcher(rx, router);
        let task = tokio::spawn(async move { dispatcher.run().await.unwrap() });

        tx.send(IncomingEvent::tmux_keyword(
            "issue-24".into(),
            "error".into(),
            "boom".into(),
            None,
        ))
        .await
        .unwrap();
        drop(tx);

        task.await.unwrap();
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
    async fn dispatcher_sends_to_slack_webhook() {
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
                event: "tmux.keyword".into(),
                slack_webhook: Some(format!("http://{addr}/webhook")),
                format: Some(crate::events::MessageFormat::Alert),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };
        let (tx, rx) = mpsc::channel(1);
        let router = Router::new(Arc::new(config));
        let mut dispatcher = test_dispatcher(rx, router);
        let task = tokio::spawn(async move { dispatcher.run().await.unwrap() });

        tx.send(IncomingEvent::tmux_keyword(
            "issue-28".into(),
            "error".into(),
            "boom".into(),
            None,
        ))
        .await
        .unwrap();
        drop(tx);

        task.await.unwrap();
        let request = timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(
            request.contains("\"text\":\"🚨 tmux session issue-28 hit keyword 'error': boom\"")
        );
        assert!(request.contains("\"blocks\""));
    }
}
