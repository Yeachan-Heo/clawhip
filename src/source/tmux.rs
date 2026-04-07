use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::process::Command;
use tokio::sync::{RwLock, mpsc};
use tokio::time::sleep;

use crate::Result;
use crate::client::DaemonClient;
use crate::config::{AppConfig, TmuxSessionMonitor};
use crate::events::{IncomingEvent, MessageFormat, RoutingMetadata};
use crate::keyword_window::{PendingKeywordHits, collect_keyword_hits};
use crate::router::glob_match;
use crate::source::Source;
use crate::summarize::{SummarizedContent, build_summarizer};

pub type SharedTmuxRegistry = Arc<RwLock<HashMap<String, RegisteredTmuxSession>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RegistrationSource {
    CliWatch,
    CliNew,
    #[default]
    ConfigMonitor,
}

impl RegistrationSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CliWatch => "cli-watch",
            Self::CliNew => "cli-new",
            Self::ConfigMonitor => "config-monitor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentProcessInfo {
    pub pid: u32,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RegisteredTmuxSession {
    pub session: String,
    pub channel: Option<String>,
    pub mention: Option<String>,
    #[serde(default)]
    pub routing: RoutingMetadata,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default = "default_keyword_window_secs")]
    pub keyword_window_secs: u64,
    pub stale_minutes: u64,
    pub format: Option<MessageFormat>,
    #[serde(default = "current_timestamp_rfc3339")]
    pub registered_at: String,
    #[serde(default)]
    pub registration_source: RegistrationSource,
    #[serde(default)]
    pub parent_process: Option<ParentProcessInfo>,
    #[serde(default)]
    pub active_wrapper_monitor: bool,
    #[serde(default)]
    pub summarize: bool,
    #[serde(default)]
    pub summarizer: String,
    #[serde(default)]
    pub heartbeat_mins: u64,
    #[serde(default)]
    pub min_new_lines: usize,
    #[serde(default)]
    pub summarize_interval_mins: u64,
    #[serde(default)]
    pub heartbeat_interval: u64,
    #[serde(default)]
    pub summary_interval: u64,
}

impl RegisteredTmuxSession {
    /// Effective heartbeat interval: heartbeat_interval overrides heartbeat_mins when > 0.
    pub fn effective_heartbeat_mins(&self) -> u64 {
        if self.heartbeat_interval > 0 { self.heartbeat_interval } else { self.heartbeat_mins }
    }

    /// Effective summary throttle: summary_interval overrides summarize_interval_mins when > 0.
    pub fn effective_summary_interval(&self) -> u64 {
        if self.summary_interval > 0 { self.summary_interval } else { self.summarize_interval_mins }
    }
}

impl From<&TmuxSessionMonitor> for RegisteredTmuxSession {
    fn from(value: &TmuxSessionMonitor) -> Self {
        Self {
            session: value.session.clone(),
            channel: value.channel.clone(),
            mention: value.mention.clone(),
            routing: RoutingMetadata::default(),
            keywords: value.keywords.clone(),
            keyword_window_secs: value.keyword_window_secs,
            stale_minutes: value.stale_minutes,
            format: value.format.clone(),
            registered_at: current_timestamp_rfc3339(),
            registration_source: RegistrationSource::ConfigMonitor,
            parent_process: None,
            active_wrapper_monitor: false,
            summarize: value.summarize,
            summarizer: value.summarizer.clone(),
            heartbeat_mins: value.heartbeat_mins,
            min_new_lines: value.min_new_lines,
            summarize_interval_mins: value.summarize_interval_mins,
            heartbeat_interval: value.heartbeat_interval,
            summary_interval: value.summary_interval,
        }
    }
}

pub struct TmuxSource {
    config: Arc<AppConfig>,
    registry: SharedTmuxRegistry,
}

impl TmuxSource {
    pub fn new(config: Arc<AppConfig>, registry: SharedTmuxRegistry) -> Self {
        Self { config, registry }
    }
}

#[async_trait::async_trait]
impl Source for TmuxSource {
    fn name(&self) -> &str {
        "tmux"
    }

    async fn run(&self, tx: mpsc::Sender<IncomingEvent>) -> Result<()> {
        let mut state = TmuxMonitorState::default();

        loop {
            poll_tmux(self.config.as_ref(), &self.registry, &tx, &mut state).await?;
            sleep(Duration::from_secs(
                self.config.monitors.poll_interval_secs.max(1),
            ))
            .await;
        }
    }
}

#[async_trait::async_trait]
trait EventEmitter: Send + Sync {
    async fn emit(&self, event: IncomingEvent) -> Result<()>;
}

#[async_trait::async_trait]
impl EventEmitter for mpsc::Sender<IncomingEvent> {
    async fn emit(&self, event: IncomingEvent) -> Result<()> {
        self.send(event)
            .await
            .map_err(|error| format!("tmux source channel closed: {error}").into())
    }
}

#[async_trait::async_trait]
impl EventEmitter for DaemonClient {
    async fn emit(&self, event: IncomingEvent) -> Result<()> {
        self.send_event(&event).await
    }
}

struct TmuxPaneState {
    session: String,
    pane_name: String,
    snapshot: String,
    content_hash: u64,
    last_change: Instant,
    last_stale_notification: Option<Instant>,
    pane_dead: bool,
}

#[derive(Default)]
struct TmuxMonitorState {
    panes: HashMap<String, TmuxPaneState>,
    pending_keyword_hits: HashMap<String, PendingKeywordHits>,
    session_last_heartbeat: HashMap<String, Instant>,
    session_last_summarized: HashMap<String, Instant>,
}

struct TmuxPaneSnapshot {
    pane_id: String,
    session: String,
    pane_name: String,
    content: String,
    pane_dead: bool,
}

