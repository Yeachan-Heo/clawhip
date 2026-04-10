use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use time::{OffsetDateTime, Weekday};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};

use crate::Result;
use crate::client::DaemonClient;
use crate::config::{AppConfig, CronJob, CronJobKind};
use crate::events::IncomingEvent;
use crate::source::Source;

pub struct CronSource {
    config: Arc<AppConfig>,
    state_path: PathBuf,
}

impl CronSource {
    pub fn new(config: Arc<AppConfig>, state_path: PathBuf) -> Self {
        Self { config, state_path }
    }
}

#[async_trait::async_trait]
impl Source for CronSource {
    fn name(&self) -> &str {
        "cron"
    }

    async fn run(&self, tx: mpsc::Sender<IncomingEvent>) -> Result<()> {
        if self.config.cron.jobs.is_empty() {
            return Ok(());
        }

        let mut scheduler =
            CronScheduler::new_with_state_path(self.config.as_ref(), self.state_path.clone())?;
        let mut tick = interval(Duration::from_secs(
            self.config.cron.poll_interval_secs.max(1),
        ));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tick.tick().await;
            scheduler.emit_due(&tx, OffsetDateTime::now_utc()).await?;
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
            .map_err(|error| format!("cron scheduler channel closed: {error}").into())
    }
}

#[async_trait::async_trait]
impl EventEmitter for DaemonClient {
    async fn emit(&self, event: IncomingEvent) -> Result<()> {
        self.send_event(&event).await
    }
}

pub async fn run_configured_job(config: &AppConfig, id: &str) -> Result<()> {
    config.validate()?;

    let job = config
        .cron
        .jobs
        .iter()
        .find(|job| job.id == id)
        .ok_or_else(|| format!("cron job '{id}' was not found"))?;

    if !job.enabled {
        return Err(format!("cron job '{id}' is disabled").into());
    }

    // Manual runs bypass zero-delta suppression: if an operator explicitly
    // kicks a job they want the event fired regardless of backlog state. We
    // still attach the state snapshot to the payload if configured so
    // downstream consumers see the same context the scheduler would.
    let state = job.state_file.as_deref().and_then(evaluate_state_file);
    let client = DaemonClient::from_config(config);
    client.emit(build_job_event(job, state.as_ref())).await
}

pub fn validate_job(job: &CronJob) -> Result<()> {
    if job.id.trim().is_empty() {
        return Err("cron jobs must set id".into());
    }
    if job.schedule.trim().is_empty() {
        return Err(format!("cron job '{}' must set schedule", job.id).into());
    }
    match &job.kind {
        CronJobKind::CustomMessage { message } if message.trim().is_empty() => {
            return Err(format!("cron job '{}' must set message", job.id).into());
        }
        CronJobKind::CustomMessage { .. } => {}
    }
    validate_timezone(job)?;
    CronSchedule::parse(&job.schedule)
        .map(|_| ())
        .map_err(|error| format!("cron job '{}': {error}", job.id).into())
}

pub fn default_state_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("cron-state.json")
}

#[derive(Debug, Clone)]
struct CronScheduler {
    jobs: Vec<ScheduledCronJob>,
    last_processed_minute: Option<i64>,
    job_fingerprints: HashMap<String, String>,
    state_path: Option<PathBuf>,
}

impl CronScheduler {
    #[cfg(test)]
    fn new(config: &AppConfig) -> Result<Self> {
        Self::new_internal(config, None)
    }

    fn new_with_state_path(config: &AppConfig, state_path: PathBuf) -> Result<Self> {
        Self::new_internal(config, Some(state_path))
    }

    fn new_internal(config: &AppConfig, state_path: Option<PathBuf>) -> Result<Self> {
        let mut jobs = Vec::new();
        for job in config.cron.jobs.iter().filter(|job| job.enabled) {
            jobs.push(ScheduledCronJob {
                config: job.clone(),
                schedule: CronSchedule::parse(&job.schedule)?,
            });
        }

        let (last_processed_minute, job_fingerprints) = match state_path.as_deref() {
            Some(path) => {
                let state = load_scheduler_state(path)?;
                (state.last_processed_minute, state.job_fingerprints)
            }
            None => (None, HashMap::new()),
        };

        Ok(Self {
            jobs,
            last_processed_minute,
            job_fingerprints,
            state_path,
        })
    }

