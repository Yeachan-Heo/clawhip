use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep};

use crate::Result;
use crate::config::{AppConfig, WorkspaceMonitor};
use crate::events::IncomingEvent;
#[cfg(test)]
use crate::events::MessageFormat;
use crate::source::Source;

pub struct WorkspaceSource {
    config: Arc<AppConfig>,
}

impl WorkspaceSource {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }
}

#[async_trait::async_trait]
impl Source for WorkspaceSource {
    fn name(&self) -> &str {
        "workspace"
    }

    async fn run(&self, tx: mpsc::Sender<IncomingEvent>) -> Result<()> {
        if self.config.monitors.workspace.is_empty() {
            return Ok(());
        }

        if cfg!(target_os = "linux") && inotifywait_available().await {
            match run_with_inotify(self.config.as_ref(), &tx).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    eprintln!("clawhip source workspace falling back to polling: {error}");
                }
            }
        }

        run_with_polling(self.config.as_ref(), &tx).await
    }
}

#[derive(Debug, Clone)]
struct WorkspaceState {
    signatures: HashMap<PathBuf, FileSignature>,
    snapshots: HashMap<PathBuf, Value>,
    pending: HashMap<PathBuf, PendingChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSignature {
    modified_ms: u128,
    len: u64,
}

#[derive(Debug, Clone)]
struct PendingChange {
    due_at: Instant,
}

#[derive(Debug, Clone)]
struct MonitorTopology<'a> {
    monitor: &'a WorkspaceMonitor,
    workspace_root: PathBuf,
    workspace_name: String,
    watch_dirs: Vec<WatchDir>,
}

#[derive(Debug, Clone)]
struct WatchDir {
    path: PathBuf,
    state_family: String,
}

#[derive(Debug, Clone)]
struct WorkspaceMatch<'a> {
    monitor: &'a WorkspaceMonitor,
    workspace_root: PathBuf,
    workspace_name: String,
    state_family: String,
    watch_dir: PathBuf,
    state_file: String,
}

impl WorkspaceState {
    fn new() -> Self {
        Self {
            signatures: HashMap::new(),
            snapshots: HashMap::new(),
            pending: HashMap::new(),
        }
    }

    fn note_change(&mut self, path: PathBuf, debounce: Duration, now: Instant) {
        self.pending.insert(
            path,
            PendingChange {
                due_at: now + debounce,
            },
        );
    }
}

async fn run_with_polling(config: &AppConfig, tx: &mpsc::Sender<IncomingEvent>) -> Result<()> {
    let mut state = WorkspaceState::new();
    prime_state(config, &mut state)?;

    loop {
        reconcile(config, tx, &mut state).await?;
        sleep(global_poll_interval(config)).await;
    }
}

async fn run_with_inotify(config: &AppConfig, tx: &mpsc::Sender<IncomingEvent>) -> Result<()> {
    let mut state = WorkspaceState::new();
    prime_state(config, &mut state)?;

    let mut roots = inotify_roots(config);
    if roots.is_empty() {
        return run_with_polling(config, tx).await;
    }

    let mut child = Command::new("inotifywait")
        .arg("-m")
        .arg("-r")
        .arg("--format")
        .arg("%w%f")
        .arg("-e")
        .arg("close_write,create,move,delete")
        .args(roots.iter().map(|root| root.as_os_str()))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "workspace inotifywait stdout unavailable".to_string())?;
    let mut lines = BufReader::new(stdout).lines();
    let mut tick = interval(global_poll_interval(config));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            line = lines.next_line() => {
                match line? {
                    Some(line) => {
                        let path = PathBuf::from(line.trim());
                        let debounce = debounce_for_path(config, &path);
                        state.note_change(path, debounce, Instant::now());
                        flush_due(config, tx, &mut state).await?;
                    }
                    None => {
                        let status = child.wait().await?;
                        return Err(format!("workspace inotifywait exited with {status}").into());
                    }
                }
            }
            _ = tick.tick() => {
                let refreshed_roots = inotify_roots(config);
                if refreshed_roots != roots {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return Err("workspace watch roots changed; restarting with polling fallback".into());
                }
                reconcile(config, tx, &mut state).await?;
                flush_due(config, tx, &mut state).await?;
                roots = refreshed_roots;
            }
        }
    }
}

