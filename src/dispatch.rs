use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::Result;
use crate::core::timer_wheel::{DelayedEntry, TimerWheel};
use crate::events::IncomingEvent;
use crate::render::Renderer;
use crate::router::{ResolvedDelivery, Router};
use crate::sink::{Sink, SinkMessage, SinkTarget};

const DEFAULT_BATCH_TICK: Duration = Duration::from_secs(1);

pub struct Dispatcher {
    rx: mpsc::Receiver<IncomingEvent>,
    router: Router,
    renderer: Box<dyn Renderer>,
    sinks: HashMap<String, Box<dyn Sink>>,
    ci_batcher: GitHubCiBatcher,
    routine_batcher: Option<RoutineDeliveryBatcher>,
    batch_tick: Duration,
}

impl Dispatcher {
    pub fn new(
        rx: mpsc::Receiver<IncomingEvent>,
        router: Router,
        renderer: Box<dyn Renderer>,
        sinks: HashMap<String, Box<dyn Sink>>,
        ci_batch_window: Duration,
        routine_batch_window: Option<Duration>,
    ) -> Self {
        Self {
            rx,
            router,
            renderer,
            sinks,
            ci_batcher: GitHubCiBatcher::new(ci_batch_window),
            routine_batcher: routine_batch_window.map(RoutineDeliveryBatcher::new),
            batch_tick: DEFAULT_BATCH_TICK,
        }
    }

    #[cfg(test)]
    fn with_ci_batch_window(mut self, window: Duration) -> Self {
        self.ci_batcher = GitHubCiBatcher::new(window);
        self
    }

    #[cfg(test)]
    fn with_routine_batch_window(mut self, window: Option<Duration>) -> Self {
        self.routine_batcher = window.map(RoutineDeliveryBatcher::new);
        self
    }

    #[cfg(test)]
    fn with_batch_tick(mut self, tick: Duration) -> Self {
        self.batch_tick = tick;
        self
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut ticker = tokio::time::interval(self.batch_tick);
        loop {
            tokio::select! {
                maybe_event = self.rx.recv() => {
                    match maybe_event {
                        Some(event) => {
                            let now_ms = now_ms();
                            self.flush_due_batches(now_ms).await?;
                            if self.is_ci_event(&event) {
                                for flushed in self.ci_batcher.observe(event, now_ms) {
                                    self.deliver_event(flushed).await;
                                }
                            } else {
                                self.resolve_and_dispatch(event, now_ms).await;
                            }
                        }
                        None => {
                            for event in self.ci_batcher.flush_all() {
                                self.deliver_event(event).await;
                            }
                            self.flush_all_routine_batches().await;
                            break;
                        }
                    }
                }
                _ = ticker.tick() => {
                    self.flush_due_batches(now_ms()).await?;
                }
            }
        }

        Ok(())
    }

    async fn flush_due_batches(&mut self, now_ms: u64) -> Result<()> {
        for event in self.ci_batcher.flush_due(now_ms) {
            self.deliver_event(event).await;
        }
        if let Some(routine_batcher) = self.routine_batcher.as_mut() {
            for batch in routine_batcher.flush_due(now_ms) {
                self.send_routine_batch(batch).await;
            }
        }
        Ok(())
    }

    fn is_ci_event(&self, event: &IncomingEvent) -> bool {
        matches!(
            event.canonical_kind(),
            "github.ci-started" | "github.ci-failed" | "github.ci-passed" | "github.ci-cancelled"
        )
    }

    async fn deliver_event(&self, event: IncomingEvent) {
        let deliveries = match self.router.resolve(&event).await {
            Ok(deliveries) => deliveries,
            Err(error) => {
                eprintln!(
                    "clawhip dispatcher failed to resolve {}: {error}",
                    event.canonical_kind()
                );
                return;
            }
        };

        for delivery in deliveries {
            self.send_delivery(&event, &delivery).await;
        }
    }

    async fn resolve_and_dispatch(&mut self, event: IncomingEvent, now_ms: u64) {
        let deliveries = match self.router.resolve(&event).await {
            Ok(deliveries) => deliveries,
            Err(error) => {
                eprintln!(
                    "clawhip dispatcher failed to resolve {}: {error}",
                    event.canonical_kind()
                );
                return;
            }
        };

        for delivery in deliveries {
            if self.should_batch_routine_delivery(&event, &delivery)
                && let Some(routine_batcher) = self.routine_batcher.as_mut()
            {
                routine_batcher.observe(
                    QueuedRoutineDelivery {
                        event: event.clone(),
                        delivery,
                    },
                    now_ms,
                );
                continue;
            }

            self.send_delivery(&event, &delivery).await;
        }
    }