    async fn emit_due<E>(&mut self, emitter: &E, now: OffsetDateTime) -> Result<Vec<String>>
    where
        E: EventEmitter + ?Sized,
    {
        if self.jobs.is_empty() {
            self.last_processed_minute = Some(now.unix_timestamp().div_euclid(60));
            self.persist_state()?;
            return Ok(Vec::new());
        }

        let current_minute = now.unix_timestamp().div_euclid(60);
        let start_minute = self
            .last_processed_minute
            .map(|minute| minute + 1)
            .unwrap_or(current_minute);
        let mut executed = Vec::new();

        for minute in start_minute..=current_minute {
            let scheduled_for = OffsetDateTime::from_unix_timestamp(minute * 60)?;
            for job in &self.jobs {
                if job.matches(scheduled_for)? {
                    let state = job
                        .config
                        .state_file
                        .as_deref()
                        .and_then(evaluate_state_file);
                    if should_suppress(&state, self.job_fingerprints.get(&job.config.id)) {
                        continue;
                    }
                    emitter
                        .emit(build_job_event(&job.config, state.as_ref()))
                        .await?;
                    executed.push(job.config.id.clone());
                    if let Some(eval) = state {
                        self.job_fingerprints
                            .insert(job.config.id.clone(), eval.fingerprint);
                    }
                }
            }
        }

        self.last_processed_minute = Some(current_minute);
        self.persist_state()?;
        Ok(executed)
    }

    fn persist_state(&self) -> Result<()> {
        let Some(path) = self.state_path.as_deref() else {
            return Ok(());
        };

        save_scheduler_state(
            path,
            &CronSchedulerState {
                last_processed_minute: self.last_processed_minute,
                job_fingerprints: self.job_fingerprints.clone(),
            },
        )
    }
}

#[derive(Debug, Clone)]
struct ScheduledCronJob {
    config: CronJob,
    schedule: CronSchedule,
}

impl ScheduledCronJob {
    fn matches(&self, scheduled_for: OffsetDateTime) -> Result<bool> {
        let local_time = job_local_time(&self.config, scheduled_for)?;
        Ok(self.schedule.matches(local_time))
    }
}

fn build_job_event(job: &CronJob, state: Option<&StateEvaluation>) -> IncomingEvent {
    let mut event = match &job.kind {
        CronJobKind::CustomMessage { message } => {
            IncomingEvent::custom(job.channel.clone(), message.clone())
        }
    }
    .with_mention(job.mention.clone())
    .with_format(job.format.clone());

    if let Some(payload) = event.payload.as_object_mut() {
        payload.insert("cron_job_id".to_string(), json!(job.id));
        payload.insert("cron_schedule".to_string(), json!(job.schedule));
        payload.insert("cron_timezone".to_string(), json!(job.timezone));
        if let Some(state) = state {
            payload.insert(
                "repo_state_fingerprint".to_string(),
                json!(state.fingerprint),
            );
            payload.insert(
                "repo_state_zero_backlog".to_string(),
                json!(state.zero_backlog),
            );
        }
    }

    event
}

/// Snapshot derived from a cron job's `state_file`, used to decide whether to
/// suppress an emission and to attach context to events that do fire.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StateEvaluation {
    /// Canonical JSON serialization of the state file contents. Any byte-level
    /// change in the parsed value changes the fingerprint, which breaks
    /// suppression and causes the job to fire immediately on the next tick.
    fingerprint: String,
    /// True only when both `open_issues` and `open_prs` are present and zero.
    /// Missing counters default to non-zero so jobs keep firing until the
    /// state file explicitly advertises a zero backlog.
    zero_backlog: bool,
}