pub async fn monitor_registered_session(
    registration: RegisteredTmuxSession,
    client: DaemonClient,
    providers: crate::config::ProvidersConfig,
) -> Result<()> {
    let mut panes = HashMap::new();
    let mut pending_keyword_hits = None;
    let mut last_heartbeat = None;
    let mut last_summarized: Option<Instant> = None;
    let poll_interval = Duration::from_secs(1);

    loop {
        let now = Instant::now();
        flush_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration,
            &client,
            &registration.session,
            now,
            Duration::from_secs(registration.keyword_window_secs.max(1)),
            false,
        )
        .await?;

        if !session_exists(&registration.session).await? {
            flush_pending_keyword_hits(
                &mut pending_keyword_hits,
                &registration,
                &client,
                &registration.session,
                now,
                Duration::from_secs(registration.keyword_window_secs.max(1)),
                true,
            )
            .await?;
            break;
        }

        let panes_snapshot = snapshot_tmux_session(&registration.session).await?;
        let mut active_panes = HashSet::new();

        for pane in panes_snapshot {
            active_panes.insert(pane.pane_id.clone());
            let pane_key = pane.pane_id.clone();
            let hash = content_hash(&pane.content);
            let latest_line = last_nonempty_line(&pane.content);

            match panes.get_mut(&pane_key) {
                None => {
                    panes.insert(
                        pane_key,
                        TmuxPaneState {
                            session: pane.session,
                            pane_name: pane.pane_name,
                            content_hash: hash,
                            snapshot: pane.content,
                            last_change: now,
                            last_stale_notification: None,
                            pane_dead: pane.pane_dead,
                        },
                    );
                }
                Some(existing) => {
                    existing.pane_dead = pane.pane_dead;
                    if existing.content_hash != hash {
                        let hits = collect_keyword_hits(
                            &existing.snapshot,
                            &pane.content,
                            &registration.keywords,
                        );
                        push_pending_keyword_hits(&mut pending_keyword_hits, now, hits);

                        if registration.summarize
                            && should_summarize_now(
                                last_summarized,
                                registration.effective_summary_interval(),
                                registration.min_new_lines,
                                &existing.snapshot,
                                &pane.content,
                                now,
                            )
                        {
                            spawn_content_changed_task(
                                client.clone(),
                                registration.clone(),
                                pane.session.clone(),
                                pane.pane_name.clone(),
                                pane.content.clone(),
                                providers.clone(),
                            );
                            last_summarized = Some(now);
                        }

                        existing.session = pane.session;
                        existing.pane_name = pane.pane_name;
                        existing.content_hash = hash;
                        existing.snapshot = pane.content;
                        existing.last_change = now;
                        existing.last_stale_notification = None;
                        last_heartbeat = Some(now);
                    } else if should_emit_stale(existing, now, registration.stale_minutes) {
                        client
                            .emit(tmux_stale_event(
                                &registration,
                                existing.session.clone(),
                                existing.pane_name.clone(),
                                latest_line,
                            ))
                            .await?;
                        existing.last_stale_notification = Some(now);
                    }
                }
            }
        }

        panes.retain(|pane_id, _| active_panes.contains(pane_id));
        maybe_emit_registered_session_heartbeat(
            &registration,
            &client,
            &panes,
            &mut last_heartbeat,
            Instant::now(),
        )
        .await?;
        sleep(poll_interval).await;
    }

    Ok(())
}

pub async fn list_active_tmux_registrations(
    config: &AppConfig,
    registry: &SharedTmuxRegistry,
) -> Result<Vec<RegisteredTmuxSession>> {
    match list_tmux_sessions().await {
        Ok(available_sessions) => {
            sync_active_config_registrations(config, registry, &available_sessions).await;
        }
        Err(error) => {
            eprintln!("clawhip source tmux list-sessions failed: {error}");
        }
    }

    let snapshot = registry.read().await;
    Ok(sorted_registry_snapshot(&snapshot))
}

async fn poll_tmux(
    config: &AppConfig,
    registry: &SharedTmuxRegistry,
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut TmuxMonitorState,
) -> Result<()> {
    let available_sessions = match list_tmux_sessions().await {
        Ok(sessions) => Some(sessions),
        Err(error) => {
            eprintln!("clawhip source tmux list-sessions failed: {error}");
            None
        }
    };
    if let Some(available_sessions) = available_sessions.as_ref() {
        sync_active_config_registrations(config, registry, available_sessions).await;
    }
    let mut sessions = resolve_monitored_sessions(
        config
            .monitors
            .tmux
            .sessions
            .iter()
            .map(RegisteredTmuxSession::from)
            .collect(),
        available_sessions.as_ref(),
    );
    for (session, registration) in registry.read().await.iter() {
        sessions.insert(session.clone(), registration.clone());
    }

    let mut active_panes = HashSet::new();
    let mut sessions_to_unregister = Vec::new();

    for (session_name, registration) in &sessions {
        if registration.active_wrapper_monitor {
            state.pending_keyword_hits.remove(session_name);
            state.session_last_heartbeat.remove(session_name);
            continue;
        }

        let now = Instant::now();
        flush_session_pending_keyword_hits(
            &mut state.pending_keyword_hits,
            session_name,
            registration,
            tx,
            now,
            false,
        )
        .await?;

        match session_exists(session_name).await {
            Ok(false) => {
                sessions_to_unregister.push(session_name.clone());
                flush_session_pending_keyword_hits(
                    &mut state.pending_keyword_hits,
                    session_name,
                    registration,
                    tx,
                    now,
                    true,
                )
                .await?;
                state.panes.retain(|_, pane| pane.session != *session_name);
                state.session_last_heartbeat.remove(session_name);
                continue;
            }
            Err(error) => {
                eprintln!(
                    "clawhip source tmux has-session failed for {}: {error}",
                    session_name
                );
                continue;
            }
            Ok(true) => {}
        }

        match snapshot_tmux_session(session_name).await {
            Ok(panes) => {
                let mut session_changed = false;
                for pane in panes {
                    let pane_key = format!("{}::{}", pane.session, pane.pane_id);
                    active_panes.insert(pane_key.clone());
                    let now = Instant::now();
                    let hash = content_hash(&pane.content);
                    let latest_line = last_nonempty_line(&pane.content);

                    let hits = match state.panes.get_mut(&pane_key) {
                        None => {
                            state.panes.insert(
                                pane_key,
                                TmuxPaneState {
                                    session: pane.session,
                                    pane_name: pane.pane_name,
                                    snapshot: pane.content,
                                    content_hash: hash,
                                    last_change: now,
                                    last_stale_notification: None,
                                    pane_dead: pane.pane_dead,
                                },
                            );
                            state
                                .session_last_heartbeat
                                .insert(session_name.clone(), now);
                            session_changed = true;
                            None
                        }
                        Some(existing) => {
                            existing.pane_dead = pane.pane_dead;
                            if existing.content_hash != hash {
                                let hits = collect_keyword_hits(
                                    &existing.snapshot,
                                    &pane.content,
                                    &registration.keywords,
                                );
                                if registration.summarize
                                    && should_summarize_now(
                                        state.session_last_summarized.get(session_name).copied(),
                                        registration.effective_summary_interval(),
                                        registration.min_new_lines,
                                        &existing.snapshot,
                                        &pane.content,
                                        now,
                                    )
                                {
                                    spawn_content_changed_task(
                                        tx.clone(),
                                        registration.clone(),
                                        session_name.clone(),
                                        pane.pane_name.clone(),
                                        pane.content.clone(),
                                        config.providers.clone(),
                                    );
                                    state
                                        .session_last_summarized
                                        .insert(session_name.to_string(), now);
                                }
                                existing.pane_name = pane.pane_name;
                                existing.snapshot = pane.content;
                                existing.content_hash = hash;
                                existing.last_change = now;
                                existing.last_stale_notification = None;
                                state
                                    .session_last_heartbeat
                                    .insert(session_name.clone(), now);
                                session_changed = true;
                                Some(hits)
                            } else {
                                if should_emit_stale(existing, now, registration.stale_minutes) {
                                    tx.emit(tmux_stale_event(
                                        registration,
                                        existing.session.clone(),
                                        existing.pane_name.clone(),
                                        latest_line,
                                    ))
                                    .await?;
                                    existing.last_stale_notification = Some(now);
                                }
                                None
                            }
                        }
                    };

                    if let Some(hits) = hits {
                        push_session_pending_keyword_hits(
                            &mut state.pending_keyword_hits,
                            session_name,
                            now,
                            hits,
                        );
                    }
                }
                maybe_emit_session_heartbeat(
                    session_name,
                    registration,
                    tx,
                    state,
                    Instant::now(),
                    session_changed,
                )
                .await?;
            }
            Err(error) => eprintln!(
                "clawhip source tmux snapshot failed for {}: {error}",
                session_name
            ),
        }
    }

    state.panes.retain(|key, _| active_panes.contains(key));

    if !sessions_to_unregister.is_empty() {
        let mut write = registry.write().await;
        for session in sessions_to_unregister {
            write.remove(&session);
        }
    }

    state
        .pending_keyword_hits
        .retain(|session, _| sessions.contains_key(session));
    state
        .session_last_heartbeat
        .retain(|session, _| sessions.contains_key(session));
    state
        .session_last_summarized
        .retain(|session, _| sessions.contains_key(session));

    Ok(())
}

