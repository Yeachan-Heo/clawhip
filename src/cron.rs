use std::collections::BTreeSet;
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

    let client = DaemonClient::from_config(config);
    client.emit(build_job_event(job)).await
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

        let last_processed_minute = match state_path.as_deref() {
            Some(path) => load_scheduler_state(path)?.last_processed_minute,
            None => None,
        };

        Ok(Self {
            jobs,
            last_processed_minute,
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
                    emitter.emit(build_job_event(&job.config)).await?;
                    executed.push(job.config.id.clone());
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

fn build_job_event(job: &CronJob) -> IncomingEvent {
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
    }

    event
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
            kind: CronJobKind::CustomMessage {
                message: "wake up".into(),
            },
        })
        .expect_err("unsupported timezone");

        assert!(error.to_string().contains("supports UTC only"));
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