    async fn send_delivery(&self, event: &IncomingEvent, delivery: &ResolvedDelivery) {
        let Some(sink) = self.sinks.get(delivery.sink.as_str()) else {
            eprintln!(
                "clawhip dispatcher missing sink '{}' for target {:?}",
                delivery.sink, delivery.target
            );
            return;
        };

        let content = match self
            .router
            .render_delivery(event, delivery, self.renderer.as_ref())
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
                return;
            }
        };

        self.send_sink_message(
            sink.as_ref(),
            &delivery.target,
            SinkMessage {
                event_kind: event.canonical_kind().to_string(),
                format: delivery.format.clone(),
                content,
                payload: event.payload.clone(),
            },
        )
        .await;
    }

    async fn send_routine_batch(&self, batch: FlushedRoutineDeliveryBatch) {
        let Some(first) = batch.items.first() else {
            return;
        };
        if batch.items.len() == 1 {
            self.send_delivery(&first.event, &first.delivery).await;
            return;
        }

        let Some(sink) = self.sinks.get(first.delivery.sink.as_str()) else {
            eprintln!(
                "clawhip dispatcher missing sink '{}' for batched target {:?}",
                first.delivery.sink, first.delivery.target
            );
            return;
        };

        let mut contents = Vec::new();
        let mut event_kinds = Vec::new();
        for item in &batch.items {
            match self
                .router
                .render_delivery_body(&item.event, &item.delivery, self.renderer.as_ref())
                .await
            {
                Ok(content) => {
                    contents.push(content);
                    event_kinds.push(item.event.canonical_kind().to_string());
                }
                Err(error) => {
                    eprintln!(
                        "clawhip dispatcher failed to render batched {} for {}/ {:?}: {error}",
                        item.event.canonical_kind(),
                        item.delivery.sink,
                        item.delivery.target
                    );
                }
            }
        }

        if contents.is_empty() {
            return;
        }

        self.send_sink_message(
            sink.as_ref(),
            &first.delivery.target,
            SinkMessage {
                event_kind: "dispatch.routine-batched".to_string(),
                format: first.delivery.format.clone(),
                content: contents.join("\n"),
                payload: json!({
                    "batched": true,
                    "count": contents.len(),
                    "event_kinds": event_kinds,
                }),
            },
        )
        .await;
    }

    async fn send_sink_message(&self, sink: &dyn Sink, target: &SinkTarget, message: SinkMessage) {
        if let Err(error) = sink.send(target, &message).await {
            eprintln!(
                "clawhip dispatcher delivery failed to {:?}: {error}",
                target
            );
        }
    }

    async fn flush_all_routine_batches(&mut self) {
        let Some(routine_batcher) = self.routine_batcher.as_mut() else {
            return;
        };
        for batch in routine_batcher.flush_all() {
            self.send_routine_batch(batch).await;
        }
    }

    fn should_batch_routine_delivery(
        &self,
        event: &IncomingEvent,
        delivery: &ResolvedDelivery,
    ) -> bool {
        self.routine_batcher.is_some()
            && delivery.sink == "discord"
            && !self.is_ci_event(event)
            && !should_bypass_routine_batch(event)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ScheduledBatchKey {
    key: String,
    version: u64,
}

#[derive(Debug, Clone)]
struct GitHubCiBatcher {
    pending: HashMap<String, PendingCiBatch>,
    timer_wheel: TimerWheel,
    window: Duration,
}

#[derive(Debug, Clone)]
struct PendingCiBatch {
    repo: String,
    number: Option<u64>,
    branch: Option<String>,
    sha: String,
    url: String,
    channel: Option<String>,
    mention: Option<String>,
    format: Option<crate::events::MessageFormat>,
    jobs: HashMap<String, BatchedCiJob>,
    expected_jobs: usize,
    run_all_terminal: bool,
    saw_in_progress: bool,
    deliver_at_ms: u64,
    version: u64,
}

#[derive(Debug, Clone, Serialize)]
struct BatchedCiJob {
    workflow: String,
    status: String,
    conclusion: Option<String>,
    url: String,
}

#[derive(Debug, Clone)]
struct RoutineDeliveryBatcher {
    pending: HashMap<String, PendingRoutineDeliveryBatch>,
    timer_wheel: TimerWheel,
    window: Duration,
}

#[derive(Debug, Clone)]
struct PendingRoutineDeliveryBatch {
    items: Vec<QueuedRoutineDelivery>,
    deliver_at_ms: u64,
    version: u64,
}

#[derive(Debug, Clone)]
struct QueuedRoutineDelivery {
    event: IncomingEvent,
    delivery: ResolvedDelivery,
}

#[derive(Debug, Clone)]
struct FlushedRoutineDeliveryBatch {
    items: Vec<QueuedRoutineDelivery>,
}

impl RoutineDeliveryBatcher {
    fn new(window: Duration) -> Self {
        Self {
            pending: HashMap::new(),
            timer_wheel: TimerWheel::new(now_ms()),
            window,
        }
    }

    fn observe(&mut self, delivery: QueuedRoutineDelivery, now_ms: u64) {
        let key = routine_batch_key(&delivery);
        let batch =
            self.pending
                .entry(key.clone())
                .or_insert_with(|| PendingRoutineDeliveryBatch {
                    items: Vec::new(),
                    deliver_at_ms: now_ms + self.window.as_millis() as u64,
                    version: 0,
                });
        batch.items.push(delivery);
        batch.version += 1;
        self.timer_wheel.schedule(DelayedEntry {
            deliver_at_ms: batch.deliver_at_ms,
            record: serde_json::to_vec(&ScheduledBatchKey {
                key,
                version: batch.version,
            })
            .unwrap_or_default(),
        });
    }

    fn flush_due(&mut self, now_ms: u64) -> Vec<FlushedRoutineDeliveryBatch> {
        let mut batches = Vec::new();
        for entry in self.timer_wheel.tick(now_ms) {
            let Some(scheduled) = serde_json::from_slice::<ScheduledBatchKey>(&entry.record).ok()
            else {
                continue;
            };
            let is_current = self
                .pending
                .get(&scheduled.key)
                .map(|batch| batch.version == scheduled.version)
                .unwrap_or(false);
            if is_current && let Some(batch) = self.flush_batch(&scheduled.key) {
                batches.push(batch);
            }
        }
        batches
    }

    fn flush_all(&mut self) -> Vec<FlushedRoutineDeliveryBatch> {
        let keys = self.pending.keys().cloned().collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| self.flush_batch(&key))
            .collect()
    }

    fn flush_batch(&mut self, key: &str) -> Option<FlushedRoutineDeliveryBatch> {
        let batch = self.pending.remove(key)?;
        Some(FlushedRoutineDeliveryBatch { items: batch.items })
    }
}