async fn sync_active_config_registrations(
    config: &AppConfig,
    registry: &SharedTmuxRegistry,
    available_sessions: &HashSet<String>,
) {
    let existing_registry = registry.read().await.clone();
    let resolved = resolve_monitored_sessions(
        config
            .monitors
            .tmux
            .sessions
            .iter()
            .map(RegisteredTmuxSession::from)
            .collect(),
        Some(available_sessions),
    );
    let active_config = resolved
        .into_iter()
        .filter(|(session, _)| available_sessions.contains(session))
        .map(|(session, mut registration)| {
            if let Some(existing) = existing_registry.get(&session).filter(|existing| {
                !existing.active_wrapper_monitor
                    && existing.registration_source == RegistrationSource::ConfigMonitor
            }) {
                registration.registered_at = existing.registered_at.clone();
                registration.parent_process = existing.parent_process.clone();
            }
            (session, registration)
        })
        .collect();

    let mut write = registry.write().await;
    merge_active_config_registrations(&mut write, active_config);
}

fn merge_active_config_registrations(
    registry: &mut HashMap<String, RegisteredTmuxSession>,
    active_config: BTreeMap<String, RegisteredTmuxSession>,
) {
    let active_sessions: HashSet<String> = active_config.keys().cloned().collect();
    registry.retain(|session, registration| {
        registration.active_wrapper_monitor
            || registration.registration_source != RegistrationSource::ConfigMonitor
            || active_sessions.contains(session)
    });

    for (session, mut registration) in active_config {
        if let Some(existing) = registry.get(&session) {
            if existing.active_wrapper_monitor {
                continue;
            }
            if existing.registration_source == RegistrationSource::ConfigMonitor {
                registration.registered_at = existing.registered_at.clone();
                registration.parent_process = existing.parent_process.clone();
            }
        }
        registry.insert(session, registration);
    }
}

fn sorted_registry_snapshot(
    registry: &HashMap<String, RegisteredTmuxSession>,
) -> Vec<RegisteredTmuxSession> {
    let mut sessions: BTreeMap<String, RegisteredTmuxSession> = BTreeMap::new();
    for (session, registration) in registry {
        sessions.insert(session.clone(), registration.clone());
    }
    sessions.into_values().collect()
}

fn resolve_monitored_sessions(
    configured_sessions: Vec<RegisteredTmuxSession>,
    available_sessions: Option<&HashSet<String>>,
) -> BTreeMap<String, RegisteredTmuxSession> {
    let mut resolved: BTreeMap<String, (MonitorSpecificity, RegisteredTmuxSession)> =
        BTreeMap::new();

    for registration in configured_sessions {
        let specificity = MonitorSpecificity::for_pattern(&registration.session);
        let matched_sessions = available_sessions
            .into_iter()
            .flat_map(|sessions| sessions.iter())
            .filter(|session| glob_match(&registration.session, session))
            .cloned()
            .collect::<Vec<_>>();

        if matched_sessions.is_empty() {
            if !is_session_pattern(&registration.session) {
                insert_resolved_session(
                    &mut resolved,
                    registration.session.clone(),
                    specificity,
                    registration,
                );
            }
            continue;
        }

        for session in matched_sessions {
            let mut registration = registration.clone();
            registration.session = session.clone();
            insert_resolved_session(&mut resolved, session, specificity, registration);
        }
    }

    resolved
        .into_iter()
        .map(|(session, (_, registration))| (session, registration))
        .collect()
}