async fn inotifywait_available() -> bool {
    Command::new("inotifywait")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

fn prime_state(config: &AppConfig, state: &mut WorkspaceState) -> Result<()> {
    let topology = build_topology(config);
    let signatures = snapshot_signatures(&topology);
    for path in signatures.keys() {
        if let Some(json) = read_json(path) {
            state.snapshots.insert(path.clone(), json);
        }
    }
    state.signatures = signatures;
    Ok(())
}

async fn reconcile(
    config: &AppConfig,
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut WorkspaceState,
) -> Result<()> {
    let topology = build_topology(config);
    let current = snapshot_signatures(&topology);
    let now = Instant::now();

    let mut changed = BTreeSet::new();
    for (path, signature) in &current {
        if state.signatures.get(path) != Some(signature) {
            changed.insert(path.clone());
        }
    }
    for path in state.signatures.keys() {
        if !current.contains_key(path) {
            changed.insert(path.clone());
        }
    }

    for path in changed {
        let debounce = debounce_for_path(config, &path);
        state.note_change(path, debounce, now);
    }
    state.signatures = current;
    flush_due_with_topology(&topology, tx, state).await
}

async fn flush_due(
    config: &AppConfig,
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut WorkspaceState,
) -> Result<()> {
    let topology = build_topology(config);
    flush_due_with_topology(&topology, tx, state).await
}

async fn flush_due_with_topology(
    topology: &[MonitorTopology<'_>],
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut WorkspaceState,
) -> Result<()> {
    let now = Instant::now();
    let due = state
        .pending
        .iter()
        .filter(|(_, pending)| pending.due_at <= now)
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();

    for path in due {
        state.pending.remove(&path);
        process_path(topology, tx, state, &path).await?;
    }

    Ok(())
}

async fn process_path(
    topology: &[MonitorTopology<'_>],
    tx: &mpsc::Sender<IncomingEvent>,
    state: &mut WorkspaceState,
    path: &Path,
) -> Result<()> {
    let Some(matched) = classify_path(topology, path) else {
        state.snapshots.remove(path);
        return Ok(());
    };

    let previous = state.snapshots.get(path).cloned();
    let current = if path.exists() { read_json(path) } else { None };

    let Some(events) = diff_workspace_state(&matched, previous.as_ref(), current.as_ref()) else {
        if let Some(current) = current {
            state.snapshots.insert(path.to_path_buf(), current);
        }
        return Ok(());
    };

    if let Some(current) = current {
        state.snapshots.insert(path.to_path_buf(), current);
    } else {
        state.snapshots.remove(path);
    }

    for event in events {
        tx.send(event)
            .await
            .map_err(|error| format!("workspace source channel closed: {error}"))?;
    }

    Ok(())
}

fn build_topology(config: &AppConfig) -> Vec<MonitorTopology<'_>> {
    let mut topology = Vec::new();

    for monitor in &config.monitors.workspace {
        let root = PathBuf::from(&monitor.path);
        for workspace_root in discover_workspace_roots(monitor) {
            let watch_dirs = monitor
                .watch_dirs
                .iter()
                .map(|watch_dir| WatchDir {
                    path: workspace_root.join(watch_dir),
                    state_family: infer_state_family(watch_dir),
                })
                .collect::<Vec<_>>();
            topology.push(MonitorTopology {
                monitor,
                workspace_name: workspace_name(&workspace_root),
                workspace_root,
                watch_dirs,
            });
        }
        if !root.exists() {
            topology.push(MonitorTopology {
                monitor,
                workspace_name: workspace_name(&root),
                workspace_root: root.clone(),
                watch_dirs: monitor
                    .watch_dirs
                    .iter()
                    .map(|watch_dir| WatchDir {
                        path: root.join(watch_dir),
                        state_family: infer_state_family(watch_dir),
                    })
                    .collect(),
            });
        }
    }

    topology.sort_by(|a, b| a.workspace_root.cmp(&b.workspace_root));
    topology
        .dedup_by(|a, b| a.workspace_root == b.workspace_root && a.monitor.path == b.monitor.path);
    topology
}

fn discover_workspace_roots(monitor: &WorkspaceMonitor) -> Vec<PathBuf> {
    let root = PathBuf::from(&monitor.path);
    let mut roots = vec![root.clone()];
    if monitor.discover_worktrees {
        let worktrees_root = root.join(".claude").join("worktrees");
        if let Ok(entries) = std::fs::read_dir(worktrees_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    roots.push(path);
                }
            }
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn snapshot_signatures(topology: &[MonitorTopology<'_>]) -> HashMap<PathBuf, FileSignature> {
    let mut signatures = HashMap::new();
    for entry in topology {
        for watch_dir in &entry.watch_dirs {
            collect_json_signatures(&watch_dir.path, &mut signatures);
        }
    }
    signatures
}

fn collect_json_signatures(dir: &Path, signatures: &mut HashMap<PathBuf, FileSignature>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_json_signatures(&path, signatures);
            continue;
        }
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        if let Some(signature) = file_signature(&path) {
            signatures.insert(path, signature);
        }
    }
}

fn file_signature(path: &Path) -> Option<FileSignature> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    Some(FileSignature {
        modified_ms,
        len: metadata.len(),
    })
}

fn classify_path<'a>(
    topology: &'a [MonitorTopology<'a>],
    path: &Path,
) -> Option<WorkspaceMatch<'a>> {
    topology.iter().find_map(|entry| {
        entry.watch_dirs.iter().find_map(|watch_dir| {
            let relative = path.strip_prefix(&watch_dir.path).ok()?;
            Some(WorkspaceMatch {
                monitor: entry.monitor,
                workspace_root: entry.workspace_root.clone(),
                workspace_name: entry.workspace_name.clone(),
                state_family: watch_dir.state_family.clone(),
                watch_dir: watch_dir.path.clone(),
                state_file: relative_to_string(relative),
            })
        })
    })
}

