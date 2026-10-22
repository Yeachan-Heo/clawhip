//! Opt-in, authenticated notification polling. Never mutates GitHub threads.
//!
//! Requests use `all=true`: a thread read elsewhere before the next poll still
//! carries its new `updated_at`, so delivery depends on the watermark and
//! per-thread dedup rather than on GitHub's unread flag.

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
const RECONCILE: time::Duration = time::Duration::minutes(5);
const HTTP_DATE_FORMAT: &str =
    "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT";

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
    since: Option<OffsetDateTime>,
    before: OffsetDateTime,
    covered_max: OffsetDateTime,
    // Offsets enumerate oversized ties, but never prove coverage across polls.
    tied: bool,
    page: u64,
    // Delivery-only timestamp scan above a pinned boundary second.
    newer_before: Option<OffsetDateTime>,
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
    let mut catch_up = state.catch_up.clone();
    let mut page = catch_up.as_ref().map_or(1, |cursor| cursor.page);
    let mut oldest = None;
    let mut newest = catch_up.as_ref().map(|cursor| cursor.covered_max);
    let pinned = catch_up.as_ref().is_some_and(|cursor| cursor.tied);
    let newer_before = catch_up.as_ref().and_then(|cursor| cursor.newer_before);
    let mut newer_page = 1;
    let mut newer_oldest = None;
    let mut newer_done = false;
    let mut result = PollResult {
        threads: Vec::new(),
        date: None,
        last_modified: None,
        watermark: state.watermark,
        catch_up: None,
    };
    for index in 0..MAX_PAGES {
        // Pinned ties get a fresh head, at most three newer continuation pages,
        // and at least one tie page. A restarting newer scan reuses the head;
        // when it ends early, the tie receives the unused request budget.
        let fresh_head = catch_up.is_some() && index == 0;
        let newer_range = pinned
            && if fresh_head {
                newer_before.is_none()
            } else {
                index < MAX_PAGES - 1 && !newer_done
            };
        let request_page = if fresh_head {
            1
        } else if newer_range {
            newer_page
        } else {
            page
        };
        let since = catch_up
            .as_ref()
            .map_or(state.watermark, |cursor| cursor.since);
        let participating = config.monitors.github_notifications.participating;
        let mut request = client.http.get(endpoint.clone()).query(&[
            ("all", "true".to_string()),
            ("participating", participating.to_string()),
            ("per_page", "50".to_string()),
            ("page", request_page.to_string()),
        ]);
        let lower = if pinned && (fresh_head || newer_range) {
            // GitHub's exclusive since excludes the pinned second itself.
            catch_up
                .as_ref()
                .map(|cursor| cursor.before - time::Duration::seconds(1))
        } else if pinned {
            // Enumerate only the boundary second, never offsets into older data.
            catch_up
                .as_ref()
                .map(|cursor| cursor.before - time::Duration::seconds(2))
        } else {
            since.map(|since| since - RECONCILE)
        };
        if let Some(lower) = lower {
            request = request.query(&[("since", lower.format(&Rfc3339)?)]);
        }
        let before = if newer_range {
            newer_before
        } else if !fresh_head {
            catch_up.as_ref().map(|cursor| cursor.before)
        } else {
            None
        };
        if let Some(before) = before {
            request = request.query(&[("before", before.format(&Rfc3339)?)]);
        }
        if index == 0
            && catch_up.is_none()
            && let Some(modified) = &state.last_modified
            && let Ok(modified) = OffsetDateTime::parse(modified, &Rfc2822)
            && let Some(covered) = state.watermark.or(state.cutoff)
        {
            // A later Last-Modified must not hide a late-visible update inside
            // the reconciliation window, even when its Date has not changed.
            request = request.header(
                IF_MODIFIED_SINCE,
                modified
                    .min(covered - RECONCILE)
                    .to_offset(time::UtcOffset::UTC)
                    .format(&time::format_description::parse(HTTP_DATE_FORMAT)?)?,
            );
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
            if newer_range {
                newer_oldest = Some(
                    newer_oldest.map_or(updated, |previous: OffsetDateTime| previous.min(updated)),
                );
            } else if !fresh_head {
                oldest =
                    Some(oldest.map_or(updated, |previous: OffsetDateTime| previous.min(updated)));
                newest = Some(newest.map_or(updated, |previous| previous.max(updated)));
            }
            result.threads.push((thread, updated));
        }
        if newer_range {
            if let Some(next) = next {
                if next <= request_page {
                    return Err("non-advancing GitHub notifications pagination link".into());
                }
                newer_page = next;
            } else {
                newer_done = true;
            }
            if newer_done || index == MAX_PAGES - 2 {
                catch_up.as_mut().unwrap().newer_before = if newer_done {
                    None // Restart fresh next poll; this scan never proves W.
                } else {
                    Some(
                        newer_oldest
                            .ok_or("GitHub notifications newer pagination without threads")?
                            + time::Duration::seconds(1),
                    )
                    // A nested second can exhaust this budget without moving
                    // the boundary. Restart from the head next poll so distinct
                    // arrivals above that second cannot remain hidden behind it.
                    .filter(|before| newer_before.is_none_or(|previous| *before < previous))
                };
            }
            continue;
        }
        if fresh_head {
            continue;
        }
        let Some(next) = next else {
            if let Some(mut cursor) = catch_up.clone().filter(|cursor| cursor.tied) {
                if cursor.page == 1 {
                    // A whole boundary second fit in this poll: older data can
                    // now be scanned by timestamp, without a persisted offset.
                    cursor.before -= time::Duration::seconds(1);
                    cursor.tied = false;
                    cursor.newer_before = None;
                }
                // Otherwise contraction may have skipped unseen IDs. Reconcile
                // from page one; an offset scan must never commit a watermark.
                cursor.page = 1;
                result.catch_up = Some(cursor);
                result.last_modified = None;
                return Ok(Some(result));
            }
            result.watermark = state.watermark.into_iter().chain(newest).max();
            // The one-page head is not a complete scan. Never commit its maximum
            // or let its conditional cursor hide overflow on the next poll.
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
    let before = oldest.ok_or("GitHub notifications pagination cap without threads")?
        + time::Duration::seconds(1);
    result.catch_up = Some(match catch_up {
        Some(mut cursor) => {
            if cursor.tied {
                cursor.page = page;
            } else if before < cursor.before {
                cursor.before = before;
                cursor.page = 1;
            } else {
                cursor.tied = true;
                cursor.page = page;
            }
            cursor
        }
        // `all=true` exposes the whole read history. Updates older than the
        // cold-start cutoff are never delivered, so a backlog that starts before
        // any watermark is bounded by the cutoff (or this poll's Date when cold)
        // instead of draining history that can only seed the dedup map.
        None => CatchUp {
            since: state.watermark.or(state.cutoff).or(result.date),
            before,
            covered_max: newest.ok_or("GitHub notifications pagination cap without threads")?,
            tied: false,
            page: 1,
            newer_before: None,
        },
    });
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
    const OLD_CONDITIONAL: &str = "if-modified-since: Wed, 14 Oct 2026 23:54:00 GMT";
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
        after_response: Vec<Value>,
        date: OffsetDateTime,
        last_modified: Option<OffsetDateTime>,
        not_modified: usize,
        requests: usize,
        fail_at: Option<usize>,
    }

    fn modified_since(request: &str) -> Option<OffsetDateTime> {
        let value = request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("if-modified-since")
                .then(|| value.trim())
        })?;
        let format = time::format_description::parse(HTTP_DATE_FORMAT).unwrap();
        let date = time::PrimitiveDateTime::parse(value, &format)
            .ok()?
            .assume_utc();
        // Ignore non-IMF-fixdate values, including numeric offsets and a
        // mismatched weekday, rather than rewarding malformed conditionals.
        (date.format(&format).ok()? == value).then_some(date)
    }

    async fn inbox_server() -> (Server, Arc<Mutex<Inbox>>) {
        let inbox = Arc::new(Mutex::new(Inbox {
            threads: Vec::new(),
            after_response: Vec::new(),
            date: OffsetDateTime::parse(DATE_VALUE, &Rfc2822).unwrap(),
            last_modified: None,
            not_modified: 0,
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
            let last_modified = inbox.last_modified.unwrap_or(inbox.date);
            if modified_since(request).is_some_and(|modified| last_modified <= modified) {
                inbox.not_modified += 1;
                return Some(Reply::status(304));
            }
            let path = request.split_whitespace().nth(1).unwrap();
            let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
            let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(url.path(), "/notifications");
            assert!(request.starts_with("GET "));
            // GitHub omits read threads unless `all=true`.
            let all = query["all"] == "true";
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
                        && before.is_none_or(|before| updated < before)
                        && (all || thread["unread"] != json!(false)))
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
                (
                    "Last-Modified".into(),
                    last_modified.format(&Rfc2822).unwrap(),
                ),
            ];
            if threads.len() > page * per_page {
                reply.headers.push((
                    "Link".into(),
                    format!("<BASE/notifications?page={}>; rel=\"next\"", page + 1),
                ));
            }
            // Visibility can change after body selection but before its Date.
            let newly_visible = std::mem::take(&mut inbox.after_response);
            inbox.threads.extend(newly_visible);
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
        let mut newer_pages = 0;
        let mut tie_pages = 0;
        for (index, request) in requests[start..].iter().enumerate() {
            let path = request.split_whitespace().nth(1).unwrap();
            let url = reqwest::Url::parse(&format!("{}{path}", server.base)).unwrap();
            let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
            let since = if let Some(cursor) = &catch_up {
                if cursor.tied {
                    let tie = cursor.before - time::Duration::seconds(1);
                    if index == 0 || query["since"] == tie.format(&Rfc3339).unwrap() {
                        if index > 0 {
                            newer_pages += 1;
                        }
                        Some(tie)
                    } else {
                        tie_pages += 1;
                        Some(tie - time::Duration::seconds(1))
                    }
                } else {
                    cursor.since.map(|since| since - RECONCILE)
                }
            } else {
                watermark.map(|since| since - RECONCILE)
            };
            assert_eq!(
                query.get("since"),
                since.map(|since| since.format(&Rfc3339).unwrap()).as_ref()
            );
            if let Some(cursor) = &catch_up {
                assert!(!request.contains("if-modified-since:"));
                if index == 0 {
                    assert!(!query.contains_key("before"));
                    assert_eq!(query["page"], "1");
                } else if cursor.tied && since == Some(cursor.before - time::Duration::seconds(1)) {
                    assert_eq!(
                        query.get("before"),
                        cursor
                            .newer_before
                            .map(|before| before.format(&Rfc3339).unwrap())
                            .as_ref()
                    );
                } else {
                    assert_eq!(query["before"], cursor.before.format(&Rfc3339).unwrap());
                }
            } else {
                assert!(!query.contains_key("before"));
            }
        }
        if catch_up.is_some_and(|cursor| cursor.tied) {
            assert!(newer_pages <= 3);
            assert!(tie_pages >= 1);
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
        assert!(requests[1].contains(OLD_CONDITIONAL));
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
        assert!(server.requests.lock().unwrap()[1].contains(OLD_CONDITIONAL));
    }

    #[tokio::test]
    async fn notifications_conditional_http_date_controls_304() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        inbox.lock().unwrap().last_modified = Some(
            // An older server modification permits 304; its non-UTC offset
            // must not leak into the outbound HTTP-date.
            OffsetDateTime::parse("2026-10-15T01:50:00+02:00", &Rfc3339).unwrap(),
        );
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let baseline = (state.cutoff, state.watermark, state.last_modified.clone());
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(inbox.lock().unwrap().not_modified, 1);
        assert_eq!(
            (state.cutoff, state.watermark, state.last_modified.clone()),
            baseline
        );
        assert!(rx.try_recv().is_err());
        assert!(
            server.requests.lock().unwrap()[1]
                .contains("if-modified-since: Wed, 14 Oct 2026 23:50:00 GMT")
        );
        for invalid in [
            "Wed, 14 Oct 2026 23:50:00 +0000",
            "Thu, 14 Oct 2026 23:50:00 GMT",
        ] {
            let response = client
                .http
                .get(format!("{}/notifications", server.base))
                .query(&[("all", "true"), ("per_page", "50"), ("page", "1")])
                .header(IF_MODIFIED_SINCE, invalid)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(inbox.lock().unwrap().not_modified, 1);
        }
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
            assert!(request.contains("all=true"));
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
        assert!(server.requests.lock().unwrap()[3].contains(OLD_CONDITIONAL));
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
        assert_eq!(
            state.watermark,
            Some(OffsetDateTime::parse(NEW, &Rfc3339).unwrap() + time::Duration::seconds(250))
        );
        assert!(state.last_modified.is_none());
        // A 251-thread, 250-second burst also fills the five-minute overlap.
        // One thread marked done elsewhere leaves a complete 250-thread normal scan.
        inbox.lock().unwrap().threads.remove(0);
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert!(state.last_modified.is_some());
        let baseline = state.watermark;
        let conditional_start = server.requests.lock().unwrap().len();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(state.watermark, baseline);
        assert!(rx.try_recv().is_err());
        let requests = server.requests.lock().unwrap();
        assert!(!requests[0].contains("since="));
        assert!(requests.last().unwrap().contains("since="));
        assert!(requests[conditional_start].contains("if-modified-since:"));
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
        assert_eq!(
            state.watermark,
            Some(OffsetDateTime::parse(NEW, &Rfc3339).unwrap() + time::Duration::seconds(699))
        );
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
        let watermark = state.watermark;
        fill_inbox(&inbox, 251, true);
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let mut ids = received_ids(&mut rx);
        assert_eq!(ids.len(), 250);
        let original = state.catch_up.clone().unwrap();
        // Started before any watermark, the backlog is bounded by the cutoff.
        assert_eq!(original.since, state.cutoff);
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
        assert_eq!(state.watermark, watermark);
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        ids.extend(received_ids(&mut rx));
        assert!(state.catch_up.as_ref().unwrap().tied);
        assert_eq!(state.catch_up.as_ref().unwrap().page, 1);
        assert_eq!(state.watermark, watermark);
        // Oversized ties cannot prove completion from saved offsets. Repeated
        // reconciliation still emits nothing twice, while the head stays live.
        assert!(state.last_modified.is_none());
        for _ in 0..4 {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            assert!(rx.try_recv().is_err());
            assert_eq!(state.watermark, watermark);
        }
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
        assert!(state.catch_up.as_ref().unwrap().tied);
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

    async fn pinned_tie_delivers_newer(count: usize, fail_continuation: bool) {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(1024);
        let mut state = MonitorState::default();
        inbox.lock().unwrap().threads = vec![thread("baseline", OLD)];
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let watermark = state.watermark;
        fill_inbox(&inbox, 600, true);
        let mut ids = Vec::new();
        for _ in 0..2 {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            ids.extend(received_ids(&mut rx));
        }
        assert!(state.catch_up.as_ref().unwrap().tied);
        let tie = OffsetDateTime::parse(NEW, &Rfc3339).unwrap();
        inbox.lock().unwrap().threads.extend((1..=count).map(|id| {
            let updated = tie + time::Duration::seconds(id as i64);
            thread(&format!("newer-{id}"), &updated.format(&Rfc3339).unwrap())
        }));
        for poll in 0..12 {
            if poll == 1 && fail_continuation {
                assert!(state.catch_up.as_ref().unwrap().newer_before.is_some());
                let baseline = (
                    state.watermark,
                    state.catch_up.clone(),
                    state.threads.clone(),
                    state.last_modified.clone(),
                );
                {
                    let mut inbox = inbox.lock().unwrap();
                    inbox.fail_at = Some(inbox.requests + 3);
                }
                let start = server.requests.lock().unwrap().len();
                assert!(poll_once(&config, &client, &tx, &mut state).await.is_err());
                assert_eq!(server.requests.lock().unwrap().len() - start, 3);
                assert_eq!(
                    (
                        state.watermark,
                        state.catch_up.clone(),
                        state.threads.clone(),
                        state.last_modified.clone()
                    ),
                    baseline
                );
                assert!(rx.try_recv().is_err());
            }
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            ids.extend(received_ids(&mut rx));
            assert_eq!(state.watermark, watermark);
            assert!(state.catch_up.as_ref().unwrap().tied);
            if poll == 0 {
                assert_eq!(
                    ids.iter().filter(|id| id.starts_with("newer-")).count(),
                    count.min(200)
                );
                assert_eq!(
                    state.catch_up.as_ref().unwrap().newer_before.is_some(),
                    count > 200
                );
            }
            if poll == 1 {
                assert_eq!(
                    ids.iter().filter(|id| id.starts_with("newer-")).count(),
                    count
                );
                assert!(state.catch_up.as_ref().unwrap().newer_before.is_none());
            }
        }
        assert_eq!(ids.len(), 600 + count);
        for id in (0..600)
            .map(|id| id.to_string())
            .chain((1..=count).map(|id| format!("newer-{id}")))
        {
            assert_eq!(ids.iter().filter(|seen| **seen == id).count(), 1, "{id}");
        }
    }

    #[tokio::test]
    async fn notifications_pinned_tie_delivers_51_newer_once() {
        pinned_tie_delivers_newer(51, false).await;
    }

    #[tokio::test]
    async fn notifications_pinned_tie_delivers_260_newer_across_polls_once() {
        pinned_tie_delivers_newer(260, false).await;
    }

    #[tokio::test]
    async fn notifications_pinned_newer_failure_preserves_delivery_cursor() {
        pinned_tie_delivers_newer(260, true).await;
    }

    #[tokio::test]
    async fn notifications_nested_newer_tie_still_reserves_original_tie_budget() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(1024);
        let mut state = MonitorState::default();
        inbox.lock().unwrap().threads = vec![thread("baseline", OLD)];
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let watermark = state.watermark;
        fill_inbox(&inbox, 600, true);
        let mut ids = Vec::new();
        for _ in 0..2 {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            ids.extend(received_ids(&mut rx));
        }
        let newer = OffsetDateTime::parse(NEWER, &Rfc3339).unwrap();
        inbox
            .lock()
            .unwrap()
            .threads
            .extend((0..260).map(|id| thread(&format!("newer-{id}"), NEWER)));
        for poll in 0..12 {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            ids.extend(received_ids(&mut rx));
            assert_eq!(state.watermark, watermark);
            assert_eq!(
                state.catch_up.as_ref().unwrap().newer_before,
                (poll % 2 == 0).then_some(newer + time::Duration::seconds(1))
            );
        }
        assert_eq!(
            ids.iter().filter(|id| !id.starts_with("newer-")).count(),
            600
        );
        // This is the documented nonrecursive limitation, not coverage proof:
        // only the first 200 IDs of the newer second can be delivered.
        assert_eq!(
            ids.iter().filter(|id| id.starts_with("newer-")).count(),
            200
        );
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 800);
    }

    #[tokio::test]
    async fn notifications_stalled_newer_cursor_restarts_above_nested_tie_transactionally() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(1024);
        let mut state = MonitorState::default();
        inbox.lock().unwrap().threads = vec![thread("baseline", OLD)];
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let watermark = state.watermark;
        fill_inbox(&inbox, 600, true);
        let mut ids = Vec::new();
        for _ in 0..2 {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            ids.extend(received_ids(&mut rx));
        }
        assert!(state.catch_up.as_ref().unwrap().tied);
        let nested = OffsetDateTime::parse(NEWER, &Rfc3339).unwrap();
        inbox
            .lock()
            .unwrap()
            .threads
            .extend((0..201).map(|id| thread(&format!("nested-{id}"), NEWER)));
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let delivered = received_ids(&mut rx);
        assert_eq!(
            delivered
                .iter()
                .filter(|id| id.starts_with("nested-"))
                .count(),
            200
        );
        ids.extend(delivered);
        assert_eq!(
            state.catch_up.as_ref().unwrap().newer_before,
            Some(nested + time::Duration::seconds(1))
        );
        inbox.lock().unwrap().threads.extend((1..=51).map(|id| {
            let updated = nested + time::Duration::seconds(id);
            thread(&format!("arrival-{id}"), &updated.format(&Rfc3339).unwrap())
        }));
        // Freeze the inbox. A failure on T happens after the local stalled
        // cursor was cleared; none of that poll's delivery/state may commit.
        let baseline = (
            state.watermark,
            state.catch_up.clone(),
            state.threads.clone(),
            state.order.clone(),
            state.cutoff,
            state.last_modified.clone(),
        );
        {
            let mut inbox = inbox.lock().unwrap();
            inbox.fail_at = Some(inbox.requests + MAX_PAGES);
        }
        let start = server.requests.lock().unwrap().len();
        assert!(poll_once(&config, &client, &tx, &mut state).await.is_err());
        assert_eq!(server.requests.lock().unwrap().len() - start, MAX_PAGES);
        assert_eq!(
            (
                state.watermark,
                state.catch_up.clone(),
                state.threads.clone(),
                state.order.clone(),
                state.cutoff,
                state.last_modified.clone(),
            ),
            baseline
        );
        assert!(rx.try_recv().is_err());
        for poll in 0..12 {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            let delivered = received_ids(&mut rx);
            if poll == 0 {
                assert_eq!(
                    delivered
                        .iter()
                        .filter(|id| id.starts_with("arrival-"))
                        .count(),
                    50
                );
                assert!(!delivered.iter().any(|id| id == "arrival-1"));
                assert!(state.catch_up.as_ref().unwrap().newer_before.is_none());
            }
            if poll == 1 {
                assert_eq!(
                    delivered
                        .iter()
                        .filter(|id| id.starts_with("arrival-"))
                        .count(),
                    1
                );
                assert!(delivered.iter().any(|id| id == "arrival-1"));
            }
            ids.extend(delivered);
            assert_eq!(state.watermark, watermark);
            assert!(state.catch_up.as_ref().unwrap().tied);
        }
        // Restarting serves arrivals above U, not recursive coverage of U.
        assert!(!ids.iter().any(|id| id == "nested-200"));
        assert_eq!(ids.len(), 600 + 200 + 51);
        for id in (0..600)
            .map(|id| id.to_string())
            .chain((0..200).map(|id| format!("nested-{id}")))
            .chain((1..=51).map(|id| format!("arrival-{id}")))
        {
            assert_eq!(ids.iter().filter(|seen| **seen == id).count(), 1, "{id}");
        }
    }

    #[tokio::test]
    async fn notifications_pinned_empty_newer_range_leaves_four_tie_pages() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(1024);
        let mut state = MonitorState::default();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        fill_inbox(&inbox, 600, true);
        for _ in 0..2 {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            received_ids(&mut rx);
        }
        assert_eq!(state.catch_up.as_ref().unwrap().page, 5);
        let start = server.requests.lock().unwrap().len();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(server.requests.lock().unwrap().len() - start, 5);
        assert_eq!(state.catch_up.as_ref().unwrap().page, 9);
        assert!(state.catch_up.as_ref().unwrap().newer_before.is_none());
    }

    #[tokio::test]
    async fn notifications_pinned_stale_newer_cursor_leaves_three_tie_pages_then_clears() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(1024);
        let mut state = MonitorState::default();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        fill_inbox(&inbox, 600, true);
        for _ in 0..2 {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            received_ids(&mut rx);
        }
        inbox
            .lock()
            .unwrap()
            .threads
            .extend((0..201).map(|id| thread(&format!("newer-{id}"), NEWER)));
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        received_ids(&mut rx);
        assert!(state.catch_up.as_ref().unwrap().newer_before.is_some());
        let page = state.catch_up.as_ref().unwrap().page;
        inbox
            .lock()
            .unwrap()
            .threads
            .retain(|thread| thread["updated_at"] == NEW);
        let start = server.requests.lock().unwrap().len();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(server.requests.lock().unwrap().len() - start, 5);
        assert_eq!(state.catch_up.as_ref().unwrap().page, page + 3);
        assert!(state.catch_up.as_ref().unwrap().newer_before.is_none());
        let delivered = received_ids(&mut rx);
        assert_eq!(delivered.len(), 150);
        assert!(delivered.iter().all(|id| !id.starts_with("newer-")));
    }

    async fn contraction_reconciles_600(update_head: bool) {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(1024);
        let mut state = MonitorState::default();
        let watermark = OffsetDateTime::parse("2026-10-15T09:00:00Z", &Rfc3339).unwrap();
        {
            let mut inbox = inbox.lock().unwrap();
            inbox.date = watermark;
            inbox.threads = vec![thread("baseline", &watermark.format(&Rfc3339).unwrap())];
        }
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let updated = (watermark + time::Duration::seconds(1))
            .format(&Rfc3339)
            .unwrap();
        {
            let mut inbox = inbox.lock().unwrap();
            inbox.date = watermark + time::Duration::seconds(1001);
            inbox.threads = (0..600)
                .map(|id| thread(&id.to_string(), &updated))
                .collect();
        }
        let mut updates = Vec::new();
        for _ in 0..3 {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            while let Ok(event) = rx.try_recv() {
                updates.push((
                    event.payload["thread_id"].as_str().unwrap().to_string(),
                    event.payload["updated_at"].as_str().unwrap().to_string(),
                ));
            }
        }
        assert_eq!(state.catch_up.as_ref().unwrap().page, 9);
        let head_update = (watermark + time::Duration::seconds(1002))
            .format(&Rfc3339)
            .unwrap();
        {
            let mut inbox = inbox.lock().unwrap();
            if update_head {
                inbox.threads[0]["updated_at"] = json!(head_update);
            } else {
                inbox.threads.remove(0);
            }
            inbox.date += time::Duration::seconds(2);
        }
        for poll in 0..8 {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            let mut fresh = Vec::new();
            while let Ok(event) = rx.try_recv() {
                fresh.push((
                    event.payload["thread_id"].as_str().unwrap().to_string(),
                    event.payload["updated_at"].as_str().unwrap().to_string(),
                ));
            }
            if update_head && poll == 0 {
                assert!(
                    fresh.contains(&("0".into(), head_update.clone())),
                    "head stays live"
                );
            }
            if poll >= 3 {
                assert!(
                    fresh.is_empty(),
                    "reconciliation must not replay remembered updates"
                );
            }
            updates.extend(fresh);
            assert_eq!(
                state.watermark,
                Some(watermark),
                "an offset is not coverage proof"
            );
        }
        for id in 0..600 {
            assert_eq!(
                updates
                    .iter()
                    .filter(|(seen, time)| seen == &id.to_string() && time == &updated)
                    .count(),
                1,
                "missing or repeated original update for thread {id}"
            );
        }
        assert_eq!(updates.len(), 600 + usize::from(update_head));
        assert!(updates.contains(&("400".into(), updated)));
        assert!(state.catch_up.as_ref().unwrap().tied);
    }

    #[tokio::test]
    async fn notifications_catch_up_read_contraction_reconciles_600_once() {
        contraction_reconciles_600(false).await;
    }

    #[tokio::test]
    async fn notifications_catch_up_updated_contraction_reconciles_600_once() {
        contraction_reconciles_600(true).await;
    }

    #[tokio::test]
    async fn notifications_warm_body_selection_before_late_update_reconciles_once() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        inbox.lock().unwrap().threads = vec![thread("baseline", OLD)];
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let selected = "2026-10-15T00:10:00Z";
        let late = "2026-10-15T00:10:01Z";
        {
            let mut inbox = inbox.lock().unwrap();
            inbox.date = OffsetDateTime::parse("2026-10-15T00:10:03Z", &Rfc3339).unwrap();
            inbox.threads = vec![thread("selected", selected)];
            inbox.after_response = vec![thread("X", late)];
        }
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(received_ids(&mut rx), ["selected"]);
        assert_eq!(
            state.watermark,
            Some(OffsetDateTime::parse(selected, &Rfc3339).unwrap())
        );
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(received_ids(&mut rx), ["X"]);
        assert!(
            server.requests.lock().unwrap()[2]
                .contains("if-modified-since: Thu, 15 Oct 2026 00:05:00 GMT")
        );
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn notifications_catch_up_late_visibility_between_windows_reconciles_once() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(512);
        let mut state = MonitorState::default();
        let watermark = OffsetDateTime::parse("2026-10-15T09:00:00Z", &Rfc3339).unwrap();
        {
            let mut inbox = inbox.lock().unwrap();
            inbox.date = watermark;
            inbox.threads = vec![thread("baseline", &watermark.format(&Rfc3339).unwrap())];
        }
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        {
            let mut inbox = inbox.lock().unwrap();
            inbox.date = watermark + time::Duration::seconds(603);
            inbox.threads = (0..251)
                .map(|id| {
                    let updated = watermark + time::Duration::seconds(300 + id);
                    thread(&id.to_string(), &updated.format(&Rfc3339).unwrap())
                })
                .collect();
        }
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let mut ids = received_ids(&mut rx);
        {
            let mut inbox = inbox.lock().unwrap();
            inbox
                .threads
                .push(thread("late-head", "2026-10-15T09:09:59Z"));
            inbox.after_response = vec![thread("late-body", "2026-10-15T09:09:58Z")];
            inbox.date += time::Duration::seconds(1);
        }
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let head = received_ids(&mut rx);
        assert!(head.iter().any(|id| id == "late-head"));
        assert!(!head.iter().any(|id| id == "late-body"));
        ids.extend(head);
        assert!(state.catch_up.is_none());
        assert_eq!(
            state.watermark,
            Some(watermark + time::Duration::seconds(550))
        );
        for _ in 0..4 {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            ids.extend(received_ids(&mut rx));
        }
        assert_eq!(ids.len(), 253);
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 253);
    }

    #[tokio::test]
    async fn notifications_oversized_tie_shrinks_then_timestamp_backlog_completes() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(512);
        let mut state = MonitorState::default();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        fill_inbox(&inbox, 251, true);
        // Use an eligible, older second, distinct from the oversized tie.
        inbox
            .lock()
            .unwrap()
            .threads
            .push(thread("older", "2026-10-15T00:00:00Z"));
        let mut ids = Vec::new();
        for _ in 0..3 {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            ids.extend(received_ids(&mut rx));
        }
        assert_eq!(ids.len(), 251);
        assert!(!ids.iter().any(|id| id == "older"));
        assert!(state.catch_up.as_ref().unwrap().tied);
        assert_eq!(state.catch_up.as_ref().unwrap().page, 1);
        inbox.lock().unwrap().threads.drain(0..60);
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert!(!state.catch_up.as_ref().unwrap().tied);
        assert_eq!(state.catch_up.as_ref().unwrap().page, 1);
        assert!(rx.try_recv().is_err());
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(received_ids(&mut rx), ["older"]);
        assert!(state.catch_up.is_none());
        assert_eq!(
            state.watermark,
            Some(OffsetDateTime::parse(NEW, &Rfc3339).unwrap())
        );
    }

    #[tokio::test]
    async fn notifications_reconciliation_recovers_late_update_behind_received_maximum() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let newest = "2026-10-15T00:10:00Z";
        {
            let mut inbox = inbox.lock().unwrap();
            inbox.date = OffsetDateTime::parse("2026-10-15T00:10:03Z", &Rfc3339).unwrap();
            inbox.threads = vec![thread("newest", newest)];
            inbox.after_response = vec![thread("late", "2026-10-15T00:05:01Z")];
        }
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(received_ids(&mut rx), ["newest"]);
        for expected in [vec!["late"], vec![], vec![]] {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            assert_eq!(received_ids(&mut rx), expected);
            assert_eq!(
                state.watermark,
                Some(OffsetDateTime::parse(newest, &Rfc3339).unwrap())
            );
        }
    }

    #[tokio::test]
    async fn notifications_empty_and_older_responses_do_not_advance_watermark_to_date() {
        let server = server(vec![
            Reply::ok(json!([])),
            Reply::ok(json!([thread("old", OLD)])),
            Reply::ok(json!([])),
            Reply::ok(json!([thread("older", "2026-10-14T23:58:00Z")])),
        ])
        .await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        poll_once(&config, &client, &tx, &mut state).await.unwrap();
        assert!(state.watermark.is_none());
        assert!(state.cutoff.is_some());
        let old = OffsetDateTime::parse(OLD, &Rfc3339).unwrap();
        for _ in 0..3 {
            poll_once(&config, &client, &tx, &mut state).await.unwrap();
            assert_eq!(state.watermark, Some(old));
        }
        assert!(rx.try_recv().is_err());
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

    fn read_thread(id: &str, updated: OffsetDateTime) -> Value {
        let mut value = thread(id, &updated.format(&Rfc3339).unwrap());
        value["unread"] = json!(false);
        value
    }

    #[tokio::test]
    async fn notifications_thread_read_before_poll_is_delivered_once() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let mut state = MonitorState::default();
        let old = OffsetDateTime::parse(OLD, &Rfc3339).unwrap();
        inbox.lock().unwrap().threads = vec![thread("7109", OLD), read_thread("639", old)];
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert!(rx.try_recv().is_err());
        assert_eq!(state.watermark, Some(old));

        // Updated and then read elsewhere before the next poll: GitHub keeps the
        // new updated_at but drops the thread from unread-only listings.
        let updated = OffsetDateTime::parse(NEW, &Rfc3339).unwrap();
        {
            let mut inbox = inbox.lock().unwrap();
            inbox.date = updated + time::Duration::seconds(12);
            inbox.threads = vec![read_thread("7109", updated), read_thread("639", updated)];
        }
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        let mut ids = received_ids(&mut rx);
        ids.sort();
        assert_eq!(ids, ["639", "7109"]);
        assert_eq!(state.watermark, Some(updated));

        // The same updated_at stays inside the reconciliation window but is
        // already processed, so neither repeated polls nor IMS changes resend it.
        for _ in 0..3 {
            inbox.lock().unwrap().date += time::Duration::seconds(60);
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            assert!(rx.try_recv().is_err());
        }
        assert!(
            server
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|request| request.contains("all=true"))
        );
    }

    #[tokio::test]
    async fn notifications_cold_baseline_of_read_history_resends_nothing() {
        let (server, inbox) = inbox_server().await;
        let config = config(&server.base);
        let client = build_github_client(None).unwrap();
        let (tx, mut rx) = mpsc::channel(512);
        let mut state = MonitorState::default();
        let start = OffsetDateTime::parse(NEW, &Rfc3339).unwrap();
        let spacing = time::Duration::minutes(1);
        let history = 300;
        let date = start + spacing * history + time::Duration::hours(1);
        {
            let mut inbox = inbox.lock().unwrap();
            inbox.date = date;
            inbox.threads = (0..history)
                .map(|id| {
                    let updated = start + spacing * id;
                    if id % 2 == 0 {
                        read_thread(&id.to_string(), updated)
                    } else {
                        thread(&id.to_string(), &updated.format(&Rfc3339).unwrap())
                    }
                })
                .collect();
        }
        // Read history exceeds the five-page cap on the cold poll.
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert!(rx.try_recv().is_err());
        let cursor = state.catch_up.clone().unwrap();
        assert_eq!(cursor.since, Some(date));
        assert_eq!(state.cutoff, Some(date));

        // The backlog is bounded by the cutoff instead of draining history.
        let before = server.requests.lock().unwrap().len();
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(server.requests.lock().unwrap().len() - before, 2);
        assert!(state.catch_up.is_none());
        assert_eq!(state.watermark, Some(start + spacing * (history - 1)));
        for _ in 0..2 {
            bounded_poll(&server, &config, &client, &tx, &mut state).await;
            assert!(rx.try_recv().is_err());
            assert!(state.catch_up.is_none());
        }

        // A historical thread updated and read after the baseline still arrives once.
        let updated = date + time::Duration::seconds(30);
        {
            let mut inbox = inbox.lock().unwrap();
            inbox.date = updated + time::Duration::seconds(30);
            inbox.threads[0] = read_thread("0", updated);
        }
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert_eq!(received_ids(&mut rx), ["0"]);
        bounded_poll(&server, &config, &client, &tx, &mut state).await;
        assert!(rx.try_recv().is_err());
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