/// Read and evaluate a cron job's `state_file`. Returns `None` when the file
/// is missing, empty, or not valid JSON so callers fail open (i.e. emit
/// normally rather than silently swallowing a broken config).
fn evaluate_state_file(path: &Path) -> Option<StateEvaluation> {
    let content = fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let open_issues = value
        .get("open_issues")
        .and_then(|v| v.as_u64())
        .unwrap_or(1);
    let open_prs = value.get("open_prs").and_then(|v| v.as_u64()).unwrap_or(1);
    let zero_backlog = open_issues == 0 && open_prs == 0;
    let fingerprint = serde_json::to_string(&value).ok()?;
    Some(StateEvaluation {
        fingerprint,
        zero_backlog,
    })
}

/// A cron job should be suppressed only when its state file advertises a zero
/// backlog *and* its canonical fingerprint matches the one stored from the
/// previous emission. Any other case (non-zero backlog, missing state,
/// different fingerprint, first fire) fires the job.
fn should_suppress(state: &Option<StateEvaluation>, previous_fingerprint: Option<&String>) -> bool {
    let Some(eval) = state else {
        return false;
    };
    if !eval.zero_backlog {
        return false;
    }
    match previous_fingerprint {
        Some(prev) => prev.as_str() == eval.fingerprint.as_str(),
        None => false,
    }
}

fn validate_timezone(job: &CronJob) -> Result<()> {
    if timezone_is_supported(&job.timezone) {
        Ok(())
    } else {
        Err(format!(
            "cron job '{}' uses unsupported timezone '{}'; the current vertical slice supports UTC only",
            job.id, job.timezone
        )
        .into())
    }
}

fn timezone_is_supported(timezone: &str) -> bool {
    matches!(timezone.trim(), "UTC" | "Etc/UTC")
}

fn job_local_time(job: &CronJob, scheduled_for: OffsetDateTime) -> Result<OffsetDateTime> {
    if timezone_is_supported(&job.timezone) {
        Ok(scheduled_for)
    } else {
        Err(format!(
            "cron job '{}' uses unsupported timezone '{}'",
            job.id, job.timezone
        )
        .into())
    }
}

#[derive(Debug, Clone)]
struct CronSchedule {
    minute: CronField,
    hour: CronField,
    day_of_month: CronField,
    month: CronField,
    day_of_week: CronField,
}

impl CronSchedule {
    fn parse(spec: &str) -> Result<Self> {
        let fields = spec.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 5 {
            return Err(format!(
                "cron schedule '{spec}' must have exactly 5 fields (minute hour day-of-month month day-of-week)"
            )
            .into());
        }

        Ok(Self {
            minute: CronField::parse(fields[0], 0, 59, false)?,
            hour: CronField::parse(fields[1], 0, 23, false)?,
            day_of_month: CronField::parse(fields[2], 1, 31, false)?,
            month: CronField::parse(fields[3], 1, 12, false)?,
            day_of_week: CronField::parse(fields[4], 0, 7, true)?,
        })
    }

    fn matches(&self, timestamp: OffsetDateTime) -> bool {
        let minute = timestamp.minute();
        let hour = timestamp.hour();
        let day_of_month = timestamp.day();
        let month = timestamp.month() as u8;
        let day_of_week = weekday_to_cron(timestamp.weekday());

        let day_matches = if self.day_of_month.any || self.day_of_week.any {
            self.day_of_month.contains(day_of_month) && self.day_of_week.contains(day_of_week)
        } else {
            self.day_of_month.contains(day_of_month) || self.day_of_week.contains(day_of_week)
        };

        self.minute.contains(minute)
            && self.hour.contains(hour)
            && self.month.contains(month)
            && day_matches
    }
}

#[derive(Debug, Clone)]
struct CronField {
    any: bool,
    allowed: BTreeSet<u8>,
}