fn diff_workspace_state(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    let events = match matched.state_file.as_str() {
        "session.json" => diff_session(matched, previous, current),
        "hud-state.json" => diff_hud_state(matched, previous, current),
        "notify-hook-state.json" => diff_notify_hook_state(matched, previous, current),
        "skill-active-state.json" => diff_skill_state(matched, previous, current),
        "metrics.json" => diff_metrics_state(matched, previous, current),
        "tmux-hook-state.json" => diff_tmux_hook_state(matched, previous, current),
        "team-leader-nudge.json" => diff_team_leader_nudge(matched, previous, current),
        "notify-fallback-state.json" => diff_notify_fallback_state(matched, previous, current),
        "mission-state.json" => diff_mission_state(matched, previous, current),
        "team-state.json" => diff_team_state(matched, previous, current),
        "subagent-tracking.json" => diff_subagent_tracking(matched, previous, current),
        other if other.starts_with("checkpoints/") => {
            diff_checkpoint_state(matched, previous, current)
        }
        other if other.ends_with("idle-notif-cooldown.json") => {
            diff_idle_notif_state(matched, previous, current)
        }
        _ => None,
    }?;

    let filtered = events
        .into_iter()
        .filter(|event| monitor_allows_event(matched.monitor, event.canonical_kind()))
        .collect::<Vec<_>>();
    (!filtered.is_empty()).then_some(filtered)
}

fn diff_session(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    match (previous, current) {
        (None, Some(current)) => Some(vec![workspace_event(
            matched,
            "workspace.session.started",
            base_payload(matched, current)
                .with_string("session_id", string_value(current, "session_id"))
                .with_string("started_at", string_value(current, "started_at"))
                .with_u64("pid", u64_value(current, "pid"))
                .with_string("cwd", string_value(current, "cwd"))
                .with_string("summary", Some("workspace session started".into()))
                .into_value(),
        )]),
        (Some(previous), Some(current)) => {
            let prev_session = string_value(previous, "session_id");
            let curr_session = string_value(current, "session_id");
            if prev_session != curr_session {
                return Some(vec![workspace_event(
                    matched,
                    "workspace.session.started",
                    base_payload(matched, current)
                        .with_string("session_id", curr_session)
                        .with_string("previous_session_id", prev_session)
                        .with_string("started_at", string_value(current, "started_at"))
                        .with_u64("pid", u64_value(current, "pid"))
                        .with_string("cwd", string_value(current, "cwd"))
                        .with_string("summary", Some("workspace session rotated".into()))
                        .into_value(),
                )]);
            }
            None
        }
        (Some(previous), None) => Some(vec![workspace_event(
            matched,
            "workspace.session.ended",
            base_payload(matched, previous)
                .with_string("session_id", string_value(previous, "session_id"))
                .with_string("started_at", string_value(previous, "started_at"))
                .with_u64("pid", u64_value(previous, "pid"))
                .with_string("summary", Some("workspace session ended".into()))
                .into_value(),
        )]),
        (None, None) => None,
    }
}