impl GitHubCiBatcher {
    fn new(window: Duration) -> Self {
        Self {
            pending: HashMap::new(),
            timer_wheel: TimerWheel::new(now_ms()),
            window,
        }
    }

    fn observe(&mut self, event: IncomingEvent, now_ms: u64) -> Vec<IncomingEvent> {
        let key = ci_batch_key(&event.payload);
        let workflow = event
            .payload
            .get("workflow")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let batch = self
            .pending
            .entry(key.clone())
            .or_insert_with(|| PendingCiBatch {
                repo: event.payload["repo"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string(),
                number: event.payload.get("number").and_then(Value::as_u64),
                branch: event
                    .payload
                    .get("branch")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                sha: event.payload["sha"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                url: event.payload["url"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                channel: event.channel.clone(),
                mention: event.mention.clone(),
                format: event.format.clone(),
                jobs: HashMap::new(),
                expected_jobs: ci_run_job_count(&event.payload),
                run_all_terminal: event
                    .payload
                    .get("run_all_terminal")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                saw_in_progress: false,
                deliver_at_ms: now_ms + self.window.as_millis() as u64,
                version: 0,
            });
        batch.repo = event.payload["repo"]
            .as_str()
            .unwrap_or(&batch.repo)
            .to_string();
        batch.number = event
            .payload
            .get("number")
            .and_then(Value::as_u64)
            .or(batch.number);
        batch.branch = event
            .payload
            .get("branch")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or(batch.branch.clone());
        batch.sha = event.payload["sha"]
            .as_str()
            .unwrap_or(&batch.sha)
            .to_string();
        batch.url = event.payload["url"]
            .as_str()
            .unwrap_or(&batch.url)
            .to_string();
        batch.channel = event.channel.clone().or(batch.channel.clone());
        batch.mention = event.mention.clone().or(batch.mention.clone());
        batch.format = event.format.clone().or(batch.format.clone());
        batch.expected_jobs = batch.expected_jobs.max(ci_run_job_count(&event.payload));
        batch.run_all_terminal = event
            .payload
            .get("run_all_terminal")
            .and_then(Value::as_bool)
            .unwrap_or(batch.run_all_terminal);
        batch.version += 1;
        if event.payload["status"].as_str().unwrap_or("unknown") != "completed" {
            batch.saw_in_progress = true;
        }
        batch.jobs.insert(
            workflow.clone(),
            BatchedCiJob {
                workflow,
                status: event.payload["status"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string(),
                conclusion: event
                    .payload
                    .get("conclusion")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                url: event.payload["url"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            },
        );

        let version = batch.version;
        let deliver_at_ms = batch.deliver_at_ms;
        self.timer_wheel.schedule(DelayedEntry {
            deliver_at_ms,
            record: serde_json::to_vec(&ScheduledBatchKey {
                key: key.clone(),
                version,
            })
            .unwrap_or_default(),
        });

        if batch.saw_in_progress
            && batch.run_all_terminal
            && batch.jobs.len() >= batch.expected_jobs
            && batch.jobs.values().all(is_terminal_job)
        {
            return self.flush_batch(&key).into_iter().collect();
        }

        Vec::new()
    }

    fn flush_due(&mut self, now_ms: u64) -> Vec<IncomingEvent> {
        let mut events = Vec::new();
        for entry in self.timer_wheel.tick(now_ms) {
            let Some(scheduled) = serde_json::from_slice::<ScheduledBatchKey>(&entry.record).ok()
            else {
                continue;
            };
            let is_current = self
                .pending
                .get(&scheduled.key)
                .map(|batch| batch.version == scheduled.version)
                .unwrap_or(false);
            if is_current && let Some(event) = self.flush_batch(&scheduled.key) {
                events.push(event);
            }
        }
        events
    }

    fn flush_all(&mut self) -> Vec<IncomingEvent> {
        let keys = self.pending.keys().cloned().collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| self.flush_batch(&key))
            .collect()
    }

    fn flush_batch(&mut self, key: &str) -> Option<IncomingEvent> {
        let batch = self.pending.remove(key)?;
        let mut jobs = batch.jobs.into_values().collect::<Vec<_>>();
        jobs.sort_by(|left, right| left.workflow.cmp(&right.workflow));

        let total_count = batch.expected_jobs.max(jobs.len());
        let passed_count = jobs
            .iter()
            .filter(|job| matches!(job.conclusion.as_deref(), Some("success") | Some("neutral")))
            .count();
        let skipped_count = jobs
            .iter()
            .filter(|job| job.conclusion.as_deref() == Some("skipped"))
            .count();
        let failed_count = jobs.iter().filter(|job| is_failure(job)).count();
        let cancelled_count = jobs
            .iter()
            .filter(|job| job.conclusion.as_deref() == Some("cancelled"))
            .count();
        let kind = if failed_count > 0 {
            "github.ci-failed"
        } else if jobs.iter().all(is_terminal_job) {
            if cancelled_count > 0 && passed_count == 0 && skipped_count == 0 {
                "github.ci-cancelled"
            } else {
                "github.ci-passed"
            }
        } else {
            "github.ci-started"
        };

        let payload = json!({
            "repo": batch.repo,
            "number": batch.number,
            "branch": batch.branch,
            "sha": batch.sha,
            "url": batch.url,
            "batched": true,
            "total_count": total_count,
            "passed_count": passed_count,
            "skipped_count": skipped_count,
            "failed_count": failed_count,
            "cancelled_count": cancelled_count,
            "jobs": jobs,
        });

        Some(IncomingEvent {
            kind: kind.to_string(),
            channel: batch.channel,
            mention: batch.mention,
            format: batch.format,
            template: None,
            payload,
        })
    }
}

fn is_terminal_job(job: &BatchedCiJob) -> bool {
    job.status == "completed"
}

fn is_failure(job: &BatchedCiJob) -> bool {
    matches!(
        job.conclusion.as_deref(),
        Some("failure" | "timed_out" | "startup_failure" | "action_required")
    )
}

fn ci_batch_key(payload: &Value) -> String {
    let repo = payload
        .get("repo")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let number = payload
        .get("number")
        .and_then(Value::as_u64)
        .map(|v| v.to_string())
        .unwrap_or_else(|| "none".into());
    let sha = payload
        .get("sha")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let url = payload
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let run_id = extract_run_id(url).unwrap_or_else(|| url.to_string());
    format!("{repo}:{number}:{sha}:{run_id}")
}

fn extract_run_id(url: &str) -> Option<String> {
    url.split("/actions/runs/")
        .nth(1)
        .and_then(|tail| tail.split('/').next())
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
}

fn ci_run_job_count(payload: &Value) -> usize {
    payload
        .get("run_job_count")
        .and_then(Value::as_u64)
        .map(|count| count as usize)
        .unwrap_or(1)
}

fn should_bypass_routine_batch(event: &IncomingEvent) -> bool {
    let kind = event.canonical_kind();
    kind.ends_with(".failed")
        || kind.ends_with(".blocked")
        || kind == "tmux.stale"
        || kind.starts_with("github.ci-")
}

fn routine_batch_key(queued: &QueuedRoutineDelivery) -> String {
    let delivery = &queued.delivery;
    let mention = normalized_delivery_text(delivery.mention.as_deref());
    let template = normalized_delivery_text(delivery.template.as_deref());
    format!(
        "{}:{}:{}:{}:{}:{}:{}",
        queued.event.canonical_kind(),
        delivery.sink,
        sink_target_key(&delivery.target),
        delivery.format.as_str(),
        mention,
        template,
        delivery.allow_dynamic_tokens
    )
}

fn normalized_delivery_text(value: Option<&str>) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| "-".to_string())
}

fn sink_target_key(target: &SinkTarget) -> String {
    match target {
        SinkTarget::DiscordChannel(channel) => format!("discord-channel:{channel}"),
        SinkTarget::DiscordWebhook(webhook) => format!("discord-webhook:{webhook}"),
        SinkTarget::SlackWebhook(webhook) => format!("slack-webhook:{webhook}"),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
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
        Dispatcher::new(
            rx,
            router,
            Box::new(DefaultRenderer),
            sinks,
            Duration::from_secs(30),
            None,
        )
    }

    async fn spawn_webhook_collector(
        expected_requests: usize,
    ) -> (
        String,
        tokio::sync::mpsc::Receiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, request_rx) = tokio::sync::mpsc::channel(expected_requests);
        let handle = tokio::spawn(async move {
            for _ in 0..expected_requests {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0_u8; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                request_tx
                    .send(String::from_utf8_lossy(&buf[..n]).to_string())
                    .await
                    .unwrap();
                // connection: close prevents reqwest's default keep-alive
                // from reusing the TCP stream. The collector calls accept()
                // per request, so pooling the connection causes the 2nd
                // request under load to hit a dead stream and the collector
                // to hang on accept() forever (flake root-cause, see #194).
                stream
                    .write_all(
                        b"HTTP/1.1 204 No Content\r\nconnection: close\r\ncontent-length: 0\r\n\r\n",
                    )
                    .await
                    .unwrap();
            }
        });

        (format!("http://{addr}/webhook"), request_rx, handle)
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

    #[tokio::test]
    async fn dispatcher_batches_ci_events_into_single_delivery() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::time::{Duration, timeout};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..1 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = vec![0_u8; 4096];
                let n = stream.read(&mut buf).await.unwrap();
                requests.push(String::from_utf8_lossy(&buf[..n]).to_string());
                stream
                    .write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n")
                    .await
                    .unwrap();
            }
            requests
        });

        let config = AppConfig {
            routes: vec![RouteRule {
                event: "github.ci-*".into(),
                sink: "discord".into(),
                webhook: Some(format!("http://{addr}/webhook")),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };
        let (tx, rx) = mpsc::channel(4);
        let router = Router::new(Arc::new(config));
        let mut dispatcher = test_dispatcher(rx, router)
            .with_ci_batch_window(Duration::from_millis(20))
            .with_batch_tick(Duration::from_millis(5));
        let task = tokio::spawn(async move { dispatcher.run().await.unwrap() });

        for workflow in ["Build", "Test"] {
            let mut event = IncomingEvent::github_ci(
                "github.ci-passed",
                "clawhip".into(),
                Some(85),
                workflow.into(),
                "completed".into(),
                Some("success".into()),
                "abcdef1234567".into(),
                format!("https://github.com/Yeachan-Heo/clawhip/actions/runs/123/jobs/{workflow}"),
                Some("feat/retry".into()),
                None,
            );
            event.payload["run_job_count"] = json!(2);
            event.payload["run_all_terminal"] = json!(true);
            tx.send(event).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        drop(tx);
        task.await.unwrap();

        let requests = timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("2/2 passed"));
        assert!(requests[0].contains("Build, Test"));
    }

    #[tokio::test]
    async fn dispatcher_batches_routine_discord_deliveries_into_single_send() {
        use tokio::time::{Duration, timeout};

        let (webhook, mut requests, server) = spawn_webhook_collector(1).await;
        let config = AppConfig {
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                sink: "discord".into(),
                webhook: Some(webhook),
                mention: Some("<@ops>".into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };
        let (tx, rx) = mpsc::channel(4);
        let router = Router::new(Arc::new(config));
        let mut dispatcher = test_dispatcher(rx, router)
            .with_routine_batch_window(Some(Duration::from_millis(20)))
            .with_batch_tick(Duration::from_millis(5));
        let task = tokio::spawn(async move { dispatcher.run().await.unwrap() });

        tx.send(IncomingEvent::tmux_keyword(
            "issue-122".into(),
            "error".into(),
            "first".into(),
            None,
        ))
        .await
        .unwrap();
        tx.send(IncomingEvent::tmux_keyword(
            "issue-122".into(),
            "warn".into(),
            "second".into(),
            None,
        ))
        .await
        .unwrap();

        let request = timeout(Duration::from_secs(2), requests.recv())
            .await
            .unwrap()
            .unwrap();
        drop(tx);
        task.await.unwrap();
        server.await.unwrap();

        assert!(request.contains("tmux:issue-122 matched 'error' => first"));
        assert!(request.contains("tmux:issue-122 matched 'warn' => second"));
        assert!(!request.contains("<@ops>"));
    }

    #[tokio::test]
    async fn dispatcher_flushes_single_routine_delivery_on_shutdown_and_preserves_mention() {
        use tokio::time::{Duration, timeout};

        let (webhook, mut requests, server) = spawn_webhook_collector(1).await;
        let config = AppConfig {
            routes: vec![RouteRule {
                event: "tmux.keyword".into(),
                sink: "discord".into(),
                webhook: Some(webhook),
                mention: Some("<@ops>".into()),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };
        let (tx, rx) = mpsc::channel(1);
        let router = Router::new(Arc::new(config));
        let mut dispatcher = test_dispatcher(rx, router)
            .with_routine_batch_window(Some(Duration::from_secs(30)))
            .with_batch_tick(Duration::from_millis(5));
        let task = tokio::spawn(async move { dispatcher.run().await.unwrap() });

        tx.send(IncomingEvent::tmux_keyword(
            "issue-122".into(),
            "error".into(),
            "only".into(),
            None,
        ))
        .await
        .unwrap();
        drop(tx);

        let request = timeout(Duration::from_secs(2), requests.recv())
            .await
            .unwrap()
            .unwrap();
        task.await.unwrap();
        server.await.unwrap();

        assert!(request.contains("<@ops> tmux:issue-122 matched 'error' => only"));
    }

    #[tokio::test]
    async fn dispatcher_sends_bypass_events_immediately_while_routine_delivery_waits() {
        use tokio::time::{Duration, timeout};

        // Regression for #196: the prior version used an 80ms routine batch
        // window and a 30ms negative wait. Under CI load the bypass HTTP
        // delivery could take long enough that the 30ms "no second request"
        // check overlapped the 80ms batch-window expiry, letting the routine
        // event escape the batcher mid-assertion. We now use the same
        // stable pattern as the other batching tests in this file: a 30s
        // batch window so the batcher cannot time out during the test, and
        // an explicit `drop(tx)` shutdown to trigger flush_all_routine_batches.
        let (webhook, mut requests, server) = spawn_webhook_collector(2).await;
        let config = AppConfig {
            routes: vec![
                RouteRule {
                    event: "tmux.keyword".into(),
                    sink: "discord".into(),
                    webhook: Some(webhook.clone()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "agent.failed".into(),
                    sink: "discord".into(),
                    webhook: Some(webhook),
                    ..RouteRule::default()
                },
            ],
            ..AppConfig::default()
        };
        let (tx, rx) = mpsc::channel(4);
        let router = Router::new(Arc::new(config));
        let mut dispatcher = test_dispatcher(rx, router)
            .with_routine_batch_window(Some(Duration::from_secs(30)))
            .with_batch_tick(Duration::from_millis(5));
        let task = tokio::spawn(async move { dispatcher.run().await.unwrap() });

        tx.send(IncomingEvent::tmux_keyword(
            "issue-122".into(),
            "error".into(),
            "queued".into(),
            None,
        ))
        .await
        .unwrap();
        tx.send(IncomingEvent::agent_failed(
            "codex".into(),
            Some("session-1".into()),
            Some("clawhip".into()),
            Some(3),
            Some("boom".into()),
            "stacktrace".into(),
            None,
            None,
        ))
        .await
        .unwrap();

        // Bypass delivery (agent.failed) must arrive first, even though it
        // was enqueued second — the routine delivery (tmux.keyword) is still
        // held by the 30s batch window.
        let first = timeout(Duration::from_secs(2), requests.recv())
            .await
            .expect("bypass delivery should arrive promptly")
            .expect("webhook collector closed before bypass delivery");
        assert!(first.contains("agent codex"));
        assert!(first.contains("failed"));

        // A short negative wait confirms the routine delivery is still in
        // the batcher rather than already in-flight. With a 30s batch
        // window this is deterministic: the batcher cannot expire during
        // the wait, so any second request would indicate a bypass leak.
        assert!(
            timeout(Duration::from_millis(50), requests.recv())
                .await
                .is_err(),
            "routine delivery escaped batch before shutdown flush"
        );

        // Shutdown (channel close) triggers flush_all_routine_batches, which
        // releases the held tmux.keyword delivery regardless of batch window.
        drop(tx);
        let second = timeout(Duration::from_secs(2), requests.recv())
            .await
            .expect("routine delivery should flush on shutdown")
            .expect("webhook collector closed before routine flush");
        task.await.unwrap();
        server.await.unwrap();

        assert!(second.contains("tmux:issue-122 matched 'error' => queued"));
    }

    #[tokio::test]
    async fn dispatcher_keeps_ci_events_off_routine_batcher() {
        use tokio::time::{Duration, timeout};

        let (webhook, mut requests, server) = spawn_webhook_collector(1).await;
        let config = AppConfig {
            routes: vec![RouteRule {
                event: "github.ci-*".into(),
                sink: "discord".into(),
                webhook: Some(webhook),
                ..RouteRule::default()
            }],
            ..AppConfig::default()
        };
        let (tx, rx) = mpsc::channel(4);
        let router = Router::new(Arc::new(config));
        let mut dispatcher = test_dispatcher(rx, router)
            .with_ci_batch_window(Duration::from_millis(20))
            .with_routine_batch_window(Some(Duration::from_millis(200)))
            .with_batch_tick(Duration::from_millis(5));
        let task = tokio::spawn(async move { dispatcher.run().await.unwrap() });

        for workflow in ["Build", "Test"] {
            let mut event = IncomingEvent::github_ci(
                "github.ci-passed",
                "clawhip".into(),
                Some(122),
                workflow.into(),
                "completed".into(),
                Some("success".into()),
                "abcdef1234567".into(),
                format!("https://github.com/Yeachan-Heo/clawhip/actions/runs/456/jobs/{workflow}"),
                Some("feat/routine-batch".into()),
                None,
            );
            event.payload["run_job_count"] = json!(2);
            event.payload["run_all_terminal"] = json!(true);
            tx.send(event).await.unwrap();
        }

        let request = timeout(Duration::from_millis(250), requests.recv())
            .await
            .unwrap()
            .unwrap();
        drop(tx);
        task.await.unwrap();
        server.await.unwrap();

        assert!(request.contains("2/2 passed"));
    }

    #[tokio::test]
    async fn dispatcher_keeps_distinct_delivery_signatures_in_separate_batches() {
        use tokio::time::{Duration, timeout};

        let (webhook, mut requests, server) = spawn_webhook_collector(2).await;
        let config = AppConfig {
            routes: vec![
                RouteRule {
                    event: "tmux.keyword".into(),
                    sink: "discord".into(),
                    webhook: Some(webhook.clone()),
                    template: Some("first".into()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "tmux.keyword".into(),
                    sink: "discord".into(),
                    webhook: Some(webhook),
                    template: Some("second".into()),
                    ..RouteRule::default()
                },
            ],
            ..AppConfig::default()
        };
        let (tx, rx) = mpsc::channel(1);
        let router = Router::new(Arc::new(config));
        let mut dispatcher = test_dispatcher(rx, router)
            .with_routine_batch_window(Some(Duration::from_secs(30)))
            .with_batch_tick(Duration::from_millis(5));
        let task = tokio::spawn(async move { dispatcher.run().await.unwrap() });

        tx.send(IncomingEvent::tmux_keyword(
            "issue-122".into(),
            "error".into(),
            "boom".into(),
            None,
        ))
        .await
        .unwrap();
        drop(tx);

        let first = timeout(Duration::from_secs(2), requests.recv())
            .await
            .unwrap()
            .unwrap();
        let second = timeout(Duration::from_secs(2), requests.recv())
            .await
            .unwrap()
            .unwrap();
        task.await.unwrap();
        server.await.unwrap();

        assert!(
            first.contains("\"content\":\"first\"") || second.contains("\"content\":\"first\"")
        );
        assert!(
            first.contains("\"content\":\"second\"") || second.contains("\"content\":\"second\"")
        );
    }

    #[tokio::test]
    async fn dispatcher_keeps_distinct_event_kinds_in_separate_routine_batches() {
        use tokio::time::{Duration, timeout};

        let (webhook, mut requests, server) = spawn_webhook_collector(2).await;
        let config = AppConfig {
            routes: vec![
                RouteRule {
                    event: "tmux.keyword".into(),
                    sink: "discord".into(),
                    webhook: Some(webhook.clone()),
                    ..RouteRule::default()
                },
                RouteRule {
                    event: "git.commit".into(),
                    sink: "discord".into(),
                    webhook: Some(webhook),
                    ..RouteRule::default()
                },
            ],
            ..AppConfig::default()
        };
        let (tx, rx) = mpsc::channel(2);
        let router = Router::new(Arc::new(config));
        let mut dispatcher = test_dispatcher(rx, router)
            .with_routine_batch_window(Some(Duration::from_millis(20)))
            .with_batch_tick(Duration::from_millis(5));
        let task = tokio::spawn(async move { dispatcher.run().await.unwrap() });

        tx.send(IncomingEvent::tmux_keyword(
            "issue-132".into(),
            "error".into(),
            "boom".into(),
            None,
        ))
        .await
        .unwrap();
        tx.send(IncomingEvent::git_commit(
            "clawhip".into(),
            "main".into(),
            "1234567890abcdef".into(),
            "ship it".into(),
            None,
        ))
        .await
        .unwrap();

        let first = timeout(Duration::from_secs(2), requests.recv())
            .await
            .unwrap()
            .unwrap();
        let second = timeout(Duration::from_secs(2), requests.recv())
            .await
            .unwrap()
            .unwrap();
        drop(tx);
        task.await.unwrap();
        server.await.unwrap();

        assert!(
            first.contains("tmux:issue-132 matched 'error' => boom")
                || second.contains("tmux:issue-132 matched 'error' => boom")
        );
        assert!(
            first.contains("git:clawhip@main 1234567 ship it")
                || second.contains("git:clawhip@main 1234567 ship it")
        );
        assert!(
            (first.contains("tmux:issue-132 matched 'error' => boom")
                && !first.contains("git:clawhip@main 1234567 ship it"))
                || (second.contains("tmux:issue-132 matched 'error' => boom")
                    && !second.contains("git:clawhip@main 1234567 ship it"))
        );
        assert!(
            (first.contains("git:clawhip@main 1234567 ship it")
                && !first.contains("tmux:issue-132 matched 'error' => boom"))
                || (second.contains("git:clawhip@main 1234567 ship it")
                    && !second.contains("tmux:issue-132 matched 'error' => boom"))
        );
    }

    #[test]
    fn batch_key_prefers_workflow_run_id() {
        let payload = json!({
            "repo": "clawhip",
            "number": 86,
            "sha": "abc",
            "url": "https://github.com/org/repo/actions/runs/123456789/jobs/42"
        });
        assert_eq!(ci_batch_key(&payload), "clawhip:86:abc:123456789");
    }

    #[test]
    fn dispatcher_uses_provided_ci_batch_window() {
        let (_tx, rx) = mpsc::channel(1);
        let router = Router::new(Arc::new(AppConfig::default()));
        let dispatcher = Dispatcher::new(
            rx,
            router,
            Box::new(DefaultRenderer),
            HashMap::new(),
            Duration::from_secs(90),
            None,
        );

        assert_eq!(dispatcher.ci_batcher.window, Duration::from_secs(90));
    }

    #[test]
    fn batcher_flushes_when_all_jobs_for_run_are_terminal() {
        let mut batcher = GitHubCiBatcher::new(Duration::from_secs(30));

        let mut first = IncomingEvent::github_ci(
            "github.ci-started",
            "clawhip".into(),
            Some(86),
            "Build".into(),
            "in_progress".into(),
            None,
            "abc".into(),
            "https://github.com/org/repo/actions/runs/123/jobs/1".into(),
            Some("feat/batch".into()),
            None,
        );
        first.payload["run_job_count"] = json!(2);
        first.payload["run_all_terminal"] = json!(false);
        assert!(batcher.observe(first, now_ms()).is_empty());

        let mut second = IncomingEvent::github_ci(
            "github.ci-passed",
            "clawhip".into(),
            Some(86),
            "Build".into(),
            "completed".into(),
            Some("success".into()),
            "abc".into(),
            "https://github.com/org/repo/actions/runs/123/jobs/1".into(),
            Some("feat/batch".into()),
            None,
        );
        second.payload["run_job_count"] = json!(2);
        second.payload["run_all_terminal"] = json!(true);
        assert!(batcher.observe(second, now_ms()).is_empty());

        let mut third = IncomingEvent::github_ci(
            "github.ci-failed",
            "clawhip".into(),
            Some(86),
            "Test".into(),
            "completed".into(),
            Some("failure".into()),
            "abc".into(),
            "https://github.com/org/repo/actions/runs/123/jobs/2".into(),
            Some("feat/batch".into()),
            None,
        );
        third.payload["run_job_count"] = json!(2);
        third.payload["run_all_terminal"] = json!(true);
        let flushed = batcher.observe(third, now_ms());
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].canonical_kind(), "github.ci-failed");
        assert_eq!(flushed[0].payload["total_count"], json!(2));
    }

    #[test]
    fn dispatcher_uses_configured_ci_batch_window_from_app_config() {
        let (_tx, rx) = mpsc::channel(1);
        let router = Router::new(Arc::new(AppConfig::default()));
        let mut sinks: HashMap<String, Box<dyn Sink>> = HashMap::new();
        sinks.insert(
            "discord".into(),
            Box::new(DiscordSink::from_config(Arc::new(AppConfig::default())).unwrap()),
        );
        sinks.insert("slack".into(), Box::new(SlackSink::default()));

        let config = AppConfig {
            dispatch: crate::config::DispatchConfig {
                ci_batch_window_secs: 90,
                routine_batch_window_secs: 5,
            },
            ..AppConfig::default()
        };

        let dispatcher = Dispatcher::new(
            rx,
            router,
            Box::new(DefaultRenderer),
            sinks,
            Duration::from_secs(config.dispatch.ci_batch_window_secs),
            config.dispatch.routine_batch_window(),
        );

        assert_eq!(
            dispatcher.ci_batcher.window,
            Duration::from_secs(config.dispatch.ci_batch_window_secs)
        );
    }
}