impl CronField {
    fn parse(spec: &str, min: u8, max: u8, wrap_sunday: bool) -> Result<Self> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("empty cron field".into());
        }
        if spec == "*" {
            return Ok(Self {
                any: true,
                allowed: BTreeSet::new(),
            });
        }

        let mut allowed = BTreeSet::new();
        for raw_part in spec.split(',') {
            let part = raw_part.trim();
            if part.is_empty() {
                return Err(format!("invalid cron field '{spec}'").into());
            }

            let (base, step) = match part.split_once('/') {
                Some((base, step)) => {
                    let step = step
                        .parse::<u8>()
                        .map_err(|_| format!("invalid cron step '{step}'"))?;
                    if step == 0 {
                        return Err(format!("cron step must be at least 1 in '{part}'").into());
                    }
                    (base, step)
                }
                None => (part, 1),
            };

            let (start, end) = if base == "*" {
                (min, max)
            } else if let Some((start, end)) = base.split_once('-') {
                (
                    parse_field_value(start, min, max)?,
                    parse_field_value(end, min, max)?,
                )
            } else {
                let value = parse_field_value(base, min, max)?;
                (value, value)
            };

            if start > end {
                return Err(format!("invalid descending cron range '{part}'").into());
            }

            let mut value = start;
            loop {
                allowed.insert(normalize_field_value(value, wrap_sunday));
                match value.checked_add(step) {
                    Some(next) if next <= end => value = next,
                    _ => break,
                }
            }
        }

        if allowed.is_empty() {
            return Err(format!("cron field '{spec}' resolved to no values").into());
        }

        Ok(Self {
            any: false,
            allowed,
        })
    }

    fn contains(&self, value: u8) -> bool {
        self.any || self.allowed.contains(&value)
    }
}

fn parse_field_value(raw: &str, min: u8, max: u8) -> Result<u8> {
    let value = raw
        .trim()
        .parse::<u8>()
        .map_err(|_| format!("invalid cron value '{raw}'"))?;
    if !(min..=max).contains(&value) {
        return Err(format!("cron value '{raw}' is outside {min}..={max}").into());
    }
    Ok(value)
}

fn normalize_field_value(value: u8, wrap_sunday: bool) -> u8 {
    if wrap_sunday && value == 7 { 0 } else { value }
}

