//! Opt-in, authenticated unread notification polling. Never mutates GitHub threads.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use reqwest::header::{DATE, IF_MODIFIED_SINCE, LAST_MODIFIED, LINK};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::{Rfc2822, Rfc3339};
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::Result;
use crate::config::AppConfig;
use crate::event::GitHubNotificationEvent;
use crate::events::IncomingEvent;
use crate::source::Source;
use crate::source::github::{
    GitHubClient, build_github_client, github_request, resolve_monitor_github_token,
};
use crate::telemetry;

// Bounds constructed requests; HTTP redirects retain the shared client's policy.
const MAX_PAGES: usize = 5;
const MAX_THREADS: usize = 1000;

pub struct GitHubNotificationsSource {
    config: Arc<AppConfig>,
}

impl GitHubNotificationsSource {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }
}

#[async_trait::async_trait]
impl Source for GitHubNotificationsSource {
    fn name(&self) -> &str {
        "github_notifications"
    }

    async fn run(&self, tx: mpsc::Sender<IncomingEvent>) -> Result<()> {
        if !self.config.monitors.github_notifications.enabled {
            return Ok(());
        }
        let mut state = MonitorState::default();
        loop {
            // Retry unavailable credentials rather than silently running unauthenticated.
            let (token, auth) = resolve_monitor_github_token(&self.config).await;
            if let Some(token) = token {
                let client = build_github_client(Some(token))?;
                loop {
                    if let Err(error) = poll_once(&self.config, &client, &tx, &mut state).await {
                        eprintln!("clawhip source github_notifications: {error}");
                    }
                    sleep(
                        state
                            .interval(self.config.monitors.github_notifications.poll_interval_secs),
                    )
                    .await;
                }
            }
            state.degraded(
                auth.error
                    .unwrap_or("GitHub notifications authentication unavailable"),
            );
            sleep(state.interval(self.config.monitors.github_notifications.poll_interval_secs))
                .await;
        }
    }
}

#[derive(Default)]
struct MonitorState {
    cutoff: Option<OffsetDateTime>,
    watermark: Option<OffsetDateTime>,
    catch_up: Option<CatchUp>,
    last_modified: Option<String>,
    threads: HashMap<String, OffsetDateTime>,
    order: VecDeque<String>,
    server_interval_secs: u64,
    last_degraded: Option<Instant>,
}

impl MonitorState {
    fn interval(&self, configured: u64) -> Duration {
        Duration::from_secs(configured.max(1).max(self.server_interval_secs))
    }

    fn remember(&mut self, id: &str, updated: OffsetDateTime) {
        if let Some(previous) = self.threads.get_mut(id) {
            *previous = (*previous).max(updated);
            return;
        }
        if self.order.len() == MAX_THREADS
            && let Some(oldest) = self.order.pop_front()
        {
            self.threads.remove(&oldest);
        }
        self.order.push_back(id.to_string());
        self.threads.insert(id.to_string(), updated);
    }