fn diff_hud_state(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    let current = current?;
    let prev_turns = previous
        .and_then(|value| u64_value(value, "turn_count"))
        .unwrap_or(0);
    let curr_turns = u64_value(current, "turn_count")?;
    let last_turn_changed = previous.and_then(|value| string_value(value, "last_turn_at"))
        != string_value(current, "last_turn_at");

    if previous.is_some() && curr_turns <= prev_turns && !last_turn_changed {
        return None;
    }

    Some(vec![workspace_event(
        matched,
        "workspace.turn.complete",
        base_payload(matched, current)
            .with_u64("turn_count", Some(curr_turns))
            .with_u64("turn_delta", Some(curr_turns.saturating_sub(prev_turns)))
            .with_string("last_turn_at", string_value(current, "last_turn_at"))
            .with_string(
                "last_agent_output",
                string_value(current, "last_agent_output"),
            )
            .with_string("summary", Some(format!("turn {curr_turns} complete")))
            .into_value(),
    )])
}

fn diff_notify_hook_state(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    let current = current?;
    let prev_recent = previous.map(extract_recent_turn_keys).unwrap_or_default();
    let curr_recent = extract_recent_turn_keys(current);
    let added = curr_recent
        .difference(&prev_recent)
        .cloned()
        .collect::<Vec<_>>();
    let last_event_changed = previous.and_then(|value| string_value(value, "last_event_at"))
        != string_value(current, "last_event_at");
    if added.is_empty() && !last_event_changed {
        return None;
    }

    Some(vec![workspace_event(
        matched,
        "workspace.agent.turn",
        base_payload(matched, current)
            .with_u64("turn_count", Some(curr_recent.len() as u64))
            .with_string("last_event_at", string_value(current, "last_event_at"))
            .with_array(
                "recent_turn_keys",
                added.into_iter().map(Value::String).collect(),
            )
            .with_string("summary", Some("agent turn activity observed".into()))
            .into_value(),
    )])
}

fn diff_skill_state(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    let current = current?;
    let prev_active = previous
        .and_then(|value| bool_value(value, "active"))
        .unwrap_or(false);
    let curr_active = bool_value(current, "active").unwrap_or(false);
    let prev_skill = previous.and_then(|value| string_value(value, "skill"));
    let curr_skill = string_value(current, "skill");
    let prev_phase = previous.and_then(|value| string_value(value, "phase"));
    let curr_phase = string_value(current, "phase");

    let kind = if !prev_active && curr_active {
        "workspace.skill.activated"
    } else if prev_active && !curr_active {
        "workspace.skill.deactivated"
    } else if prev_phase != curr_phase {
        "workspace.skill.phase-changed"
    } else if prev_skill != curr_skill {
        "workspace.skill.activated"
    } else {
        return None;
    };

    Some(vec![workspace_event(
        matched,
        kind,
        base_payload(matched, current)
            .with_bool("active", Some(curr_active))
            .with_string("skill", curr_skill)
            .with_string("phase", curr_phase)
            .with_string("keyword", string_value(current, "keyword"))
            .with_string("source_kind", string_value(current, "source"))
            .with_string("previous_skill", prev_skill)
            .with_string("previous_phase", prev_phase)
            .with_string("summary", Some("workspace skill state changed".into()))
            .into_value(),
    )])
}

fn diff_metrics_state(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    let current = current?;
    if previous == Some(current) {
        return None;
    }

    let mut payload = base_payload(matched, current)
        .with_string("summary", Some("workspace metrics updated".into()))
        .into_object();
    payload.insert("metrics".into(), current.clone());
    Some(vec![workspace_event(
        matched,
        "workspace.metrics.updated",
        Value::Object(payload),
    )])
}

fn diff_tmux_hook_state(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    let current = current?;
    let prev_total = previous
        .and_then(|value| u64_value(value, "total_injections"))
        .unwrap_or(0);
    let curr_total = u64_value(current, "total_injections")?;
    if previous.is_some() && curr_total == prev_total {
        return None;
    }
    Some(vec![workspace_event(
        matched,
        "workspace.tmux.injection",
        base_payload(matched, current)
            .with_u64("total_injections", Some(curr_total))
            .with_u64(
                "injection_delta",
                Some(curr_total.saturating_sub(prev_total)),
            )
            .with_string("last_reason", string_value(current, "last_reason"))
            .with_string("last_event_at", string_value(current, "last_event_at"))
            .with_string("summary", Some("tmux injection state changed".into()))
            .into_value(),
    )])
}