fn is_session_pattern(session: &str) -> bool {
    session.contains('*')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MonitorSpecificity {
    exact_match: bool,
    literal_chars: usize,
    wildcard_count: usize,
}

impl MonitorSpecificity {
    fn for_pattern(pattern: &str) -> Self {
        Self {
            exact_match: !is_session_pattern(pattern),
            literal_chars: pattern.chars().filter(|ch| *ch != '*').count(),
            wildcard_count: pattern.chars().filter(|ch| *ch == '*').count(),
        }
    }

    fn outranks(self, other: Self) -> bool {
        if self.exact_match != other.exact_match {
            return self.exact_match;
        }
        if self.literal_chars != other.literal_chars {
            return self.literal_chars > other.literal_chars;
        }

        self.wildcard_count < other.wildcard_count
    }
}

fn insert_resolved_session(
    resolved: &mut BTreeMap<String, (MonitorSpecificity, RegisteredTmuxSession)>,
    session: String,
    specificity: MonitorSpecificity,
    registration: RegisteredTmuxSession,
) {
    match resolved.get(&session) {
        Some((existing_specificity, _)) if !specificity.outranks(*existing_specificity) => {}
        _ => {
            resolved.insert(session, (specificity, registration));
        }
    }
}

fn should_emit_stale(pane: &TmuxPaneState, now: Instant, stale_minutes: u64) -> bool {
    if stale_minutes == 0 || pane.pane_dead {
        return false;
    }
    let stale_after = Duration::from_secs(stale_minutes * 60);
    now.duration_since(pane.last_change) >= stale_after
        && pane
            .last_stale_notification
            .map(|previous| now.duration_since(previous) >= stale_after)
            .unwrap_or(true)
}

fn tmux_keyword_event(
    registration: &RegisteredTmuxSession,
    session: String,
    hits: Vec<(String, String)>,
) -> IncomingEvent {
    let event = if hits.len() <= 1 {
        match hits.into_iter().next() {
            Some((keyword, line)) => {
                IncomingEvent::tmux_keyword(session, keyword, line, registration.channel.clone())
            }
            None => IncomingEvent::tmux_keyword(
                session,
                String::new(),
                String::new(),
                registration.channel.clone(),
            ),
        }
    } else {
        IncomingEvent::tmux_keywords(session, hits, registration.channel.clone())
    };

    event
        .with_routing_metadata(&registration.routing)
        .with_mention(registration.mention.clone())
        .with_format(registration.format.clone())
}

fn tmux_stale_event(
    registration: &RegisteredTmuxSession,
    session: String,
    pane: String,
    last_line: String,
) -> IncomingEvent {
    IncomingEvent::tmux_stale(
        session,
        pane,
        registration.stale_minutes,
        last_line,
        registration.channel.clone(),
    )
    .with_routing_metadata(&registration.routing)
    .with_mention(registration.mention.clone())
    .with_format(registration.format.clone())
}

fn tmux_content_changed_event(
    registration: &RegisteredTmuxSession,
    session: String,
    pane: String,
    content: SummarizedContent,
) -> IncomingEvent {
    IncomingEvent::tmux_content_changed_with_metadata(
        session,
        pane,
        content.summary,
        content.raw_truncated,
        content.backend,
        content.content_mode.as_str().to_string(),
        registration.channel.clone(),
    )
    .with_mention(registration.mention.clone())
    .with_format(registration.format.clone())
}

fn spawn_content_changed_task<E>(
    emitter: E,
    registration: RegisteredTmuxSession,
    session_name: String,
    pane_name: String,
    content: String,
    providers: crate::config::ProvidersConfig,
) where
    E: EventEmitter + Clone + Send + Sync + 'static,
{
    tokio::spawn(async move {
        match build_summarizer(&registration.summarizer, &providers) {
            Ok(summarizer) => match summarizer.summarize(&content, &session_name).await {
                Ok(transformed) => {
                    let event = tmux_content_changed_event(
                        &registration,
                        session_name,
                        pane_name,
                        transformed,
                    );
                    let _ = emitter.emit(event).await;
                }
                Err(error) => {
                    eprintln!("clawhip: summarize failed for {session_name}: {error}");
                }
            },
            Err(error) => {
                eprintln!(
                    "clawhip: could not initialize summarizer '{}' for {session_name}: {error}",
                    registration.summarizer
                );
            }
        }
    });
}

fn tmux_heartbeat_event(
    registration: &RegisteredTmuxSession,
    session: String,
    minutes_since_change: u64,
) -> IncomingEvent {
    IncomingEvent::tmux_heartbeat(session, minutes_since_change, registration.channel.clone())
        .with_mention(registration.mention.clone())
        .with_format(registration.format.clone())
}

fn count_new_lines(old: &str, new: &str) -> usize {
    new.lines().count().saturating_sub(old.lines().count())
}

fn should_summarize_now(
    last_summarized: Option<Instant>,
    interval_mins: u64,
    min_new_lines: usize,
    old_content: &str,
    new_content: &str,
    now: Instant,
) -> bool {
    if min_new_lines > 0 && count_new_lines(old_content, new_content) < min_new_lines {
        return false;
    }
    if interval_mins == 0 {
        return true;
    }
    last_summarized
        .map(|t| now.duration_since(t) >= Duration::from_secs(interval_mins * 60))
        .unwrap_or(true)
}

async fn maybe_emit_session_heartbeat<E: EventEmitter>(
    session_name: &str,
    registration: &RegisteredTmuxSession,
    emitter: &E,
    state: &mut TmuxMonitorState,
    now: Instant,
    session_changed: bool,
) -> Result<()> {
    if registration.effective_heartbeat_mins() == 0 {
        state.session_last_heartbeat.remove(session_name);
        return Ok(());
    }

    if session_changed {
        state
            .session_last_heartbeat
            .insert(session_name.to_string(), now);
    }

    let interval = Duration::from_secs(registration.effective_heartbeat_mins() * 60);
    let Some(last_change) = state
        .panes
        .values()
        .filter(|pane| pane.session == session_name)
        .map(|pane| pane.last_change)
        .max()
    else {
        state
            .session_last_heartbeat
            .entry(session_name.to_string())
            .or_insert(now);
        return Ok(());
    };

    let last_heartbeat = state
        .session_last_heartbeat
        .entry(session_name.to_string())
        .or_insert(now);
    if now.duration_since(last_change) < interval || now.duration_since(*last_heartbeat) < interval
    {
        return Ok(());
    }

    emitter
        .emit(tmux_heartbeat_event(
            registration,
            session_name.to_string(),
            now.duration_since(last_change).as_secs() / 60,
        ))
        .await?;
    *last_heartbeat = now;
    Ok(())
}

async fn maybe_emit_registered_session_heartbeat<E: EventEmitter>(
    registration: &RegisteredTmuxSession,
    emitter: &E,
    panes: &HashMap<String, TmuxPaneState>,
    last_heartbeat: &mut Option<Instant>,
    now: Instant,
) -> Result<()> {
    if registration.effective_heartbeat_mins() == 0 {
        *last_heartbeat = None;
        return Ok(());
    }

    let interval = Duration::from_secs(registration.effective_heartbeat_mins() * 60);
    let Some(last_change) = panes.values().map(|pane| pane.last_change).max() else {
        last_heartbeat.get_or_insert(now);
        return Ok(());
    };

    let last_heartbeat_at = last_heartbeat.get_or_insert(now);
    if now.duration_since(last_change) < interval
        || now.duration_since(*last_heartbeat_at) < interval
    {
        return Ok(());
    }

    emitter
        .emit(tmux_heartbeat_event(
            registration,
            registration.session.clone(),
            now.duration_since(last_change).as_secs() / 60,
        ))
        .await?;
    *last_heartbeat_at = now;
    Ok(())
}

async fn flush_pending_keyword_hits<E: EventEmitter>(
    pending_keyword_hits: &mut Option<PendingKeywordHits>,
    registration: &RegisteredTmuxSession,
    emitter: &E,
    session: &str,
    now: Instant,
    keyword_window: Duration,
    force: bool,
) -> Result<()> {
    let should_flush = pending_keyword_hits
        .as_ref()
        .map(|pending| force || pending.ready_to_flush(now, keyword_window))
        .unwrap_or(false);
    if !should_flush {
        return Ok(());
    }

    let Some(pending) = pending_keyword_hits.take() else {
        return Ok(());
    };
    let hits = pending
        .into_hits()
        .into_iter()
        .map(|hit| (hit.keyword, hit.line))
        .collect::<Vec<_>>();
    if hits.is_empty() {
        return Ok(());
    }

    emitter
        .emit(tmux_keyword_event(registration, session.to_string(), hits))
        .await
}

async fn flush_session_pending_keyword_hits<E: EventEmitter>(
    pending_keyword_hits: &mut HashMap<String, PendingKeywordHits>,
    session: &str,
    registration: &RegisteredTmuxSession,
    emitter: &E,
    now: Instant,
    force: bool,
) -> Result<()> {
    let mut pending = pending_keyword_hits.remove(session);
    flush_pending_keyword_hits(
        &mut pending,
        registration,
        emitter,
        session,
        now,
        Duration::from_secs(registration.keyword_window_secs.max(1)),
        force,
    )
    .await?;
    if let Some(pending) = pending {
        pending_keyword_hits.insert(session.to_string(), pending);
    }
    Ok(())
}

fn push_pending_keyword_hits(
    pending_keyword_hits: &mut Option<PendingKeywordHits>,
    now: Instant,
    hits: Vec<crate::keyword_window::KeywordHit>,
) {
    if hits.is_empty() {
        return;
    }

    pending_keyword_hits
        .get_or_insert_with(|| PendingKeywordHits::new(now))
        .push(hits);
}

fn push_session_pending_keyword_hits(
    pending_keyword_hits: &mut HashMap<String, PendingKeywordHits>,
    session: &str,
    now: Instant,
    hits: Vec<crate::keyword_window::KeywordHit>,
) {
    if hits.is_empty() {
        return;
    }

    pending_keyword_hits
        .entry(session.to_string())
        .or_insert_with(|| PendingKeywordHits::new(now))
        .push(hits);
}

pub(crate) async fn session_exists(session: &str) -> Result<bool> {
    let output = Command::new(tmux_bin())
        .arg("has-session")
        .arg("-t")
        .arg(session)
        .output()
        .await?;
    Ok(output.status.success())
}

async fn list_tmux_sessions() -> Result<HashSet<String>> {
    let output = Command::new(tmux_bin())
        .arg("list-sessions")
        .arg("-F")
        .arg("#{session_name}")
        .output()
        .await?;
    if !output.status.success() {
        return Err(tmux_stderr(&output.stderr).into());
    }

    Ok(String::from_utf8(output.stdout)?
        .lines()
        .map(str::trim)
        .filter(|session| !session.is_empty())
        .map(ToString::to_string)
        .collect())
}

async fn snapshot_tmux_session(session: &str) -> Result<Vec<TmuxPaneSnapshot>> {
    let output = Command::new(tmux_bin())
        .arg("list-panes")
        .arg("-t")
        .arg(session)
        .arg("-F")
        .arg("#{pane_id}|#{session_name}|#{window_index}.#{pane_index}|#{pane_dead}|#{pane_title}")
        .output()
        .await?;
    if !output.status.success() {
        return Err(tmux_stderr(&output.stderr).into());
    }

    let mut panes = Vec::new();
    for line in String::from_utf8(output.stdout)?.lines() {
        let mut parts = line.splitn(5, '|');
        let pane_id = parts.next().unwrap_or_default().to_string();
        if pane_id.is_empty() {
            continue;
        }
        let session_name = parts.next().unwrap_or_default().to_string();
        let pane_name = parts.next().unwrap_or_default().to_string();
        let pane_dead = parts.next().unwrap_or_default() == "1";
        let capture = Command::new(tmux_bin())
            .arg("capture-pane")
            .arg("-p")
            .arg("-t")
            .arg(&pane_id)
            .arg("-S")
            .arg("-200")
            .output()
            .await?;
        if !capture.status.success() {
            return Err(tmux_stderr(&capture.stderr).into());
        }
        panes.push(TmuxPaneSnapshot {
            pane_id,
            session: session_name,
            pane_name,
            content: String::from_utf8(capture.stdout)?,
            pane_dead,
        });
    }
    Ok(panes)
}

pub(crate) fn content_hash(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

pub(crate) fn last_nonempty_line(content: &str) -> String {
    content
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("<no output>")
        .trim()
        .to_string()
}

pub(crate) fn tmux_bin() -> String {
    std::env::var("CLAWHIP_TMUX_BIN").unwrap_or_else(|_| "tmux".to_string())
}

fn tmux_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr).trim().to_string()
}