fn weekday_to_cron(weekday: Weekday) -> u8 {
    match weekday {
        Weekday::Sunday => 0,
        Weekday::Monday => 1,
        Weekday::Tuesday => 2,
        Weekday::Wednesday => 3,
        Weekday::Thursday => 4,
        Weekday::Friday => 5,
        Weekday::Saturday => 6,
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct CronSchedulerState {
    last_processed_minute: Option<i64>,
    /// Per-job canonical JSON fingerprint of the `state_file` contents at the
    /// time of the last successful emission. Used to suppress repeated
    /// zero-delta follow-up nudges when the repo backlog is already zero.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    job_fingerprints: HashMap<String, String>,
}

fn load_scheduler_state(path: &Path) -> Result<CronSchedulerState> {
    if !path.exists() {
        return Ok(CronSchedulerState::default());
    }

    let raw = fs::read_to_string(path)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(CronSchedulerState::default());
    }

    match serde_json::from_str(trimmed) {
        Ok(state) => Ok(state),
        Err(error) => {
            eprintln!(
                "clawhip cron state '{}' is invalid; ignoring persisted state: {error}",
                path.display()
            );
            Ok(CronSchedulerState::default())
        }
    }
}

fn save_scheduler_state(path: &Path, state: &CronSchedulerState) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tempfile::tempdir;
    use time::{Date, Month, PrimitiveDateTime, Time};

    use crate::config::{CronConfig, DefaultsConfig};
    use crate::events::MessageFormat;

    use super::*;

    #[derive(Default)]
    struct RecordingEmitter {
        events: Arc<Mutex<Vec<IncomingEvent>>>,
    }

    #[async_trait::async_trait]
    impl EventEmitter for RecordingEmitter {
        async fn emit(&self, event: IncomingEvent) -> Result<()> {
            self.events.lock().expect("events lock").push(event);
            Ok(())
        }
    }

    #[tokio::test]
    async fn scheduler_emits_matching_custom_job_once_per_minute() {
        let config = sample_config("*/10 * * * *");
        let mut scheduler = CronScheduler::new(&config).expect("scheduler");
        let emitter = RecordingEmitter::default();

        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 20, 3))
            .await
            .expect("first tick");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 20, 55))
            .await
            .expect("same-minute tick");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 30, 1))
            .await
            .expect("later tick");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].channel.as_deref(), Some("ops"));
        assert_eq!(events[0].mention.as_deref(), Some("<@bot>"));
        assert_eq!(events[0].format, Some(MessageFormat::Alert));
        assert_eq!(events[0].payload["message"], json!("check open PRs"));
        assert_eq!(events[0].payload["cron_job_id"], json!("dev-followup"));
    }

    #[tokio::test]
    async fn scheduler_restart_does_not_refire_jobs_for_same_minute() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let config = sample_config("*/10 * * * *");
        let emitter = RecordingEmitter::default();

        let mut first = CronScheduler::new_with_state_path(&config, state_path.clone())
            .expect("first scheduler");
        first
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 20, 3))
            .await
            .expect("first emit");

        let mut restarted =
            CronScheduler::new_with_state_path(&config, state_path).expect("restarted scheduler");
        restarted
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 20, 45))
            .await
            .expect("same-minute restart");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn scheduler_restart_still_emits_on_next_matching_minute() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let config = sample_config("*/10 * * * *");
        let emitter = RecordingEmitter::default();

        let mut first = CronScheduler::new_with_state_path(&config, state_path.clone())
            .expect("first scheduler");
        first
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 20, 3))
            .await
            .expect("first emit");

        let mut restarted =
            CronScheduler::new_with_state_path(&config, state_path).expect("restarted scheduler");
        restarted
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 30, 1))
            .await
            .expect("next-minute restart");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn scheduler_startup_tolerates_empty_state_file() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        fs::write(&state_path, "").expect("write empty state");

        let scheduler =
            CronScheduler::new_with_state_path(&sample_config("*/10 * * * *"), state_path)
                .expect("scheduler");

        assert_eq!(scheduler.last_processed_minute, None);
    }

    #[test]
    fn scheduler_startup_tolerates_invalid_state_file() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        fs::write(&state_path, "{not-json").expect("write invalid state");

        let scheduler =
            CronScheduler::new_with_state_path(&sample_config("*/10 * * * *"), state_path)
                .expect("scheduler");

        assert_eq!(scheduler.last_processed_minute, None);
    }

    #[test]
    fn validate_job_rejects_non_utc_timezones_for_now() {
        let error = validate_job(&CronJob {
            id: "seoul".into(),
            schedule: "0 9 * * *".into(),
            timezone: "Asia/Seoul".into(),
            enabled: true,
            channel: Some("ops".into()),
            mention: None,
            format: None,
            state_file: None,
            kind: CronJobKind::CustomMessage {
                message: "wake up".into(),
            },
        })
        .expect_err("unsupported timezone");

        assert!(error.to_string().contains("supports UTC only"));
    }

    #[tokio::test]
    async fn suppresses_repeat_nudge_when_backlog_is_zero_and_state_unchanged() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        fs::write(
            &repo_state,
            r#"{"open_issues":0,"open_prs":0,"sha":"deadbeef"}"#,
        )
        .expect("write repo state");

        let config = sample_config_with_state("*/10 * * * *", Some(repo_state));
        let mut scheduler =
            CronScheduler::new_with_state_path(&config, state_path).expect("scheduler");
        let emitter = RecordingEmitter::default();

        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 20, 3))
            .await
            .expect("first tick");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 30, 5))
            .await
            .expect("second tick — should suppress");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 40, 5))
            .await
            .expect("third tick — should also suppress");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(
            events.len(),
            1,
            "expected only the first nudge to fire while state stayed at zero backlog"
        );
        assert_eq!(
            events[0].payload["repo_state_zero_backlog"],
            json!(true),
            "emitted event should carry the zero-backlog signal"
        );
    }

    #[tokio::test]
    async fn emits_again_when_state_file_changes_even_with_zero_backlog() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        fs::write(
            &repo_state,
            r#"{"open_issues":0,"open_prs":0,"sha":"aaaa"}"#,
        )
        .expect("write repo state v1");

        let config = sample_config_with_state("*/10 * * * *", Some(repo_state.clone()));
        let mut scheduler =
            CronScheduler::new_with_state_path(&config, state_path).expect("scheduler");
        let emitter = RecordingEmitter::default();

        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 20, 3))
            .await
            .expect("first tick");

        // A real delta lands: sha changes even though counters stay zero. The
        // scheduler must re-fire because "zero-delta" means the state itself
        // has not moved; any byte-level change breaks that assumption.
        fs::write(
            &repo_state,
            r#"{"open_issues":0,"open_prs":0,"sha":"bbbb"}"#,
        )
        .expect("write repo state v2");

        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 30, 5))
            .await
            .expect("second tick — should re-emit");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn never_suppresses_when_backlog_is_nonzero() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        // Backlog is 3 open PRs. Even if nothing else changes, we want the
        // nudge to keep firing so operators don't lose track of active work.
        fs::write(&repo_state, r#"{"open_issues":0,"open_prs":3}"#).expect("write repo state");

        let config = sample_config_with_state("*/10 * * * *", Some(repo_state));
        let mut scheduler =
            CronScheduler::new_with_state_path(&config, state_path).expect("scheduler");
        let emitter = RecordingEmitter::default();

        for hour_minute in [(8u8, 20u8), (8, 30), (8, 40)] {
            scheduler
                .emit_due(
                    &emitter,
                    dt(2026, Month::April, 2, hour_minute.0, hour_minute.1, 1),
                )
                .await
                .expect("tick");
        }

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].payload["repo_state_zero_backlog"], json!(false));
    }

    #[tokio::test]
    async fn re_emits_immediately_when_backlog_transitions_back_from_zero() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        fs::write(&repo_state, r#"{"open_issues":0,"open_prs":0}"#)
            .expect("write repo state v1 (zero)");

        let config = sample_config_with_state("*/10 * * * *", Some(repo_state.clone()));
        let mut scheduler =
            CronScheduler::new_with_state_path(&config, state_path).expect("scheduler");
        let emitter = RecordingEmitter::default();

        // First tick establishes the zero-backlog baseline and fires once.
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 20, 0))
            .await
            .expect("tick 1");
        // Second tick is suppressed: same zero backlog, same fingerprint.
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 30, 0))
            .await
            .expect("tick 2");

        // Backlog grows: a new PR lands.
        fs::write(&repo_state, r#"{"open_issues":0,"open_prs":1}"#)
            .expect("write repo state v2 (nonzero)");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 40, 0))
            .await
            .expect("tick 3 — nonzero delta");

        // Work ships; backlog drops back to zero. The scheduler should fire
        // one more time to announce the transition, then fall silent again.
        fs::write(&repo_state, r#"{"open_issues":0,"open_prs":0}"#)
            .expect("write repo state v3 (back to zero)");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 50, 0))
            .await
            .expect("tick 4 — zero transition");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 9, 0, 0))
            .await
            .expect("tick 5 — suppressed again");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(
            events.len(),
            3,
            "expected emissions at ticks 1, 3, 4 (ticks 2 and 5 suppressed)"
        );
    }

    #[tokio::test]
    async fn missing_state_file_fails_open_and_fires_normally() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("does-not-exist.json");

        let config = sample_config_with_state("*/10 * * * *", Some(repo_state));
        let mut scheduler =
            CronScheduler::new_with_state_path(&config, state_path).expect("scheduler");
        let emitter = RecordingEmitter::default();

        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 20, 0))
            .await
            .expect("tick 1");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 30, 0))
            .await
            .expect("tick 2");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(
            events.len(),
            2,
            "missing state file must not silently suppress a configured job"
        );
    }

    #[tokio::test]
    async fn job_without_state_file_preserves_legacy_behavior() {
        let config = sample_config("*/10 * * * *");
        let mut scheduler = CronScheduler::new(&config).expect("scheduler");
        let emitter = RecordingEmitter::default();

        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 20, 0))
            .await
            .expect("tick 1");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 30, 0))
            .await
            .expect("tick 2");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 2);
        assert!(
            events[0].payload.get("repo_state_fingerprint").is_none(),
            "legacy jobs without state_file should not leak repo_state_* fields"
        );
    }

    #[tokio::test]
    async fn fingerprint_persists_across_scheduler_restarts() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        fs::write(&repo_state, r#"{"open_issues":0,"open_prs":0}"#).expect("write repo state");

        let config = sample_config_with_state("*/10 * * * *", Some(repo_state));
        let emitter = RecordingEmitter::default();

        let mut first = CronScheduler::new_with_state_path(&config, state_path.clone())
            .expect("first scheduler");
        first
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 20, 0))
            .await
            .expect("first emit");

        // Restart reads the persisted fingerprint and should keep suppressing
        // — a daemon restart shouldn't cause a spurious repeat nudge.
        let mut restarted =
            CronScheduler::new_with_state_path(&config, state_path).expect("restarted scheduler");
        restarted
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 30, 0))
            .await
            .expect("restarted emit");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn evaluate_state_file_treats_missing_counters_as_nonzero() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("repo.json");
        fs::write(&path, r#"{"sha":"abc"}"#).expect("write state file");

        let eval = evaluate_state_file(&path).expect("evaluation");
        assert!(
            !eval.zero_backlog,
            "a state file without counters must not trigger suppression"
        );
    }

    #[test]
    fn evaluate_state_file_normalizes_whitespace_in_fingerprint() {
        let dir = tempdir().expect("tempdir");
        let compact = dir.path().join("compact.json");
        let pretty = dir.path().join("pretty.json");
        fs::write(&compact, r#"{"open_issues":0,"open_prs":0}"#).expect("write compact");
        fs::write(&pretty, "{\n  \"open_issues\": 0,\n  \"open_prs\": 0\n}\n")
            .expect("write pretty");

        let compact_eval = evaluate_state_file(&compact).expect("compact eval");
        let pretty_eval = evaluate_state_file(&pretty).expect("pretty eval");
        assert_eq!(
            compact_eval.fingerprint, pretty_eval.fingerprint,
            "whitespace-only formatting changes must not count as a delta"
        );
    }

    #[test]
    fn schedule_parser_supports_lists_ranges_and_steps() {
        let schedule = CronSchedule::parse("0,15,30-45/15 9-17/4 * * 1-5").expect("schedule");

        assert!(schedule.matches(dt(2026, Month::April, 6, 9, 0, 0)));
        assert!(schedule.matches(dt(2026, Month::April, 6, 13, 15, 0)));
        assert!(schedule.matches(dt(2026, Month::April, 10, 17, 45, 0)));
        assert!(!schedule.matches(dt(2026, Month::April, 10, 17, 10, 0)));
        assert!(!schedule.matches(dt(2026, Month::April, 11, 9, 0, 0)));
    }

    fn sample_config(schedule: &str) -> AppConfig {
        sample_config_with_state(schedule, None)
    }

    fn sample_config_with_state(schedule: &str, state_file: Option<PathBuf>) -> AppConfig {
        AppConfig {
            defaults: DefaultsConfig {
                channel: Some("ops".into()),
                format: MessageFormat::Compact,
            },
            cron: CronConfig {
                poll_interval_secs: 30,
                jobs: vec![CronJob {
                    id: "dev-followup".into(),
                    schedule: schedule.into(),
                    timezone: "UTC".into(),
                    enabled: true,
                    channel: Some("ops".into()),
                    mention: Some("<@bot>".into()),
                    format: Some(MessageFormat::Alert),
                    state_file,
                    kind: CronJobKind::CustomMessage {
                        message: "check open PRs".into(),
                    },
                }],
            },
            ..AppConfig::default()
        }
    }

    fn dt(year: i32, month: Month, day: u8, hour: u8, minute: u8, second: u8) -> OffsetDateTime {
        let date = Date::from_calendar_date(year, month, day).expect("date");
        let time = Time::from_hms(hour, minute, second).expect("time");
        PrimitiveDateTime::new(date, time).assume_utc()
    }
}