fn diff_team_leader_nudge(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    let current = current?;
    if previous == Some(current) {
        return None;
    }
    Some(vec![workspace_event(
        matched,
        "workspace.team.nudged",
        base_payload(matched, current)
            .with_string(
                "last_nudged_by_team",
                string_value(current, "last_nudged_by_team"),
            )
            .with_string(
                "last_idle_nudged_by_team",
                string_value(current, "last_idle_nudged_by_team"),
            )
            .with_string("summary", Some("team leader nudge state changed".into()))
            .into_value(),
    )])
}

fn diff_notify_fallback_state(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    let current = current?;
    let prev_pid = previous.and_then(|value| u64_value(value, "pid"));
    let curr_pid = u64_value(current, "pid");
    if previous.is_some() && prev_pid == curr_pid {
        return None;
    }
    Some(vec![workspace_event(
        matched,
        "workspace.session.blocked",
        base_payload(matched, current)
            .with_u64("pid", curr_pid)
            .with_u64("parent_pid", u64_value(current, "parent_pid"))
            .with_string("notify_script", string_value(current, "notify_script"))
            .with_string("started_at", string_value(current, "started_at"))
            .with_string("cwd", string_value(current, "cwd"))
            .with_string("summary", Some("notify fallback monitor active".into()))
            .into_value(),
    )])
}

fn diff_mission_state(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    let current = current?;
    let prev_updated = previous.and_then(|value| string_value(value, "updatedAt"));
    let curr_updated = string_value(current, "updatedAt");
    if previous.is_some() && prev_updated == curr_updated {
        return None;
    }
    let missions = current
        .get("missions")
        .and_then(Value::as_array)
        .map(|missions| missions.len() as u64);
    Some(vec![workspace_event(
        matched,
        "workspace.mission.updated",
        base_payload(matched, current)
            .with_string("updated_at", curr_updated)
            .with_u64("mission_count", missions)
            .with_string("summary", Some("mission state updated".into()))
            .into_value(),
    )])
}

fn diff_team_state(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    let current = current?;
    if previous == Some(current) {
        return None;
    }
    Some(vec![workspace_event(
        matched,
        "workspace.team.updated",
        base_payload(matched, current)
            .with_string("summary", Some("team state updated".into()))
            .into_value(),
    )])
}

fn diff_subagent_tracking(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    let current = current?;
    if previous == Some(current) {
        return None;
    }
    Some(vec![workspace_event(
        matched,
        "workspace.team.updated",
        base_payload(matched, current)
            .with_string("summary", Some("subagent tracking updated".into()))
            .into_value(),
    )])
}

fn diff_checkpoint_state(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    let current = current?;
    let prev_created = previous.and_then(|value| string_value(value, "created_at"));
    let curr_created = string_value(current, "created_at");
    if previous.is_some() && prev_created == curr_created {
        return None;
    }
    Some(vec![workspace_event(
        matched,
        "workspace.session.checkpointed",
        base_payload(matched, current)
            .with_string("created_at", curr_created)
            .with_string("trigger", string_value(current, "trigger"))
            .with_string("todo_summary", string_value(current, "todo_summary"))
            .with_string("summary", Some("workspace checkpoint captured".into()))
            .into_value(),
    )])
}

fn diff_idle_notif_state(
    matched: &WorkspaceMatch<'_>,
    previous: Option<&Value>,
    current: Option<&Value>,
) -> Option<Vec<IncomingEvent>> {
    let current = current?;
    if previous == Some(current) {
        return None;
    }
    Some(vec![workspace_event(
        matched,
        "workspace.session.blocked",
        base_payload(matched, current)
            .with_string(
                "summary",
                Some("workspace session idle cooldown updated".into()),
            )
            .into_value(),
    )])
}

fn workspace_event(matched: &WorkspaceMatch<'_>, kind: &str, payload: Value) -> IncomingEvent {
    IncomingEvent::workspace(kind.to_string(), payload, matched.monitor.channel.clone())
        .with_mention(matched.monitor.mention.clone())
        .with_format(matched.monitor.format.clone())
}