fn default_keyword_window_secs() -> u64 {
    30
}

pub fn current_timestamp_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventBody, compat::from_incoming_event};
    use crate::keyword_window::KeywordHit;

    fn registration(keywords: Vec<&str>) -> RegisteredTmuxSession {
        RegisteredTmuxSession {
            session: "issue-24".into(),
            channel: Some("alerts".into()),
            mention: Some("<@123>".into()),
            routing: RoutingMetadata::default(),
            keywords: keywords.into_iter().map(str::to_string).collect(),
            keyword_window_secs: 30,
            stale_minutes: 15,
            format: Some(MessageFormat::Compact),
            registered_at: "2026-04-02T00:00:00Z".into(),
            registration_source: RegistrationSource::ConfigMonitor,
            parent_process: None,
            active_wrapper_monitor: false,
            ..Default::default()
        }
    }

    #[test]
    fn keyword_hits_only_emit_for_new_lines() {
        let hits = collect_keyword_hits(
            "done
all good",
            "done
all good
error: failed
PR created #7",
            &["error".into(), "PR created".into()],
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].keyword, "error");
        assert_eq!(hits[1].keyword, "PR created");
    }

    #[test]
    fn tmux_keyword_event_inherits_channel_format_and_mention() {
        let mut registration = registration(vec!["error"]);
        registration.format = Some(MessageFormat::Alert);

        let event = tmux_keyword_event(
            &registration,
            "issue-24".into(),
            vec![("error".into(), "boom".into())],
        );

        assert_eq!(event.channel.as_deref(), Some("alerts"));
        assert_eq!(event.mention.as_deref(), Some("<@123>"));
        assert!(matches!(event.format, Some(MessageFormat::Alert)));
        assert_eq!(event.payload["session"], "issue-24");
        assert_eq!(event.payload["keyword"], "error");
        assert_eq!(event.payload["line"], "boom");
        assert_eq!(event.payload["hit_count"], serde_json::Value::Null);
    }

    #[test]
    fn tmux_keyword_event_carries_registered_routing_metadata() {
        let mut registration = registration(vec!["error"]);
        registration.routing = RoutingMetadata {
            project: Some("clawhip".into()),
            repo_name: Some("clawhip".into()),
            worktree_path: Some("/repo/clawhip.worktrees/issue-152".into()),
            ..RoutingMetadata::default()
        };

        let event = tmux_keyword_event(
            &registration,
            "clawhip-issue-152".into(),
            vec![("error".into(), "boom".into())],
        );

        assert_eq!(event.payload["project"], "clawhip");
        assert_eq!(event.payload["repo_name"], "clawhip");
        assert_eq!(
            event.payload["worktree_path"],
            "/repo/clawhip.worktrees/issue-152"
        );
    }

    #[test]
    fn tmux_keyword_event_uses_aggregated_body_for_multi_hit_windows() {
        let mut registration = registration(vec!["error", "complete"]);
        registration.format = Some(MessageFormat::Alert);

        let event = tmux_keyword_event(
            &registration,
            "issue-24".into(),
            vec![
                ("error".into(), "boom".into()),
                ("complete".into(), "done".into()),
            ],
        );

        match from_incoming_event(&event).unwrap().body {
            EventBody::TmuxKeywordAggregated(body) => {
                assert_eq!(body.session, "issue-24");
                assert_eq!(body.hit_count, 2);
                assert_eq!(body.hits.len(), 2);
            }
            other => panic!("expected aggregated tmux keyword body, got {other:?}"),
        }
    }

    #[test]
    fn tmux_stale_event_inherits_channel_format_and_mention() {
        let mut registration = registration(vec!["error"]);
        registration.format = Some(MessageFormat::Inline);

        let event = tmux_stale_event(
            &registration,
            "issue-24".into(),
            "0.0".into(),
            "waiting".into(),
        );

        assert_eq!(event.channel.as_deref(), Some("alerts"));
        assert_eq!(event.mention.as_deref(), Some("<@123>"));
        assert!(matches!(event.format, Some(MessageFormat::Inline)));
        assert_eq!(event.payload["session"], "issue-24");
        assert_eq!(event.payload["pane"], "0.0");
        assert_eq!(event.payload["minutes"], 15);
        assert_eq!(event.payload["last_line"], "waiting");
    }

    #[test]
    fn config_monitor_registration_sets_audit_defaults() {
        let monitor = TmuxSessionMonitor {
            session: "issue-*".into(),
            channel: Some("alerts".into()),
            mention: None,
            keywords: vec!["panic".into()],
            keyword_window_secs: 30,
            stale_minutes: 10,
            format: None,
            ..Default::default()
        };

        let registration = RegisteredTmuxSession::from(&monitor);

        assert!(matches!(
            registration.registration_source,
            RegistrationSource::ConfigMonitor
        ));
        assert!(!registration.registered_at.is_empty());
        assert!(registration.parent_process.is_none());
    }

    #[test]
    fn merge_active_config_registrations_preserves_existing_timestamps_and_prunes_inactive_ones() {
        let mut registry = HashMap::from([
            (
                "issue-105".into(),
                RegisteredTmuxSession {
                    session: "issue-105".into(),
                    channel: Some("alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["error".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    active_wrapper_monitor: false,
                    ..Default::default()
                },
            ),
            (
                "wrapper".into(),
                RegisteredTmuxSession {
                    session: "wrapper".into(),
                    channel: Some("alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T01:00:00Z".into(),
                    registration_source: RegistrationSource::CliWatch,
                    parent_process: Some(ParentProcessInfo {
                        pid: 42,
                        name: Some("codex".into()),
                    }),
                    active_wrapper_monitor: true,
                    ..Default::default()
                },
            ),
            (
                "stale-config".into(),
                RegisteredTmuxSession {
                    session: "stale-config".into(),
                    channel: Some("alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T02:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    active_wrapper_monitor: false,
                    ..Default::default()
                },
            ),
        ]);

        merge_active_config_registrations(
            &mut registry,
            BTreeMap::from([(
                "issue-105".into(),
                RegisteredTmuxSession {
                    session: "issue-105".into(),
                    channel: Some("alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["error".into(), "complete".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T09:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    active_wrapper_monitor: false,
                    ..Default::default()
                },
            )]),
        );

        assert_eq!(registry.len(), 2);
        assert_eq!(registry["issue-105"].registered_at, "2026-04-02T00:00:00Z");
        assert_eq!(registry["issue-105"].keywords, vec!["error", "complete"]);
        assert!(registry.contains_key("wrapper"));
        assert!(!registry.contains_key("stale-config"));
    }

    #[test]
    fn registered_tmux_session_deserializes_without_new_audit_fields() {
        let registration: RegisteredTmuxSession = serde_json::from_value(serde_json::json!({
            "session": "issue-24",
            "channel": "alerts",
            "mention": "<@123>",
            "keywords": ["panic"],
            "keyword_window_secs": 30,
            "stale_minutes": 10,
            "format": "compact",
            "active_wrapper_monitor": false
        }))
        .unwrap();

        assert!(matches!(
            registration.registration_source,
            RegistrationSource::ConfigMonitor
        ));
        assert!(registration.parent_process.is_none());
        assert!(!registration.registered_at.is_empty());
    }

    #[tokio::test]
    async fn flush_pending_keyword_hits_aggregates_unique_hits() {
        let (tx, mut rx) = mpsc::channel(1);
        let registration = RegisteredTmuxSession {
            format: Some(MessageFormat::Compact),
            mention: None,
            routing: RoutingMetadata::default(),
            ..registration(vec!["error", "complete"])
        };
        let start = Instant::now();
        let mut pending_keyword_hits = Some({
            let mut pending = PendingKeywordHits::new(start);
            pending.push(vec![
                KeywordHit {
                    keyword: "error".into(),
                    line: "error: failed".into(),
                },
                KeywordHit {
                    keyword: "error".into(),
                    line: "error: failed".into(),
                },
                KeywordHit {
                    keyword: "complete".into(),
                    line: "complete".into(),
                },
            ]);
            pending
        });

        flush_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration,
            &tx,
            &registration.session,
            start + Duration::from_secs(30),
            Duration::from_secs(30),
            false,
        )
        .await
        .unwrap();

        assert!(pending_keyword_hits.is_none());
        let event = rx.recv().await.unwrap();
        assert_eq!(event.canonical_kind(), "tmux.keyword");
        assert_eq!(event.payload["hit_count"], 2);
    }

    #[tokio::test]
    async fn flush_pending_keyword_hits_clears_window_after_send_attempt() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let registration = RegisteredTmuxSession {
            format: Some(MessageFormat::Compact),
            mention: None,
            routing: RoutingMetadata::default(),
            ..registration(vec!["error", "complete"])
        };
        let start = Instant::now();
        let mut pending_keyword_hits = Some({
            let mut pending = PendingKeywordHits::new(start);
            pending.push(vec![KeywordHit {
                keyword: "error".into(),
                line: "boom".into(),
            }]);
            pending
        });

        let result = flush_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration,
            &tx,
            &registration.session,
            start + Duration::from_secs(30),
            Duration::from_secs(30),
            false,
        )
        .await;

        assert!(result.is_err());
        assert!(pending_keyword_hits.is_none());
    }

    #[tokio::test]
    async fn identical_keyword_lines_can_emit_again_after_window_flush() {
        let (tx, mut rx) = mpsc::channel(4);
        let registration = RegisteredTmuxSession {
            format: Some(MessageFormat::Compact),
            mention: None,
            routing: RoutingMetadata::default(),
            ..registration(vec!["error"])
        };
        let start = Instant::now();
        let mut snapshot = "done".to_string();
        let mut pending_keyword_hits = None;

        let first_snapshot = "done
error: failed";
        let first_hits = collect_keyword_hits(&snapshot, first_snapshot, &registration.keywords);
        push_pending_keyword_hits(&mut pending_keyword_hits, start, first_hits);
        snapshot = first_snapshot.into();

        flush_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration,
            &tx,
            &registration.session,
            start + Duration::from_secs(30),
            Duration::from_secs(30),
            false,
        )
        .await
        .unwrap();

        let first_event = rx.recv().await.unwrap();
        assert_eq!(first_event.payload["hit_count"], serde_json::Value::Null);
        assert_eq!(first_event.payload["keyword"], "error");
        assert_eq!(first_event.payload["line"], "error: failed");

        let second_snapshot = "done
error: failed
error: failed";
        let second_hits = collect_keyword_hits(&snapshot, second_snapshot, &registration.keywords);
        push_pending_keyword_hits(
            &mut pending_keyword_hits,
            start + Duration::from_secs(31),
            second_hits,
        );

        flush_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration,
            &tx,
            &registration.session,
            start + Duration::from_secs(61),
            Duration::from_secs(30),
            false,
        )
        .await
        .unwrap();

        let second_event = rx.recv().await.unwrap();
        assert_eq!(second_event.payload["hit_count"], serde_json::Value::Null);
        assert_eq!(second_event.payload["keyword"], "error");
        assert_eq!(second_event.payload["line"], "error: failed");
    }

    #[tokio::test]
    async fn session_keyword_hits_aggregate_across_panes_and_dedup_within_window() {
        let (tx, mut rx) = mpsc::channel(1);
        let registration = RegisteredTmuxSession {
            format: Some(MessageFormat::Compact),
            mention: None,
            routing: RoutingMetadata::default(),
            ..registration(vec!["error", "complete"])
        };
        let start = Instant::now();
        let mut pending_keyword_hits = HashMap::new();

        push_session_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration.session,
            start,
            vec![KeywordHit {
                keyword: "error".into(),
                line: "error: failed".into(),
            }],
        );
        push_session_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration.session,
            start + Duration::from_secs(5),
            vec![
                KeywordHit {
                    keyword: "error".into(),
                    line: "error: failed".into(),
                },
                KeywordHit {
                    keyword: "complete".into(),
                    line: "build complete".into(),
                },
            ],
        );

        flush_session_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration.session,
            &registration,
            &tx,
            start + Duration::from_secs(30),
            false,
        )
        .await
        .unwrap();

        assert!(pending_keyword_hits.is_empty());
        let event = rx.recv().await.unwrap();
        match from_incoming_event(&event).unwrap().body {
            EventBody::TmuxKeywordAggregated(body) => {
                assert_eq!(body.hit_count, 2);
                assert_eq!(body.hits.len(), 2);
            }
            other => panic!("expected aggregated tmux keyword body, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_keyword_hits_flush_when_window_expires() {
        let (tx, mut rx) = mpsc::channel(1);
        let registration = RegisteredTmuxSession {
            format: Some(MessageFormat::Compact),
            mention: None,
            routing: RoutingMetadata::default(),
            ..registration(vec!["error"])
        };
        let start = Instant::now();
        let mut pending_keyword_hits = HashMap::new();
        push_session_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration.session,
            start,
            vec![KeywordHit {
                keyword: "error".into(),
                line: "error: failed".into(),
            }],
        );

        flush_session_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration.session,
            &registration,
            &tx,
            start + Duration::from_secs(29),
            false,
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());
        assert!(pending_keyword_hits.contains_key(&registration.session));

        flush_session_pending_keyword_hits(
            &mut pending_keyword_hits,
            &registration.session,
            &registration,
            &tx,
            start + Duration::from_secs(30),
            false,
        )
        .await
        .unwrap();

        assert!(pending_keyword_hits.is_empty());
        let event = rx.recv().await.unwrap();
        assert_eq!(event.payload["keyword"], "error");
        assert_eq!(event.payload["line"], "error: failed");
    }

    #[test]
    fn resolve_monitored_sessions_expands_glob_patterns_to_actual_sessions() {
        let available_sessions = HashSet::from([
            "rcc-api".to_string(),
            "rcc-web".to_string(),
            "other".to_string(),
        ]);
        let resolved = resolve_monitored_sessions(
            vec![RegisteredTmuxSession {
                session: "rcc-*".into(),
                channel: Some("alerts".into()),
                mention: None,
                routing: RoutingMetadata::default(),
                keywords: vec!["panic".into()],
                keyword_window_secs: 30,
                stale_minutes: 10,
                format: None,
                registered_at: "2026-04-02T00:00:00Z".into(),
                registration_source: RegistrationSource::ConfigMonitor,
                parent_process: None,
                active_wrapper_monitor: false,
                ..Default::default()
            }],
            Some(&available_sessions),
        );

        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved["rcc-api"].session, "rcc-api");
        assert_eq!(resolved["rcc-api"].channel.as_deref(), Some("alerts"));
        assert_eq!(resolved["rcc-api"].keywords, vec!["panic"]);
        assert_eq!(resolved["rcc-web"].session, "rcc-web");
        assert_eq!(resolved["rcc-web"].channel.as_deref(), Some("alerts"));
    }

    #[test]
    fn resolve_monitored_sessions_keeps_keywords_isolated_per_actual_session() {
        let available_sessions = HashSet::from(["rcc-prod".to_string(), "omx-prod".to_string()]);
        let resolved = resolve_monitored_sessions(
            vec![
                RegisteredTmuxSession {
                    session: "rcc-*".into(),
                    channel: Some("rcc-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    active_wrapper_monitor: false,
                    ..Default::default()
                },
                RegisteredTmuxSession {
                    session: "omx-*".into(),
                    channel: Some("omx-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["error".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    active_wrapper_monitor: false,
                    ..Default::default()
                },
            ],
            Some(&available_sessions),
        );

        assert_eq!(resolved["rcc-prod"].keywords, vec!["panic"]);
        assert_eq!(resolved["rcc-prod"].channel.as_deref(), Some("rcc-alerts"));
        assert_eq!(resolved["omx-prod"].keywords, vec!["error"]);
        assert_eq!(resolved["omx-prod"].channel.as_deref(), Some("omx-alerts"));
    }

    #[test]
    fn resolve_monitored_sessions_keeps_exact_sessions_when_listing_is_unavailable() {
        let resolved = resolve_monitored_sessions(
            vec![
                RegisteredTmuxSession {
                    session: "exact-session".into(),
                    channel: Some("alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    active_wrapper_monitor: false,
                    ..Default::default()
                },
                RegisteredTmuxSession {
                    session: "rcc-*".into(),
                    channel: Some("alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    active_wrapper_monitor: false,
                    ..Default::default()
                },
            ],
            None,
        );

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved["exact-session"].session, "exact-session");
    }

    #[test]
    fn resolve_monitored_sessions_prefers_exact_match_over_glob_overlap() {
        let available_sessions = HashSet::from(["rcc-api".to_string()]);
        let resolved = resolve_monitored_sessions(
            vec![
                RegisteredTmuxSession {
                    session: "*".into(),
                    channel: Some("default-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["error".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    active_wrapper_monitor: false,
                    ..Default::default()
                },
                RegisteredTmuxSession {
                    session: "rcc-api".into(),
                    channel: Some("rcc-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    active_wrapper_monitor: false,
                    ..Default::default()
                },
            ],
            Some(&available_sessions),
        );

        assert_eq!(resolved["rcc-api"].channel.as_deref(), Some("rcc-alerts"));
        assert_eq!(resolved["rcc-api"].keywords, vec!["panic"]);
    }

    #[test]
    fn resolve_monitored_sessions_prefers_more_specific_glob_over_broader_glob() {
        let available_sessions = HashSet::from(["rcc-api".to_string(), "omx-api".to_string()]);
        let resolved = resolve_monitored_sessions(
            vec![
                RegisteredTmuxSession {
                    session: "*".into(),
                    channel: Some("default-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["error".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    active_wrapper_monitor: false,
                    ..Default::default()
                },
                RegisteredTmuxSession {
                    session: "rcc-*".into(),
                    channel: Some("rcc-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    active_wrapper_monitor: false,
                    ..Default::default()
                },
            ],
            Some(&available_sessions),
        );

        assert_eq!(resolved["rcc-api"].channel.as_deref(), Some("rcc-alerts"));
        assert_eq!(resolved["rcc-api"].keywords, vec!["panic"]);
        assert_eq!(
            resolved["omx-api"].channel.as_deref(),
            Some("default-alerts")
        );
        assert_eq!(resolved["omx-api"].keywords, vec!["error"]);
    }

    #[test]
    fn resolve_monitored_sessions_breaks_same_literal_ties_with_fewer_wildcards() {
        let available_sessions = HashSet::from(["abc-prod".to_string()]);
        let resolved = resolve_monitored_sessions(
            vec![
                RegisteredTmuxSession {
                    session: "*abc*".into(),
                    channel: Some("broad-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["error".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    active_wrapper_monitor: false,
                    ..Default::default()
                },
                RegisteredTmuxSession {
                    session: "abc*".into(),
                    channel: Some("specific-alerts".into()),
                    mention: None,
                    routing: RoutingMetadata::default(),
                    keywords: vec!["panic".into()],
                    keyword_window_secs: 30,
                    stale_minutes: 10,
                    format: None,
                    registered_at: "2026-04-02T00:00:00Z".into(),
                    registration_source: RegistrationSource::ConfigMonitor,
                    parent_process: None,
                    active_wrapper_monitor: false,
                    ..Default::default()
                },
            ],
            Some(&available_sessions),
        );

        assert_eq!(
            resolved["abc-prod"].channel.as_deref(),
            Some("specific-alerts")
        );
        assert_eq!(resolved["abc-prod"].keywords, vec!["panic"]);
    }

    #[test]
    fn stale_minutes_zero_disables_stale_detection() {
        let pane = TmuxPaneState {
            session: "test".into(),
            pane_name: "0.0".into(),
            snapshot: String::new(),
            content_hash: 0,
            last_change: Instant::now() - Duration::from_secs(3600),
            last_stale_notification: None,
            pane_dead: false,
        };
        // stale_minutes=0 should never emit, even after 1 hour idle
        assert!(!should_emit_stale(&pane, Instant::now(), 0));
    }

    #[test]
    fn stale_minutes_nonzero_still_emits() {
        let pane = TmuxPaneState {
            session: "test".into(),
            pane_name: "0.0".into(),
            snapshot: String::new(),
            content_hash: 0,
            last_change: Instant::now() - Duration::from_secs(3600),
            last_stale_notification: None,
            pane_dead: false,
        };
        // stale_minutes=1 should emit after 1 hour idle
        assert!(should_emit_stale(&pane, Instant::now(), 1));
    }

    #[test]
    fn pane_dead_suppresses_stale_alert() {
        let pane = TmuxPaneState {
            session: "test".into(),
            pane_name: "0.0".into(),
            snapshot: String::new(),
            content_hash: 0,
            last_change: Instant::now() - Duration::from_secs(3600),
            last_stale_notification: None,
            pane_dead: true,
        };
        // Dead pane should never emit stale, even after 1 hour idle
        assert!(!should_emit_stale(&pane, Instant::now(), 1));
    }

    #[test]
    fn count_new_lines_no_change_returns_zero() {
        assert_eq!(count_new_lines("a\nb\n", "a\nb\n"), 0);
    }

    #[test]
    fn count_new_lines_fewer_lines_returns_zero() {
        assert_eq!(count_new_lines("a\nb\nc\n", "a\n"), 0);
    }

    #[test]
    fn count_new_lines_returns_net_addition() {
        assert_eq!(count_new_lines("a\nb\n", "a\nb\nc\nd\n"), 2);
    }

    #[test]
    fn should_summarize_now_no_filter_no_throttle() {
        assert!(should_summarize_now(None, 0, 0, "old", "new", Instant::now()));
    }

    #[test]
    fn should_summarize_now_no_prior_summarize_always_allowed_with_throttle() {
        assert!(should_summarize_now(None, 5, 0, "old", "new", Instant::now()));
    }

    #[test]
    fn should_summarize_now_interval_allows_when_expired() {
        let now = Instant::now();
        let old_enough = now - Duration::from_secs(6 * 60);
        assert!(should_summarize_now(Some(old_enough), 5, 0, "old", "new", now));
    }

    #[test]
    fn should_summarize_now_interval_blocks_when_too_soon() {
        let now = Instant::now();
        let recent = now - Duration::from_secs(30);
        // interval_mins=5 but only 30 seconds elapsed
        assert!(!should_summarize_now(Some(recent), 5, 0, "old", "new", now));
    }

    #[test]
    fn should_summarize_now_min_new_lines_allows_when_met() {
        let old = "a\n".repeat(10);
        let new = format!("{}{}", old, "b\n".repeat(5));
        assert!(should_summarize_now(None, 0, 5, &old, &new, Instant::now()));
    }

    #[test]
    fn should_summarize_now_min_new_lines_blocks_when_insufficient() {
        assert!(!should_summarize_now(None, 0, 5, "a\nb\n", "a\nb\nc\n", Instant::now()));
    }
}