    fn degraded(&mut self, error: &str) {
        if self
            .last_degraded
            .is_some_and(|last| last.elapsed() < Duration::from_secs(60))
        {
            return;
        }
        self.last_degraded = Some(Instant::now());
        let mut record = telemetry::record(
            telemetry::event_name::SOURCE_DEGRADED,
            "source_poll_failed",
            "source:github_notifications",
        );
        record.insert("source".into(), serde_json::json!("github_notifications"));
        record.insert("error".into(), serde_json::json!(error));
        telemetry::emit(record);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CatchUp {
    since: OffsetDateTime,
    before: OffsetDateTime,
    poll_date: OffsetDateTime,
    // Resume within an unchanged boundary second when it fills a whole budget.
    page: u64,
}

#[derive(Deserialize)]
struct Notification {
    id: String,
    reason: String,
    updated_at: String,
    repository: Repository,
    subject: Subject,
}

#[derive(Deserialize)]
struct Repository {
    full_name: String,
}

#[derive(Deserialize)]
struct Subject {
    title: String,
    #[serde(rename = "type")]
    kind: String,
    url: Option<String>,
    latest_comment_url: Option<String>,
}

struct PollResult {
    threads: Vec<(Notification, OffsetDateTime)>,
    date: Option<OffsetDateTime>,
    last_modified: Option<String>,
    watermark: Option<OffsetDateTime>,
    catch_up: Option<CatchUp>,
}

async fn poll_once(
    config: &AppConfig,
    client: &GitHubClient,
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut MonitorState,
) -> Result<()> {
    if !config.monitors.github_notifications.enabled {
        return Ok(());
    }
    let result = match fetch_threads(config, client, state).await {
        Ok(Some(result)) => result,
        Ok(None) => return Ok(()), // 304 is a successful poll, not a degradation.
        Err(error) => {
            state.degraded(&error.to_string());
            return Err(error);
        }
    };
    let cold = state.cutoff.is_none();
    let date = result.date.ok_or_else(|| {
        state.degraded("GitHub notifications response missing a valid Date header");
        "GitHub notifications response missing a valid Date header"
    })?;
    let cutoff = state.cutoff.unwrap_or(date);
    let monitor = &config.monitors.github_notifications;
    for (thread, updated) in result.threads {
        let newer = state
            .threads
            .get(&thread.id)
            .is_none_or(|previous| updated > *previous);
        let allowed = (monitor.reasons.is_empty() || monitor.reasons.contains(&thread.reason))
            && (monitor.repos.is_empty()
                || monitor
                    .repos
                    .iter()
                    .any(|repo| repo.eq_ignore_ascii_case(&thread.repository.full_name)));
        if !cold && newer && updated >= cutoff && allowed {
            let (number, url) = subject_link(&thread, &config.monitors.github_api_base);
            let event = IncomingEvent::github_notification(
                GitHubNotificationEvent {
                    repo: thread.repository.full_name.clone(),
                    thread_id: thread.id.clone(),
                    reason: thread.reason.clone(),
                    subject_type: thread.subject.kind.clone(),
                    title: thread.subject.title.clone(),
                    number,
                    url,
                    updated_at: thread.updated_at.clone(),
                },
                monitor.channel.clone(),
            )
            .with_mention(monitor.mention.clone())
            .with_format(monitor.format.clone());
            tx.send(event)
                .await
                .map_err(|error| format!("github_notifications source channel closed: {error}"))?;
        }
        state.remember(&thread.id, updated);
    }
    state.cutoff = Some(cutoff);
    state.watermark = result.watermark;
    state.catch_up = result.catch_up;
    if state.catch_up.is_some() {
        state.last_modified = None;
        state.degraded("GitHub notifications pagination cap reached");
    } else {
        state.last_modified = result.last_modified;
    }
    Ok(())
}

async fn fetch_threads(
    config: &AppConfig,
    client: &GitHubClient,
    state: &mut MonitorState,
) -> Result<Option<PollResult>> {
    let endpoint = reqwest::Url::parse(&format!(
        "{}/notifications",
        config.monitors.github_api_base.trim_end_matches('/')
    ))?;
    let catch_up = state.catch_up.clone();
    let mut page = catch_up.as_ref().map_or(1, |cursor| cursor.page);
    let mut oldest = None;
    let mut result = PollResult {
        threads: Vec::new(),
        date: None,
        last_modified: None,
        watermark: state.watermark,
        catch_up: None,
    };
    for index in 0..MAX_PAGES {
        // A fresh head costs one request; the remaining budget drains the saved window.
        // Using its original poll Date avoids repeatedly scanning the whole backlog here.
        let fresh_head = catch_up.is_some() && index == 0;
        let since = if fresh_head {
            catch_up.as_ref().map(|cursor| cursor.poll_date)
        } else {
            catch_up
                .as_ref()
                .map(|cursor| cursor.since)
                .or(state.watermark)
        };
        let participating = config.monitors.github_notifications.participating;
        let mut request = client.http.get(endpoint.clone()).query(&[
            ("all", "false".to_string()),
            ("participating", participating.to_string()),
            ("per_page", "50".to_string()),
            ("page", if fresh_head { 1 } else { page }.to_string()),
        ]);
        if let Some(since) = since {
            request = request.query(&[(
                "since",
                (since - time::Duration::seconds(1)).format(&Rfc3339)?,
            )]);
        }
        if !fresh_head && let Some(cursor) = &catch_up {
            request = request.query(&[("before", cursor.before.format(&Rfc3339)?)]);
        }
        if index == 0
            && catch_up.is_none()
            && let Some(modified) = &state.last_modified
        {
            request = request.header(IF_MODIFIED_SINCE, modified);
        }
        let response = github_request(client, request, "notifications").await?;
        if let Some(interval) = response
            .headers()
            .get("x-poll-interval")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
        {
            state.server_interval_secs = interval;
        }
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            if index == 0 && catch_up.is_none() {
                return Ok(None);
            }
            return Err("unexpected 304 on GitHub notifications pagination".into());
        }
        if !response.status().is_success() {
            return Err(format!(
                "GitHub notifications request failed with {}",
                response.status()
            )
            .into());
        }
        if index == 0 {
            result.date = response
                .headers()
                .get(DATE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| OffsetDateTime::parse(value, &Rfc2822).ok());
            result.last_modified = response
                .headers()
                .get(LAST_MODIFIED)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
        }
        let next = next_page(response.headers(), &endpoint)?;
        let threads: Vec<Notification> = response
            .json()
            .await
            .context("decoding GitHub notification threads")?;
        for thread in threads {
            let updated = OffsetDateTime::parse(&thread.updated_at, &Rfc3339)
                .context("invalid GitHub notification updated_at")?;
            if !fresh_head {
                oldest =
                    Some(oldest.map_or(updated, |previous: OffsetDateTime| previous.min(updated)));
            }
            result.threads.push((thread, updated));
        }
        if fresh_head {
            continue;
        }
        let Some(next) = next else {
            result.watermark = catch_up
                .as_ref()
                .map(|cursor| cursor.poll_date)
                .or(result.date);
            // The head may have more pages newer than poll_date. Do not let its
            // Last-Modified hide them after the saved backlog has drained.
            if catch_up.is_some() {
                result.last_modified = None;
            }
            return Ok(Some(result));
        };
        if next <= page {
            return Err("non-advancing GitHub notifications pagination link".into());
        }
        page = next;
    }
    let date = result
        .date
        .ok_or("GitHub notifications response missing a valid Date header")?;
    let before = oldest.ok_or("GitHub notifications pagination cap without threads")?
        + time::Duration::seconds(1);
    result.catch_up = Some(match catch_up {
        Some(mut cursor) => {
            if before < cursor.before {
                cursor.before = before;
                cursor.page = 1;
            } else {
                // All pages can share the boundary second. Keep that inclusive
                // boundary and resume its next page instead of skipping the tie.
                cursor.page = page;
            }
            cursor
        }
        None => CatchUp {
            since: state.watermark.unwrap_or(date),
            before,
            poll_date: date,
            page: 1,
        },
    });
    result.watermark = Some(state.watermark.unwrap_or(date));
    Ok(Some(result))
}

fn next_page(headers: &reqwest::header::HeaderMap, endpoint: &reqwest::Url) -> Result<Option<u64>> {
    for header in headers.get_all(LINK) {
        for link in header.to_str()?.split(',') {
            let mut parts = link.trim().split(';');
            let target = parts.next().unwrap_or_default().trim();
            if !parts.any(|part| part.trim() == "rel=\"next\"") {
                continue;
            }
            let url = reqwest::Url::parse(target.trim_start_matches('<').trim_end_matches('>'))?;
            if url.origin() != endpoint.origin()
                || url.path() != endpoint.path()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err("unsafe GitHub notifications pagination link".into());
            }
            let page = url
                .query_pairs()
                .find(|(key, _)| key == "page")
                .and_then(|(_, value)| value.parse::<u64>().ok())
                .ok_or("GitHub notifications next link missing page")?;
            return Ok(Some(page));
        }
    }
    Ok(None)
}