fn base_payload(matched: &WorkspaceMatch<'_>, current: &Value) -> PayloadBuilder {
    PayloadBuilder::new()
        .with_string(
            "workspace_root",
            Some(matched.workspace_root.display().to_string()),
        )
        .with_string("workspace_name", Some(matched.workspace_name.clone()))
        .with_string("monitor_path", Some(matched.monitor.path.clone()))
        .with_string("state_family", Some(matched.state_family.clone()))
        .with_string("state_dir", Some(matched.watch_dir.display().to_string()))
        .with_string("state_file", Some(matched.state_file.clone()))
        .with_string("tool", string_value(current, "tool"))
}

fn read_json(path: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn debounce_for_path(config: &AppConfig, path: &Path) -> Duration {
    let topology = build_topology(config);
    let matched = classify_path(&topology, path);
    Duration::from_millis(
        matched
            .map(|matched| matched.monitor.debounce_ms)
            .unwrap_or(DEFAULT_WORKSPACE_DEBOUNCE_MS)
            .max(1),
    )
}

fn global_poll_interval(config: &AppConfig) -> Duration {
    let secs = config
        .monitors
        .workspace
        .iter()
        .filter_map(|monitor| monitor.poll_interval_secs)
        .min()
        .unwrap_or(config.monitors.poll_interval_secs)
        .max(1);
    Duration::from_secs(secs)
}

fn inotify_roots(config: &AppConfig) -> Vec<PathBuf> {
    let mut roots = build_topology(config)
        .into_iter()
        .map(|entry| entry.workspace_root)
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    roots.into_iter().filter(|root| root.exists()).collect()
}

fn infer_state_family(watch_dir: &str) -> String {
    let normalized = watch_dir.replace('\\', "/");
    if normalized.contains(".omx") {
        "omx".into()
    } else if normalized.contains(".omc") {
        "omc".into()
    } else {
        "workspace".into()
    }
}

fn workspace_name(path: &Path) -> String {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn relative_to_string(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn string_value(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn u64_value(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn bool_value(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

fn extract_recent_turn_keys(value: &Value) -> HashSet<String> {
    value
        .get("recent_turns")
        .and_then(Value::as_object)
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default()
}

fn monitor_allows_event(monitor: &WorkspaceMonitor, kind: &str) -> bool {
    if monitor.events.is_empty() {
        return true;
    }
    monitor
        .events
        .iter()
        .any(|pattern| glob_match(pattern, kind))
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

const DEFAULT_WORKSPACE_DEBOUNCE_MS: u64 = 2_000;
const DEFAULT_WORKSPACE_WATCH_DIRS: [&str; 2] = [".omx/state", ".omc/state"];

pub fn default_workspace_watch_dirs() -> Vec<String> {
    DEFAULT_WORKSPACE_WATCH_DIRS
        .iter()
        .map(|value| value.to_string())
        .collect()
}

pub fn default_workspace_debounce_ms() -> u64 {
    DEFAULT_WORKSPACE_DEBOUNCE_MS
}

#[derive(Debug, Clone)]
struct PayloadBuilder {
    inner: Map<String, Value>,
}

impl PayloadBuilder {
    fn new() -> Self {
        Self { inner: Map::new() }
    }

    fn with_string(mut self, key: &str, value: Option<String>) -> Self {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            self.inner.insert(key.to_string(), Value::String(value));
        }
        self
    }

    fn with_u64(mut self, key: &str, value: Option<u64>) -> Self {
        if let Some(value) = value {
            self.inner.insert(key.to_string(), Value::from(value));
        }
        self
    }

    fn with_bool(mut self, key: &str, value: Option<bool>) -> Self {
        if let Some(value) = value {
            self.inner.insert(key.to_string(), Value::Bool(value));
        }
        self
    }

    fn with_array(mut self, key: &str, value: Vec<Value>) -> Self {
        if !value.is_empty() {
            self.inner.insert(key.to_string(), Value::Array(value));
        }
        self
    }

    fn into_object(self) -> Map<String, Value> {
        self.inner
    }

    fn into_value(self) -> Value {
        Value::Object(self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn workspace_monitor(root: &Path) -> WorkspaceMonitor {
        WorkspaceMonitor {
            path: root.display().to_string(),
            watch_dirs: default_workspace_watch_dirs(),
            discover_worktrees: true,
            channel: Some("alerts".into()),
            mention: Some("<@1>".into()),
            format: Some(MessageFormat::Compact),
            events: Vec::new(),
            poll_interval_secs: Some(3),
            debounce_ms: 2_000,
        }
    }

    #[test]
    fn discovery_finds_configured_roots_and_worktrees() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".omx/state")).unwrap();
        std::fs::create_dir_all(dir.path().join(".omc/state")).unwrap();
        std::fs::create_dir_all(dir.path().join(".claude/worktrees/feat-84/.omx/state")).unwrap();

        let roots = discover_workspace_roots(&workspace_monitor(dir.path()));
        assert!(roots.contains(&dir.path().to_path_buf()));
        assert!(roots.contains(&dir.path().join(".claude/worktrees/feat-84")));
    }

    #[test]
    fn session_diff_emits_started_and_ended() {
        let dir = tempdir().unwrap();
        let monitor = workspace_monitor(dir.path());
        let topology = MonitorTopology {
            workspace_root: dir.path().to_path_buf(),
            workspace_name: "repo".into(),
            watch_dirs: vec![WatchDir {
                path: dir.path().join(".omx/state"),
                state_family: "omx".into(),
            }],
            monitor: &monitor,
        };
        let binding = [topology];
        let matched = classify_path(&binding, &dir.path().join(".omx/state/session.json")).unwrap();

        let started = diff_session(
            &matched,
            None,
            Some(&serde_json::json!({"session_id":"abc","pid":1,"cwd":"/tmp"})),
        )
        .unwrap();
        assert_eq!(started[0].canonical_kind(), "workspace.session.started");
        let ended = diff_session(
            &matched,
            Some(&serde_json::json!({"session_id":"abc","pid":1})),
            None,
        )
        .unwrap();
        assert_eq!(ended[0].canonical_kind(), "workspace.session.ended");
    }

    #[test]
    fn hud_diff_emits_turn_completion() {
        let dir = tempdir().unwrap();
        let monitor = workspace_monitor(dir.path());
        let topology = MonitorTopology {
            workspace_root: dir.path().to_path_buf(),
            workspace_name: "repo".into(),
            watch_dirs: vec![WatchDir {
                path: dir.path().join(".omx/state"),
                state_family: "omx".into(),
            }],
            monitor: &monitor,
        };
        let binding = [topology];
        let matched =
            classify_path(&binding, &dir.path().join(".omx/state/hud-state.json")).unwrap();

        let events = diff_hud_state(
            &matched,
            Some(&json!({"turn_count":1,"last_turn_at":"t1"})),
            Some(&json!({"turn_count":2,"last_turn_at":"t2","last_agent_output":"done"})),
        )
        .unwrap();
        assert_eq!(events[0].canonical_kind(), "workspace.turn.complete");
        assert_eq!(events[0].payload["turn_delta"], Value::from(1));
    }

    #[test]
    fn skill_diff_emits_activation() {
        let dir = tempdir().unwrap();
        let monitor = workspace_monitor(dir.path());
        let topology = MonitorTopology {
            workspace_root: dir.path().to_path_buf(),
            workspace_name: "repo".into(),
            watch_dirs: vec![WatchDir {
                path: dir.path().join(".omx/state"),
                state_family: "omx".into(),
            }],
            monitor: &monitor,
        };
        let binding = [topology];
        let matched = classify_path(
            &binding,
            &dir.path().join(".omx/state/skill-active-state.json"),
        )
        .unwrap();

        let events = diff_skill_state(
            &matched,
            Some(&json!({"active":false})),
            Some(&json!({"active":true,"skill":"ralph","phase":"exec"})),
        )
        .unwrap();
        assert_eq!(events[0].canonical_kind(), "workspace.skill.activated");
        assert_eq!(events[0].payload["skill"], Value::from("ralph"));
    }

    #[test]
    fn debounce_replaces_pending_deadline_for_same_file() {
        let mut state = WorkspaceState::new();
        let path = PathBuf::from("/tmp/state.json");
        let now = Instant::now();
        state.note_change(path.clone(), Duration::from_secs(2), now);
        let first_due = state.pending[&path].due_at;
        state.note_change(
            path.clone(),
            Duration::from_secs(2),
            now + Duration::from_secs(1),
        );
        assert!(state.pending[&path].due_at > first_due);
    }

    #[test]
    fn event_filter_honors_globs() {
        let mut monitor = workspace_monitor(Path::new("/tmp/repo"));
        monitor.events = vec!["workspace.skill.*".into()];
        assert!(monitor_allows_event(&monitor, "workspace.skill.activated"));
        assert!(!monitor_allows_event(&monitor, "workspace.turn.complete"));
    }
}