fn subject_link(thread: &Notification, api_base: &str) -> (Option<u64>, String) {
    let repo = &thread.repository.full_name;
    let subject_is_github = thread
        .subject
        .url
        .as_deref()
        .and_then(|url| reqwest::Url::parse(url).ok())
        .is_some_and(|url| url.host_str() == Some("api.github.com"));
    let web_base = reqwest::Url::parse(api_base)
        .ok()
        .filter(|_| !subject_is_github)
        .filter(|url| url.host_str() != Some("api.github.com"))
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|| "https://github.com".into());
    let fallback = format!("{web_base}/{repo}");
    let Some(url) = thread
        .subject
        .url
        .as_deref()
        .and_then(|url| reqwest::Url::parse(url).ok())
    else {
        return (None, fallback);
    };
    let prefix = format!("/repos/{repo}/");
    let path = url.path().strip_prefix("/api/v3").unwrap_or(url.path());
    let Some(tail) = path.strip_prefix(&prefix) else {
        return (None, fallback);
    };
    let parts: Vec<_> = tail.split('/').collect();
    let (number, mut html) = match parts.as_slice() {
        ["pulls", number] | ["issues", number] | ["discussions", number] => {
            let Some(number) = number.parse::<u64>().ok() else {
                return (None, fallback);
            };
            let category = if parts[0] == "pulls" {
                "pull"
            } else {
                parts[0]
            };
            (Some(number), format!("{fallback}/{category}/{number}"))
        }
        ["commits", sha] => (None, format!("{fallback}/commit/{sha}")),
        _ => (None, fallback),
    };
    if number.is_some()
        && let Some(comment) = thread
            .subject
            .latest_comment_url
            .as_deref()
            .and_then(|url| reqwest::Url::parse(url).ok())
    {
        let path = comment
            .path()
            .strip_prefix("/api/v3")
            .unwrap_or(comment.path());
        if let Some(tail) = path.strip_prefix(&prefix) {
            let parts: Vec<_> = tail.split('/').collect();
            if let [category, "comments", id] = parts.as_slice()
                && id.parse::<u64>().is_ok()
            {
                match *category {
                    "issues" => html.push_str(&format!("#issuecomment-{id}")),
                    "pulls" => html.push_str(&format!("#discussion_r{id}")),
                    _ => {}
                }
            }
        }
    }
    (number, html)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const DATE_VALUE: &str = "Thu, 15 Oct 2026 00:00:00 GMT";
    const OLD: &str = "2026-10-14T23:59:00Z";
    const NEW: &str = "2026-10-15T00:00:01Z";
    const NEWER: &str = "2026-10-15T00:00:02Z";

    fn thread(id: &str, updated: &str) -> Value {
        json!({
            "id": id, "reason": "review_requested", "updated_at": updated,
            "repository": {"full_name": "owner/repo", "private": true},
            "subject": {"title": "A PR", "type": "PullRequest",
                "url": "https://api.github.com/repos/owner/repo/pulls/12",
                "latest_comment_url": "https://api.github.com/repos/owner/repo/issues/comments/123"},
            "body": "never forward notification extras"
        })
    }

    struct Reply {
        status: u16,
        body: String,
        headers: Vec<(String, String)>,
    }

    impl Reply {
        fn ok(threads: Value) -> Self {
            Self {
                status: 200,
                body: threads.to_string(),
                headers: vec![
                    ("Date".into(), DATE_VALUE.into()),
                    ("Last-Modified".into(), DATE_VALUE.into()),
                ],
            }
        }

        fn status(status: u16) -> Self {
            Self {
                status,
                body: String::new(),
                headers: Vec::new(),
            }
        }
    }

    struct Server {
        base: String,
        requests: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn server(replies: Vec<Reply>) -> Server {
        let mut replies = replies.into_iter();
        server_with(move |_| replies.next()).await
    }

    async fn server_with(
        mut reply_for: impl FnMut(&str) -> Option<Reply> + Send + 'static,
    ) -> Server {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let server_base = base.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..n]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(request).unwrap();
                captured.lock().unwrap().push(request.clone());
                let Some(reply) = reply_for(&request) else {
                    break;
                };
                let headers: String = reply
                    .headers
                    .iter()
                    .map(|(key, value)| {
                        format!("{key}: {}\r\n", value.replace("BASE", &server_base))
                    })
                    .collect();
                let response = format!(
                    "HTTP/1.1 {} Test\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{}\r\n{}",
                    reply.status,
                    reply.body.len(),
                    headers,
                    reply.body,
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Server {
            base,
            requests,
            task,
        }
    }

    fn config(base: &str) -> AppConfig {
        let mut config = AppConfig::default();
        config.monitors.github_api_base = base.into();
        config.monitors.github_notifications.enabled = true;
        config.monitors.github_notifications.channel = Some("notifications".into());
        config.monitors.github_notifications.mention = Some("<@123>".into());
        config.monitors.github_notifications.format = Some(crate::events::MessageFormat::Inline);
        config
    }

    struct Inbox {
        threads: Vec<Value>,
        date: OffsetDateTime,
        requests: usize,
        fail_at: Option<usize>,
    }

    async fn inbox_server() -> (Server, Arc<Mutex<Inbox>>) {
        let inbox = Arc::new(Mutex::new(Inbox {
            threads: Vec::new(),
            date: OffsetDateTime::parse(DATE_VALUE, &Rfc2822).unwrap(),
            requests: 0,
            fail_at: None,
        }));
        let captured = inbox.clone();
        let server = server_with(move |request| {
            let mut inbox = captured.lock().unwrap();
            inbox.requests += 1;
            if inbox.fail_at == Some(inbox.requests) {
                return Some(Reply::status(500));
            }
            let date = inbox.date.format(&Rfc2822).unwrap();
            if request.contains(&format!("if-modified-since: {date}\r\n")) {
                return Some(Reply::status(304));
            }
            let path = request.split_whitespace().nth(1).unwrap();
            let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
            let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(url.path(), "/notifications");
            assert!(request.starts_with("GET "));
            assert_eq!(query["all"], "false");
            let since = query
                .get("since")
                .map(|value| OffsetDateTime::parse(value, &Rfc3339).unwrap());
            let before = query
                .get("before")
                .map(|value| OffsetDateTime::parse(value, &Rfc3339).unwrap());
            let page: usize = query["page"].parse().unwrap();
            let per_page: usize = query["per_page"].parse().unwrap();
            let mut threads: Vec<_> = inbox
                .threads
                .iter()
                .filter_map(|thread| {
                    let updated =
                        OffsetDateTime::parse(thread["updated_at"].as_str().unwrap(), &Rfc3339)
                            .unwrap();
                    (since.is_none_or(|since| updated > since)
                        && before.is_none_or(|before| updated < before))
                    .then_some((updated, thread.clone()))
                })
                .collect();
            threads.sort_by_key(|(updated, _)| std::cmp::Reverse(*updated));
            let mut reply = Reply::ok(json!(
                threads
                    .iter()
                    .skip((page - 1) * per_page)
                    .take(per_page)
                    .map(|(_, thread)| thread.clone())
                    .collect::<Vec<_>>()
            ));
            reply.headers = vec![
                ("Date".into(), date.clone()),
                ("Last-Modified".into(), date),
            ];
            if threads.len() > page * per_page {
                reply.headers.push((
                    "Link".into(),
                    format!("<BASE/notifications?page={}>; rel=\"next\"", page + 1),
                ));
            }
            Some(reply)
        })
        .await;
        (server, inbox)
    }

    fn fill_inbox(inbox: &Arc<Mutex<Inbox>>, count: usize, tied: bool) {
        let mut inbox = inbox.lock().unwrap();
        let start = OffsetDateTime::parse(NEW, &Rfc3339).unwrap();
        inbox.date = start + time::Duration::seconds(1000);
        inbox.threads = (0..count)
            .map(|id| {
                let updated = start + time::Duration::seconds(if tied { 0 } else { id as i64 });
                thread(&id.to_string(), &updated.format(&Rfc3339).unwrap())
            })
            .collect();
    }

    fn received_ids(rx: &mut mpsc::Receiver<IncomingEvent>) -> Vec<String> {
        let mut ids = Vec::new();
        while let Ok(event) = rx.try_recv() {
            ids.push(event.payload["thread_id"].as_str().unwrap().to_string());
        }
        ids
    }

    async fn bounded_poll(
        server: &Server,
        config: &AppConfig,
        client: &GitHubClient,
        tx: &mpsc::Sender<IncomingEvent>,
        state: &mut MonitorState,
    ) {
        let start = server.requests.lock().unwrap().len();
        let catch_up = state.catch_up.clone();
        let watermark = state.watermark;
        poll_once(config, client, tx, state).await.unwrap();
        let requests = server.requests.lock().unwrap();
        assert!(requests.len() - start <= MAX_PAGES);
        for (index, request) in requests[start..].iter().enumerate() {
            let path = request.split_whitespace().nth(1).unwrap();
            let url = reqwest::Url::parse(&format!("{}{path}", server.base)).unwrap();
            let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
            let since = catch_up
                .as_ref()
                .map(|cursor| {
                    if index == 0 {
                        cursor.poll_date
                    } else {
                        cursor.since
                    }
                })
                .or(watermark);
            assert_eq!(
                query.get("since"),
                since
                    .map(|since| {
                        (since - time::Duration::seconds(1))
                            .format(&Rfc3339)
                            .unwrap()
                    })
                    .as_ref()
            );
            if let Some(cursor) = &catch_up {
                assert!(!request.contains("if-modified-since:"));
                if index == 0 {
                    assert!(!query.contains_key("before"));
                    assert_eq!(query["page"], "1");
                } else {
                    assert_eq!(query["before"], cursor.before.format(&Rfc3339).unwrap());
                }
            } else {
                assert!(!query.contains_key("before"));
            }
        }
    }

    #[tokio::test]
    async fn notifications_cold_start_update_and_unchanged() {
        let server = server(vec![
            Reply::ok(json!([thread("1", OLD)])),
            Reply::ok(json!([thread("1", NEW)])),
            Reply::ok(json!([thread("1", NEW)])),
        ])
        .await;
        let config = config(&server.base);
        let client = build_github_client(Some("test-token".into())).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert!(rx.try_recv().is_err());
        assert_eq!(state.threads.len(), 1);
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        let event = rx.try_recv().unwrap();
        assert_eq!(event.kind, "github.notification");
        assert_eq!(
            event.payload,
            json!({"repo": "owner/repo", "thread_id": "1",
            "reason": "review_requested", "subject_type": "PullRequest", "title": "A PR",
            "number": 12, "url": "https://github.com/owner/repo/pull/12#issuecomment-123",
            "updated_at": NEW})
        );
        assert_eq!(event.channel.as_deref(), Some("notifications"));
        assert_eq!(event.mention.as_deref(), Some("<@123>"));
        let mut routed = event.clone();
        routed.channel = None;
        let route_config = AppConfig {
            routes: vec![crate::config::RouteRule {
                event: "github.notification".into(),
                sink: "localfile".into(),
                local_path: Some("/tmp/notifications.jsonl".into()),
                filter: [
                    ("reason".into(), "review_*".into()),
                    ("subject_type".into(), "PullRequest".into()),
                ]
                .into_iter()
                .collect(),
                ..crate::config::RouteRule::default()
            }],
            ..AppConfig::default()
        };
        let router = crate::router::Router::new(Arc::new(route_config));
        assert_eq!(router.resolve(&routed).await.unwrap().len(), 1);
        routed.payload["reason"] = json!("mention");
        assert!(router.resolve(&routed).await.is_err());
        routed.payload["reason"] = json!("review_requested");
        routed.payload["subject_type"] = json!("Issue");
        assert!(router.resolve(&routed).await.is_err());
        let envelope = crate::event::compat::from_incoming_event(&event).unwrap();
        assert!(matches!(
            envelope.body,
            crate::event::EventBody::GitHubNotification(_)
        ));
        for format in [
            crate::events::MessageFormat::Compact,
            crate::events::MessageFormat::Alert,
            crate::events::MessageFormat::Inline,
            crate::events::MessageFormat::Raw,
        ] {
            let rendered = event.render_default(&format).unwrap();
            assert!(rendered.contains("review_requested"));
            assert!(rendered.contains("A PR"));
        }
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert!(rx.try_recv().is_err());
        let requests = server.requests.lock().unwrap();
        assert!(requests[0].contains("authorization: Bearer test-token"));
        assert!(!requests[0].to_lowercase().contains("if-modified-since:"));
        assert!(requests[1].contains(&format!("if-modified-since: {DATE_VALUE}")));
    }

    #[tokio::test]
    async fn notifications_304_preserves_baseline_and_honors_interval() {
        let mut unchanged = Reply::status(304);
        unchanged
            .headers
            .push(("X-Poll-Interval".into(), "180".into()));
        unchanged
            .headers
            .push(("Last-Modified".into(), "must not replace cursor".into()));
        let server = server(vec![Reply::ok(json!([thread("1", OLD)])), unchanged]).await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        let baseline = (
            state.threads.clone(),
            state.cutoff,
            state.last_modified.clone(),
        );
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert_eq!(
            (
                state.threads.clone(),
                state.cutoff,
                state.last_modified.clone()
            ),
            baseline
        );
        assert!(rx.try_recv().is_err());
        assert!(state.last_degraded.is_none());
        assert_eq!(state.interval(60), Duration::from_secs(180));
        assert_eq!(state.interval(300), Duration::from_secs(300));
        assert!(
            server.requests.lock().unwrap()[1]
                .contains(&format!("if-modified-since: {DATE_VALUE}"))
        );
    }

    #[tokio::test]
    async fn notifications_latest_poll_interval_on_success_failure_and_304() {
        let mut replies = vec![
            Reply::ok(json!([])),
            Reply::ok(json!([])),
            Reply::status(500),
            Reply::ok(json!([])),
            Reply::status(304),
            Reply::status(304),
            Reply::ok(json!([])),
            Reply::ok(json!([])),
        ];
        for (reply, interval) in replies
            .iter_mut()
            .zip(["3600", "60", "3600", "60", "3600", "60", "invalid"])
        {
            reply
                .headers
                .push(("X-Poll-Interval".into(), interval.into()));
        }
        let server = server(replies).await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        for (index, expected) in [3600, 60, 3600, 60, 3600, 60, 60, 60]
            .into_iter()
            .enumerate()
        {
            let result = poll_once(&config, &client, &tx, &mut state).await;
            assert_eq!(result.is_err(), index == 2);
            assert_eq!(state.interval(1), Duration::from_secs(expected));
            assert_eq!(state.interval(120), Duration::from_secs(expected.max(120)));
            assert!(rx.try_recv().is_err());
        }
        assert_eq!(server.requests.lock().unwrap().len(), 8);
    }

    #[tokio::test]
    async fn notifications_filters_participating_and_cold_cutoff() {
        let mut other_repo = thread("2", NEW);
        other_repo["repository"]["full_name"] = json!("other/repo");
        let mut other_reason = thread("3", NEW);
        other_reason["reason"] = json!("mention");
        let server = server(vec![
            Reply::ok(json!([])),
            Reply::ok(json!([
                thread("1", NEW),
                other_repo,
                other_reason,
                thread("historical", OLD)
            ])),
        ])
        .await;
        let mut config = config(&server.base);
        config.monitors.github_notifications.participating = true;
        config.monitors.github_notifications.reasons = vec!["review_requested".into()];
        config.monitors.github_notifications.repos = vec!["OWNER/REPO".into()];
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert_eq!(rx.try_recv().unwrap().payload["thread_id"], "1");
        assert!(rx.try_recv().is_err());
        assert_eq!(state.threads.len(), 4);
        for request in server.requests.lock().unwrap().iter() {
            assert!(request.starts_with("GET /notifications?"));
            assert!(request.contains("all=false"));
            assert!(request.contains("participating=true"));
            assert!(request.contains("per_page=50"));
        }
    }

    #[tokio::test]
    async fn notifications_failure_preserves_cursor_then_recovers_once() {
        telemetry::take_test_records();
        let server = server(vec![
            Reply::ok(json!([thread("1", OLD)])),
            Reply::status(500),
            Reply::status(401),
            Reply::ok(json!([thread("1", NEW)])),
            Reply::ok(json!([thread("1", NEW)])),
        ])
        .await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        let baseline = (
            state.threads.clone(),
            state.cutoff,
            state.last_modified.clone(),
        );
        for status in ["500", "401"] {
            let error = poll_once(&config, &client, &tx, &mut state)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(status));
            assert!(rx.try_recv().is_err());
            assert_eq!(
                (
                    state.threads.clone(),
                    state.cutoff,
                    state.last_modified.clone()
                ),
                baseline
            );
        }
        let records = telemetry::take_test_records();
        assert_eq!(
            records
                .iter()
                .filter(|record| record["source"] == "github_notifications"
                    && record["telemetry_event"] == "source_degraded")
                .count(),
            1,
            "degradation must be throttled"
        );
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert_eq!(rx.try_recv().unwrap().payload["updated_at"], NEW);
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert!(rx.try_recv().is_err());
        assert!(
            server.requests.lock().unwrap()[3]
                .contains(&format!("if-modified-since: {DATE_VALUE}"))
        );
    }

    #[tokio::test]
    async fn notifications_capped_backlog_drains_all_251_once() {
        telemetry::take_test_records();
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(512);
        let mut state = MonitorState::default();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let watermark = state.watermark;
        fill_inbox(&inbox, 251, false);
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let mut ids = received_ids(&mut rx);
        assert_eq!(ids.len(), 250);
        assert_eq!(state.watermark, watermark);
        assert!(state.catch_up.is_some());
        assert!(state.last_modified.is_none());
        assert!(telemetry::take_test_records().iter().any(|record| {
            record["source"] == "github_notifications"
                && record["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("pagination cap"))
        }));
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        ids.extend(received_ids(&mut rx));
        assert_eq!(ids.len(), 251);
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 251);
        assert!(state.catch_up.is_none());
        assert_eq!(state.watermark, Some(inbox.lock().unwrap().date));
        assert!(state.last_modified.is_none());
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert!(state.last_modified.is_some());
        let baseline = state.watermark;
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(state.watermark, baseline);
        assert!(rx.try_recv().is_err());
        let requests = server.requests.lock().unwrap();
        assert!(!requests[0].contains("since="));
        for request in &requests[1..] {
            assert!(request.contains("since="));
        }
        assert!(requests.last().unwrap().contains("if-modified-since:"));
    }

    #[tokio::test]
    async fn notifications_catch_up_advances_before_across_multiple_budgets() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(1024);
        let mut state = MonitorState::default();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        fill_inbox(&inbox, 700, false);
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let mut ids = received_ids(&mut rx);
        let watermark = state.watermark;
        for _ in 0..3 {
            let previous = state.catch_up.clone().unwrap();
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            ids.extend(received_ids(&mut rx));
            if let Some(cursor) = &state.catch_up {
                assert!(cursor.before < previous.before);
                assert_eq!(cursor.page, 1);
                assert_eq!(state.watermark, watermark);
            }
        }
        assert!(state.catch_up.is_none());
        assert_eq!(state.watermark, Some(inbox.lock().unwrap().date));
        assert_eq!(ids.len(), 700);
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 700);
    }

    #[tokio::test]
    async fn notifications_fresh_head_overflow_is_scanned_after_catch_up() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(512);
        let mut state = MonitorState::default();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        fill_inbox(&inbox, 251, false);
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let mut ids = received_ids(&mut rx);
        {
            let mut inbox = inbox.lock().unwrap();
            let updated = (inbox.date + time::Duration::seconds(1))
                .format(&Rfc3339)
                .unwrap();
            inbox
                .threads
                .extend((0..60).map(|id| thread(&format!("late-{id}"), &updated)));
            inbox.date += time::Duration::seconds(2);
        }
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let head = received_ids(&mut rx);
        assert_eq!(head.iter().filter(|id| id.starts_with("late-")).count(), 50);
        ids.extend(head);
        assert!(state.catch_up.is_none());
        assert!(state.last_modified.is_none());
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let remaining = received_ids(&mut rx);
        assert_eq!(remaining.len(), 10);
        ids.extend(remaining);
        assert_eq!(ids.len(), 311);
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 311);
    }

    #[tokio::test]
    async fn notifications_filtered_head_does_not_starve_older_match() {
        let (server, inbox) = inbox_server().await;
        let mut config = config(&server.base);
        config.monitors.github_notifications.reasons = vec!["review_requested".into()];
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(512);
        let mut state = MonitorState::default();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        fill_inbox(&inbox, 251, false);
        for thread in inbox.lock().unwrap().threads.iter_mut().skip(1) {
            thread["reason"] = json!("mention");
        }
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert!(rx.try_recv().is_err());
        assert!(state.catch_up.is_some());
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(received_ids(&mut rx), ["0"]);
        assert!(state.catch_up.is_none());
    }

    #[tokio::test]
    async fn notifications_boundary_ties_and_new_activity_during_catch_up() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(512);
        let mut state = MonitorState::default();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        fill_inbox(&inbox, 251, true);
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let mut ids = received_ids(&mut rx);
        assert_eq!(ids.len(), 250);
        let original = state.catch_up.clone().unwrap();
        {
            let mut inbox = inbox.lock().unwrap();
            let updated = inbox.date + time::Duration::seconds(1);
            inbox
                .threads
                .push(thread("late", &updated.format(&Rfc3339).unwrap()));
            inbox.date += time::Duration::seconds(2);
        }
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(received_ids(&mut rx), ["late"]);
        ids.push("late".into());
        let cursor = state.catch_up.as_ref().unwrap();
        assert_eq!(cursor.before, original.before);
        assert_eq!(cursor.page, 5, "a full boundary second resumes, not skips");
        assert_eq!(state.watermark, Some(original.since));
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        ids.extend(received_ids(&mut rx));
        assert!(state.catch_up.is_none());
        assert_eq!(state.watermark, Some(original.poll_date));
        // The catch-up head's Last-Modified must not suppress the next window.
        assert!(state.last_modified.is_none());
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert!(rx.try_recv().is_err());
        assert_eq!(ids.len(), 252);
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 252, "all tied IDs and fresh head emitted once");
        let requests = server.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .any(|request| request.contains("before=2026-10-15T00%3A00%3A02Z"))
        );
        assert!(requests.iter().any(|request| request.contains("page=6")));
    }

    #[tokio::test]
    async fn notifications_catch_up_failure_preserves_continuation_and_head() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(512);
        let mut state = MonitorState::default();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        fill_inbox(&inbox, 251, true);
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(state.catch_up.as_ref().unwrap().page, 5);
        let mut ids = received_ids(&mut rx);
        let baseline = (
            state.watermark,
            state.catch_up.clone(),
            state.threads.clone(),
        );
        {
            let mut inbox = inbox.lock().unwrap();
            let updated = inbox.date + time::Duration::seconds(1);
            inbox
                .threads
                .push(thread("late", &updated.format(&Rfc3339).unwrap()));
            inbox.date += time::Duration::seconds(2);
            inbox.fail_at = Some(inbox.requests + 2); // head succeeds, backlog fails
        }
        let start = server.requests.lock().unwrap().len();
        assert!(poll_once(&config, &client, &tx, &mut state).await.is_err());
        assert_eq!(server.requests.lock().unwrap().len() - start, 2);
        assert_eq!(
            (
                state.watermark,
                state.catch_up.clone(),
                state.threads.clone()
            ),
            baseline
        );
        assert!(state.last_modified.is_none());
        assert!(rx.try_recv().is_err());
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        ids.extend(received_ids(&mut rx));
        assert!(state.catch_up.is_none());
        assert_eq!(ids.len(), 252);
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 252);
        let requests = server.requests.lock().unwrap();
        assert_eq!(
            requests[start].lines().next(),
            requests[start + 2].lines().next()
        );
        assert_eq!(
            requests[start + 1].lines().next(),
            requests[start + 3].lines().next()
        );
    }

    #[tokio::test]
    async fn notifications_disabled_makes_zero_requests() {
        let server = server(vec![Reply::ok(json!([]))]).await;
        let config = Arc::new(AppConfig::default());
        let (tx, mut rx) = mpsc::channel(1);
        GitHubNotificationsSource::new(config)
            .run(tx.clone())
            .await
            .unwrap();
        let mut disabled = self::config(&server.base);
        disabled.monitors.github_notifications.enabled = false;
        poll_once(
            &disabled,
            &build_github_client(None).unwrap(),
            &tx,
            &mut MonitorState::default(),
        )
        .await
        .unwrap();
        assert!(server.requests.lock().unwrap().is_empty());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn notifications_dedupe_bounded_and_monotonic() {
        let mut state = MonitorState::default();
        let newer = OffsetDateTime::parse(NEWER, &Rfc3339).unwrap();
        let older = OffsetDateTime::parse(NEW, &Rfc3339).unwrap();
        state.remember("0", newer);
        state.remember("0", older);
        assert_eq!(state.threads["0"], newer);
        for id in 1..=MAX_THREADS {
            state.remember(&id.to_string(), older);
        }
        assert_eq!(state.threads.len(), MAX_THREADS);
        assert_eq!(state.order.len(), MAX_THREADS);
        assert!(!state.threads.contains_key("0"));
    }

    #[tokio::test]
    async fn notifications_page_failure_is_transactional_and_recovery_dedupes_pages() {
        let mut first = Reply::ok(json!([thread("1", NEW)]));
        first.headers.push((
            "Link".into(),
            "<BASE/notifications?page=2>; rel=\"next\"".into(),
        ));
        let mut retry_first = Reply::ok(json!([thread("1", NEW)]));
        retry_first.headers = first.headers.clone();
        let server = server(vec![
            Reply::ok(json!([thread("1", OLD)])),
            first,
            Reply::status(500),
            retry_first,
            Reply::ok(json!([thread("1", NEW), thread("2", NEWER)])),
        ])
        .await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        let baseline = (
            state.threads.clone(),
            state.cutoff,
            state.last_modified.clone(),
        );
        assert!(poll_once(&config, &client, &tx, &mut state).await.is_err());
        assert!(rx.try_recv().is_err());
        assert_eq!(
            (
                state.threads.clone(),
                state.cutoff,
                state.last_modified.clone()
            ),
            baseline
        );
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert_eq!(rx.try_recv().unwrap().payload["thread_id"], "1");
        assert_eq!(rx.try_recv().unwrap().payload["thread_id"], "2");
        assert!(rx.try_recv().is_err());
        let requests = server.requests.lock().unwrap();
        assert!(requests[3].contains("if-modified-since:"));
        assert!(requests[4].contains("page=2"));
        assert!(!requests[4].contains("if-modified-since:"));
    }

    #[tokio::test]
    async fn notifications_read_thread_resurfaces_only_when_updated() {
        let server = server(vec![
            Reply::ok(json!([thread("1", OLD)])),
            Reply::ok(json!([])),
            Reply::ok(json!([thread("1", OLD), thread("2", OLD)])),
            Reply::ok(json!([thread("1", NEW), thread("2", NEW)])),
        ])
        .await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        for _ in 0..3 {
            poll_once(&config, &client, &tx, &mut state).await.unwrap();
            assert!(rx.try_recv().is_err());
        }
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert_eq!(rx.try_recv().unwrap().payload["thread_id"], "1");
        assert_eq!(rx.try_recv().unwrap().payload["thread_id"], "2");
        assert!(rx.try_recv().is_err());
        assert!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| request.starts_with("GET "))
        );
    }

    #[tokio::test]
    async fn notifications_missing_date_or_malformed_timestamp_preserves_state() {
        let mut no_date = Reply::ok(json!([thread("1", OLD)]));
        no_date.headers.retain(|(key, _)| key != "Date");
        let mut warm_no_date = Reply::ok(json!([thread("1", NEW)]));
        warm_no_date.headers.retain(|(key, _)| key != "Date");
        let mut invalid_date = Reply::ok(json!([thread("1", NEW)]));
        invalid_date.headers.retain(|(key, _)| key != "Date");
        invalid_date.headers.push(("Date".into(), "invalid".into()));
        let server = server(vec![
            no_date,
            Reply::ok(json!([thread("1", OLD)])),
            warm_no_date,
            invalid_date,
            Reply::ok(json!([thread("1", NEW), thread("bad", "invalid-date")])),
            Reply::ok(json!([thread("1", NEW)])),
        ])
        .await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        assert!(poll_once(&config, &client, &tx, &mut state).await.is_err());
        assert!(
            state.cutoff.is_none() && state.last_modified.is_none() && state.threads.is_empty()
        );
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        let baseline = (
            state.threads.clone(),
            state.cutoff,
            state.last_modified.clone(),
            state.watermark,
            state.catch_up.clone(),
        );
        for _ in 0..3 {
            assert!(poll_once(&config, &client, &tx, &mut state).await.is_err());
            assert_eq!(
                (
                    state.threads.clone(),
                    state.cutoff,
                    state.last_modified.clone(),
                    state.watermark,
                    state.catch_up.clone(),
                ),
                baseline
            );
            assert!(rx.try_recv().is_err());
        }
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert_eq!(rx.try_recv().unwrap().payload["thread_id"], "1");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn notifications_network_failure_preserves_state() {
        let server = server(vec![Reply::ok(json!([thread("1", OLD)]))]).await;
        let mut config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        server.task.abort();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        config.monitors.github_api_base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener); // Connection refused, without relying on an external address.
        let baseline = (
            state.threads.clone(),
            state.cutoff,
            state.last_modified.clone(),
        );
        assert!(poll_once(&config, &client, &tx, &mut state).await.is_err());
        assert_eq!(
            (
                state.threads.clone(),
                state.cutoff,
                state.last_modified.clone()
            ),
            baseline
        );
        assert!(rx.try_recv().is_err());
        assert!(state.last_degraded.is_some());
    }

    #[tokio::test]
    async fn notifications_forbidden_and_rate_limited_share_backoff() {
        for status in [403, 429] {
            let mut limited = Reply::status(status);
            limited
                .headers
                .push(("X-Poll-Interval".into(), "180".into()));
            let server = server(vec![
                Reply::ok(json!([thread("1", OLD)])),
                limited,
                Reply::ok(json!([thread("1", NEW)])),
            ])
            .await;
            let config = config(&server.base);
            let client = build_github_client(None).unwrap();
            let (tx, mut rx) = mpsc::channel(16);
            let mut state = MonitorState::default();
            poll_once(&config, &client, &tx, &mut state).await.unwrap();
            let baseline = (
                state.threads.clone(),
                state.cutoff,
                state.last_modified.clone(),
            );
            assert!(poll_once(&config, &client, &tx, &mut state).await.is_err());
            let error = poll_once(&config, &client, &tx, &mut state)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("backoff"));
            assert_eq!(server.requests.lock().unwrap().len(), 2);
            assert_eq!(state.interval(60), Duration::from_secs(180));
            assert_eq!(
                (
                    state.threads.clone(),
                    state.cutoff,
                    state.last_modified.clone()
                ),
                baseline
            );
            assert!(rx.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn notifications_rejects_cross_origin_pagination_without_following() {
        let mut reply = Reply::ok(json!([thread("1", NEW)]));
        reply.headers.push((
            "Link".into(),
            "<https://untrusted.example/notifications?page=2>; rel=\"next\"".into(),
        ));
        let server = server(vec![reply]).await;
        let config = config(&server.base);
        let client = build_github_client(Some("test-token".into())).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        let error = poll_once(&config, &client, &tx, &mut state)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsafe"));
        assert_eq!(server.requests.lock().unwrap().len(), 1);
        assert!(state.threads.is_empty() && state.cutoff.is_none());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn notifications_cold_pagination_seeds_every_page_and_uses_first_date() {
        let mut first = Reply::ok(json!([thread("1", OLD)]));
        first.headers.push((
            "Link".into(),
            "<BASE/notifications?page=2>; rel=\"next\"".into(),
        ));
        let mut second = Reply::ok(json!([thread("2", NEW)]));
        second.headers.retain(|(key, _)| key != "Date");
        second
            .headers
            .push(("Date".into(), "Thu, 15 Oct 2026 00:00:05 GMT".into()));
        let server = server(vec![
            first,
            second,
            Reply::ok(json!([
                thread("1", OLD),
                thread("2", NEW),
                thread("3", NEWER)
            ])),
        ])
        .await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert_eq!(state.threads.len(), 2);
        assert!(rx.try_recv().is_err());
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert_eq!(rx.try_recv().unwrap().payload["thread_id"], "3");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn notifications_subject_links_without_metadata_requests() {
        let mut value = thread("1", NEW);
        value["subject"]["url"] = json!("https://api.github.com/repos/owner/repo/issues/12");
        let notification = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            subject_link(&notification, "https://api.github.com"),
            (
                Some(12),
                "https://github.com/owner/repo/issues/12#issuecomment-123".into()
            )
        );
        value["subject"]["latest_comment_url"] = Value::Null;
        value["subject"]["url"] = json!("https://api.github.com/repos/owner/repo/commits/abc123");
        let notification = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            subject_link(&notification, "https://api.github.com"),
            (None, "https://github.com/owner/repo/commit/abc123".into())
        );
        value["subject"]["url"] = Value::Null;
        let notification = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(
            subject_link(&notification, "https://api.github.com"),
            (None, "https://github.com/owner/repo".into())
        );
        value["subject"]["url"] = json!("https://git.example/api/v3/repos/owner/repo/pulls/12");
        let notification = serde_json::from_value(value).unwrap();
        assert_eq!(
            subject_link(&notification, "https://git.example/api/v3"),
            (Some(12), "https://git.example/owner/repo/pull/12".into())
        );
    }

    #[tokio::test]
    async fn notifications_null_subject_url_emits_without_number() {
        let mut release = thread("release", NEW);
        release["subject"]["type"] = json!("Release");
        release["subject"]["url"] = Value::Null;
        release["subject"]["latest_comment_url"] = Value::Null;
        let server = server(vec![Reply::ok(json!([])), Reply::ok(json!([release]))]).await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        let event = rx.try_recv().unwrap();
        assert_eq!(event.payload["subject_type"], "Release");
        assert!(event.payload.get("number").is_none());
        assert_eq!(event.payload["url"], format!("{}/owner/repo", server.base));
        assert!(
            event
                .render_default(&crate::events::MessageFormat::Compact)
                .is_ok()
        );
        let envelope = crate::event::compat::from_incoming_event(&event).unwrap();
        assert!(matches!(
            envelope.body,
            crate::event::EventBody::GitHubNotification(GitHubNotificationEvent {
                number: None,
                ..
            })
        ));
        assert!(rx.try_recv().is_err());
        assert_eq!(server.requests.lock().unwrap().len(), 2);
    }
}
