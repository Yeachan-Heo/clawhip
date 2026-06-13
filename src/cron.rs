use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
    zero_backlog_suppressions: HashMap<String, ZeroBacklogSuppression>,
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

        let (last_processed_minute, job_fingerprints, zero_backlog_suppressions) =
            match state_path.as_deref() {
                Some(path) => {
                    let state = load_scheduler_state(path)?;
                    (
                        state.last_processed_minute,
                        state.job_fingerprints,
                        state.zero_backlog_suppressions,
                    )
                }
                None => (None, HashMap::new(), HashMap::new()),
            };

        Ok(Self {
            jobs,
            last_processed_minute,
            job_fingerprints,
            zero_backlog_suppressions,
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
                    if let Some(eval) = state.as_ref()
                        && should_suppress(
                            &job.config,
                            eval,
                            self.job_fingerprints.get(&job.config.id),
                            self.zero_backlog_suppressions.get(&job.config.id),
                            scheduled_for.unix_timestamp(),
                        )
                    {
                        if let Some(suppression) =
                            self.zero_backlog_suppressions.get(&job.config.id)
                        {
                            eprintln!(
                                "{}",
                                suppression_notice(
                                    &job.config.id,
                                    suppression,
                                    scheduled_for.unix_timestamp(),
                                )
                            );
                        }
                        continue;
                    }
                    emitter
                        .emit(build_job_event(&job.config, state.as_ref()))
                        .await?;
                    executed.push(job.config.id.clone());
                    if let Some(eval) = state {
                        self.job_fingerprints
                            .insert(job.config.id.clone(), eval.fingerprint.clone());
                        if eval.zero_backlog && job.config.zero_backlog_suppression_ttl_secs > 0 {
                            self.zero_backlog_suppressions.insert(
                                job.config.id.clone(),
                                ZeroBacklogSuppression {
                                    key: eval.suppression_key,
                                    expires_at: scheduled_for.unix_timestamp()
                                        + job.config.zero_backlog_suppression_ttl_secs as i64,
                                },
                            );
                        } else {
                            self.zero_backlog_suppressions.remove(&job.config.id);
                        }
                    } else {
                        self.zero_backlog_suppressions.remove(&job.config.id);
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
                zero_backlog_suppressions: self.zero_backlog_suppressions.clone(),
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
            payload.insert(
                "repo_state_observation_source".to_string(),
                json!(state.observation_source),
            );
            payload.insert(
                "repo_state_observation_confidence".to_string(),
                json!(state.observation_confidence),
            );
            payload.insert(
                "repo_state_github_api_fallback".to_string(),
                json!(state.github_api_fallback),
            );
        }
    }

    event
}

/// Snapshot derived from a cron job's `state_file`, used to decide whether to
/// suppress an emission and to attach public-safe context to events that do fire.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StateEvaluation {
    /// Deterministic bounded hash of a canonical public-safe subset of the state.
    fingerprint: String,
    /// Deterministic bounded key used for TTL suppression. It never includes raw
    /// logs, config, tokens, private channel payloads, or full receipt bodies.
    suppression_key: String,
    /// True only for validated GAJAE zero-backlog/follow-up receipts with zero
    /// PRs/issues, green dev CI, no action-needed sessions, and no holds.
    zero_backlog: bool,
    /// Public-safe label for where this observation came from (e.g.
    /// `github-api`, `github-api-fallback`, or an operator-provided source).
    observation_source: String,
    /// Confidence marker: `validated` for full zero-backlog authority,
    /// `fallback-unverified` for a degraded API fallback lacking evidence,
    /// otherwise `reported`.
    observation_confidence: String,
    /// True when the receipt signals a GitHub API/rate-limit failure, detected
    /// separately from an empty backlog.
    github_api_fallback: bool,
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
    let public_state = public_suppression_state(&value);
    let canonical = serde_json::to_string(&public_state).ok()?;
    let fingerprint = stable_hash_hex(canonical.as_bytes());
    let suppression_key = format!("gajae-zero-backlog:{fingerprint}");
    let github_api_fallback = github_api_failed(&value);
    let zero_backlog = is_validated_zero_backlog_receipt(&value);
    Some(StateEvaluation {
        fingerprint,
        suppression_key,
        observation_source: observation_source(&value, github_api_fallback),
        observation_confidence: observation_confidence(&value, zero_backlog, github_api_fallback),
        zero_backlog,
        github_api_fallback,
    })
}

fn public_suppression_state(value: &Value) -> BTreeMap<String, Value> {
    let mut state = BTreeMap::new();
    insert_public_value(&mut state, value, "family");
    insert_public_value(&mut state, value, "status");
    insert_public_value(&mut state, value, "receipt_id");
    insert_public_value(&mut state, value, "subject");
    insert_public_value(&mut state, value, "open_issues");
    insert_public_value(&mut state, value, "open_prs");
    insert_public_value(&mut state, value, "latest_dev_ci");
    insert_public_value(&mut state, value, "dev_ci");
    // Branch head / check-summary digests so a real `dev` head change (a new
    // commit landing with otherwise-identical counts and CI label) alters the
    // fingerprint and re-emits, instead of being coalesced as an unchanged tick.
    // Values pass through `bounded_public_token`, so only public-safe identifiers
    // (e.g. a commit SHA) are retained.
    insert_public_value(&mut state, value, "dev_head");
    insert_public_value(&mut state, value, "branch_head");
    insert_public_value(&mut state, value, "head_sha");
    insert_public_value(&mut state, value, "dev_check_summary");
    insert_public_value(&mut state, value, "check_summary");
    insert_public_value(&mut state, value, "active_sessions_needing_action");
    insert_public_value(&mut state, value, "sessions_needing_action");
    insert_public_value(&mut state, value, "session_stale_events");
    insert_public_value(&mut state, value, "approval_hold");
    insert_public_value(&mut state, value, "release_hold");
    insert_public_value(&mut state, value, "action_needed_sessions");
    insert_public_value(&mut state, value, "action_required");
    insert_public_value(&mut state, value, "observation_source");
    insert_public_value(&mut state, value, "stop_decision");
    insert_public_value(&mut state, value, "new_event_id");
    insert_public_value(&mut state, value, "github_api_status");
    insert_public_value(&mut state, value, "github_api");
    insert_public_value(&mut state, value, "observation_source");
    insert_public_value(&mut state, value, "fallback_evidence");
    state
}

fn insert_public_value(state: &mut BTreeMap<String, Value>, value: &Value, key: &str) {
    if let Some(value) = value.get(key) {
        state.insert(key.to_string(), public_value(value));
    }
}

fn public_value(value: &Value) -> Value {
    match value {
        Value::String(value) => json!(bounded_public_token(value)),
        Value::Number(_) | Value::Bool(_) | Value::Null => value.clone(),
        Value::Array(values) => json!(values.len()),
        Value::Object(_) => json!("object"),
    }
}

fn bounded_public_token(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':' | '/'))
        .take(96)
        .collect()
}

fn is_validated_zero_backlog_receipt(value: &Value) -> bool {
    is_full_zero_backlog_receipt(value) || is_lightweight_zero_backlog_checkpoint(value)
}

/// Full zero-backlog receipt: requires green dev CI and explicit session/hold
/// evidence in addition to a clear backlog. Used for richer transitions.
fn is_full_zero_backlog_receipt(value: &Value) -> bool {
    validated_zero_backlog_family(value)
        && receipt_status_validated(value)
        && numeric_field(value, "open_issues") == Some(0)
        && numeric_field(value, "open_prs") == Some(0)
        && ci_is_green(value)
        && numeric_field(value, "active_sessions_needing_action").unwrap_or(0) == 0
        && numeric_field(value, "sessions_needing_action").unwrap_or(0) == 0
        && numeric_field(value, "session_stale_events").unwrap_or(0) == 0
        && !bool_field(value, "approval_hold")
        && !bool_field(value, "release_hold")
        && fallback_has_authority(value)
}

/// Lightweight zero-backlog follow-up checkpoint: a compact receipt for routine
/// ticks. It carries observed counts and a deterministic stop decision but
/// intentionally omits the full CI/session evidence. Suppression only applies
/// when the checkpoint itself decided to suppress the follow-up.
fn is_lightweight_zero_backlog_checkpoint(value: &Value) -> bool {
    value.get("family").and_then(Value::as_str)
        == Some(crate::gajae::ZERO_BACKLOG_FOLLOWUP_CHECKPOINT_FAMILY)
        && receipt_status_validated(value)
        && numeric_field(value, "open_issues") == Some(0)
        && numeric_field(value, "open_prs") == Some(0)
        && numeric_field(value, "action_needed_sessions").unwrap_or(0) == 0
        && !bool_field(value, "approval_hold")
        && !bool_field(value, "release_hold")
        && !bool_field(value, "action_required")
        && value.get("stop_decision").and_then(Value::as_str) == Some("suppress-followup")
        && bool_field(value, "stop_decision_deterministic")
}

fn validated_zero_backlog_family(value: &Value) -> bool {
    matches!(
        value.get("family").and_then(Value::as_str),
        Some(
            "runtime-followup-receipt"
                | "zero-backlog-checkpoint"
                | "backlog-suppression-snapshot"
                | "zero-backlog-post-merge-event-coalescing"
        )
    )
}

fn receipt_status_validated(value: &Value) -> bool {
    matches!(
        value.get("status").and_then(Value::as_str),
        Some("validated" | "valid")
    )
}

fn ci_is_green(value: &Value) -> bool {
    let status = value
        .get("latest_dev_ci")
        .or_else(|| value.get("dev_ci"))
        .and_then(Value::as_str);
    matches!(status, Some("green" | "passed" | "success"))
}

fn numeric_field(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn bool_field(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Detect a GitHub API / rate-limit failure from a receipt, separately from an
/// empty backlog. A degraded observation must never be silently treated like a
/// healthy zero-backlog confirmation.
fn github_api_failed(value: &Value) -> bool {
    let status = value
        .get("github_api_status")
        .or_else(|| value.get("github_api"))
        .and_then(Value::as_str)
        .map(|status| status.trim().to_ascii_lowercase());
    matches!(
        status.as_deref(),
        Some(
            "rate_limited"
                | "rate-limited"
                | "ratelimited"
                | "throttled"
                | "error"
                | "errored"
                | "failed"
                | "failure"
                | "unavailable"
                | "degraded"
        )
    )
}

/// Public-safe label for where a follow-up observation came from. Honors an
/// explicit `observation_source` and otherwise reflects whether the GitHub API
/// path was healthy or fell back.
fn observation_source(value: &Value, github_api_fallback: bool) -> String {
    if let Some(source) = value.get("observation_source").and_then(Value::as_str) {
        let token = bounded_public_token(source);
        if !token.is_empty() {
            return token;
        }
    }
    if github_api_fallback {
        "github-api-fallback".to_string()
    } else {
        "github-api".to_string()
    }
}

/// Confidence marker for a follow-up observation. `validated` only when every
/// zero-backlog authority check passes; a degraded API fallback lacking the
/// required corroborating evidence is flagged `fallback-unverified` so it is
/// never mistaken for one.
fn observation_confidence(value: &Value, zero_backlog: bool, github_api_fallback: bool) -> String {
    if zero_backlog {
        "validated".to_string()
    } else if github_api_fallback && !fallback_has_authority(value) {
        "fallback-unverified".to_string()
    } else {
        "reported".to_string()
    }
}

/// A degraded GitHub API fallback may only carry merge/close authority when the
/// receipt explicitly advertises that the required corroborating evidence was
/// gathered from a safe source (e.g. confirmed via public repository pages).
fn fallback_has_authority(value: &Value) -> bool {
    !github_api_failed(value) || bool_field(value, "fallback_evidence")
}

fn stable_hash_hex(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn should_suppress(
    job: &CronJob,
    eval: &StateEvaluation,
    previous_fingerprint: Option<&String>,
    suppression: Option<&ZeroBacklogSuppression>,
    now: i64,
) -> bool {
    if job.zero_backlog_suppression_ttl_secs == 0 || !eval.zero_backlog {
        return false;
    }
    previous_fingerprint.is_some_and(|prev| prev == &eval.fingerprint)
        && suppression.is_some_and(|suppression| {
            suppression.key == eval.suppression_key && suppression.expires_at > now
        })
}

/// Public-safe one-line notice recorded when a zero-backlog follow-up nudge is
/// intentionally suppressed. It contains only the operator-defined job id, the
/// deterministic public-safe suppression key, and the remaining lease seconds —
/// never raw receipts, logs, tokens, or private payloads — so operators can see
/// the nudge was withheld on purpose, not silently dropped.
fn suppression_notice(job_id: &str, suppression: &ZeroBacklogSuppression, now: i64) -> String {
    let remaining = (suppression.expires_at - now).max(0);
    format!(
        "clawhip cron '{job_id}' zero-backlog follow-up suppressed (key={}, expires_in={remaining}s); nudge intentionally withheld, not dropped",
        suppression.key
    )
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
struct ZeroBacklogSuppression {
    key: String,
    expires_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct CronSchedulerState {
    last_processed_minute: Option<i64>,
    /// Per-job public-safe fingerprint from the last successful emission.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    job_fingerprints: HashMap<String, String>,
    /// Per-job zero-backlog suppression lease. Matching follow-up receipts are
    /// suppressed only until `expires_at`; any public-safe key delta breaks it.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    zero_backlog_suppressions: HashMap<String, ZeroBacklogSuppression>,
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
            zero_backlog_suppression_ttl_secs: 60 * 60,
            kind: CronJobKind::CustomMessage {
                message: "wake up".into(),
            },
        })
        .expect_err("unsupported timezone");

        assert!(error.to_string().contains("supports UTC only"));
    }

    #[test]
    fn suppression_notice_is_public_safe_and_observable() {
        let suppression = ZeroBacklogSuppression {
            key: "gajae-zero-backlog:0123456789abcdef".into(),
            expires_at: 1_000,
        };
        let notice = suppression_notice("dev-followup", &suppression, 400);
        assert!(notice.contains("dev-followup"));
        assert!(notice.contains("gajae-zero-backlog:0123456789abcdef"));
        assert!(notice.contains("expires_in=600s"));
        assert!(notice.contains("not dropped"));

        // Past-expiry leases clamp to zero rather than reporting negatives.
        let expired = suppression_notice("dev-followup", &suppression, 5_000);
        assert!(expired.contains("expires_in=0s"));
    }

    #[tokio::test]
    async fn validated_zero_backlog_receipt_suppresses_repeated_followups_until_ttl() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        write_zero_backlog_receipt(&repo_state, None, false).expect("write receipt");

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
            .expect("second tick suppressed");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 9, 30, 5))
            .await
            .expect("ttl expired tick");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 2, "middle follow-up should be silent");
        assert_eq!(events[0].payload["message"], json!("check open PRs"));
        assert_eq!(events[0].payload["repo_state_zero_backlog"], json!(true));
        let fingerprint = events[0].payload["repo_state_fingerprint"]
            .as_str()
            .expect("fingerprint");
        assert_eq!(fingerprint.len(), 16);
        assert!(!fingerprint.contains("secret-token"));
    }

    #[tokio::test]
    async fn new_public_event_breaks_zero_backlog_suppression_immediately() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        write_zero_backlog_receipt(&repo_state, None, false).expect("write receipt v1");

        let config = sample_config_with_state("*/10 * * * *", Some(repo_state.clone()));
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
            .expect("suppressed tick");

        write_zero_backlog_receipt(&repo_state, Some("github-pr-262"), false)
            .expect("write receipt with new event");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 40, 5))
            .await
            .expect("new event breaks suppression");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 2);
        assert_ne!(
            events[0].payload["repo_state_fingerprint"],
            events[1].payload["repo_state_fingerprint"],
            "new PR/issue/CI/stale marker must alter public-safe key and re-emit"
        );
    }

    #[tokio::test]
    async fn release_hold_receipt_is_not_zero_backlog_suppressed() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        write_zero_backlog_receipt(&repo_state, None, true).expect("write release hold receipt");

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
            .expect("release hold remains routed");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload["repo_state_zero_backlog"], json!(false));
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
    async fn stale_session_marker_breaks_zero_backlog_suppression_immediately() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        write_zero_backlog_receipt(&repo_state, None, false).expect("write receipt v1");

        let config = sample_config_with_state("*/10 * * * *", Some(repo_state.clone()));
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
            .expect("tick 2 suppressed");

        fs::write(
            &repo_state,
            r#"{"family":"runtime-followup-receipt","status":"validated","open_issues":0,"open_prs":0,"latest_dev_ci":"green","session_stale_events":1,"secret":"raw log"}"#,
        )
        .expect("write stale session receipt");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 40, 0))
            .await
            .expect("stale session breaks suppression");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].payload["repo_state_zero_backlog"], json!(false));
    }

    #[tokio::test]
    async fn ci_failure_breaks_zero_backlog_suppression_immediately() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        write_zero_backlog_receipt(&repo_state, None, false).expect("write receipt v1");

        let config = sample_config_with_state("*/10 * * * *", Some(repo_state.clone()));
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
            .expect("tick 2 suppressed");

        fs::write(
            &repo_state,
            r#"{"family":"runtime-followup-receipt","status":"validated","open_issues":0,"open_prs":0,"latest_dev_ci":"failed","active_sessions_needing_action":0}"#,
        )
        .expect("write failed-ci receipt");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 40, 0))
            .await
            .expect("ci failure breaks suppression");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].payload["repo_state_zero_backlog"], json!(false));
    }

    #[tokio::test]
    async fn branch_head_change_breaks_zero_backlog_suppression_immediately() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        fs::write(
            &repo_state,
            r#"{"family":"runtime-followup-receipt","status":"validated","open_issues":0,"open_prs":0,"latest_dev_ci":"green","active_sessions_needing_action":0,"dev_head":"aaaaaaaaaaaa"}"#,
        )
        .expect("write head-a receipt");

        let config = sample_config_with_state("*/10 * * * *", Some(repo_state.clone()));
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
            .expect("tick 2 suppressed");

        // Identical zero-backlog counts and CI label, but a new dev commit landed:
        // the head digest changed, so this is a real transition and must re-emit.
        fs::write(
            &repo_state,
            r#"{"family":"runtime-followup-receipt","status":"validated","open_issues":0,"open_prs":0,"latest_dev_ci":"green","active_sessions_needing_action":0,"dev_head":"bbbbbbbbbbbb"}"#,
        )
        .expect("write head-b receipt");
        scheduler
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 40, 0))
            .await
            .expect("branch head change re-emits");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(
            events.len(),
            2,
            "first tick + head-change tick emit; the identical middle tick coalesces"
        );
        assert_eq!(
            events[1].payload["repo_state_zero_backlog"],
            json!(true),
            "a new dev head is still a valid zero-backlog state, just a fresh transition"
        );
        assert_ne!(
            events[0].payload["repo_state_fingerprint"],
            events[1].payload["repo_state_fingerprint"],
            "a branch head change must alter the public-safe key and re-emit"
        );
    }

    #[test]
    fn branch_head_digest_is_part_of_fingerprint_and_public_safe() {
        let dir = tempdir().expect("tempdir");
        let head_a = dir.path().join("head-a.json");
        let head_b = dir.path().join("head-b.json");
        fs::write(
            &head_a,
            r#"{"family":"runtime-followup-receipt","status":"validated","open_issues":0,"open_prs":0,"latest_dev_ci":"green","active_sessions_needing_action":0,"dev_head":"abc123def456","raw_log":"secret-token must not leak"}"#,
        )
        .expect("write head-a");
        fs::write(
            &head_b,
            r#"{"family":"runtime-followup-receipt","status":"validated","open_issues":0,"open_prs":0,"latest_dev_ci":"green","active_sessions_needing_action":0,"dev_head":"999fedcba000","raw_log":"secret-token must not leak"}"#,
        )
        .expect("write head-b");

        let eval_a = evaluate_state_file(&head_a).expect("eval a");
        let eval_b = evaluate_state_file(&head_b).expect("eval b");
        assert!(
            eval_a.zero_backlog && eval_b.zero_backlog,
            "both receipts remain valid zero-backlog states"
        );
        assert_ne!(
            eval_a.fingerprint, eval_b.fingerprint,
            "a branch head change must alter the fingerprint"
        );
        assert_ne!(eval_a.suppression_key, eval_b.suppression_key);
        assert!(
            !eval_a.fingerprint.contains("secret-token"),
            "the fingerprint must never leak private receipt content"
        );
        assert_eq!(eval_a.fingerprint.len(), 16);
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
    async fn suppression_lease_persists_across_scheduler_restarts() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        write_zero_backlog_receipt(&repo_state, None, false).expect("write receipt");

        let config = sample_config_with_state("*/10 * * * *", Some(repo_state));
        let emitter = RecordingEmitter::default();

        let mut first = CronScheduler::new_with_state_path(&config, state_path.clone())
            .expect("first scheduler");
        first
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 20, 0))
            .await
            .expect("first emit");

        let mut restarted =
            CronScheduler::new_with_state_path(&config, state_path).expect("restarted scheduler");
        restarted
            .emit_due(&emitter, dt(2026, Month::April, 2, 8, 30, 0))
            .await
            .expect("restarted suppressed");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["message"], json!("check open PRs"));
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
        write_zero_backlog_receipt(&compact, None, false).expect("write compact");
        fs::write(
            &pretty,
            "{\n  \"family\": \"runtime-followup-receipt\",\n  \"status\": \"validated\",\n  \"receipt_id\": \"public-1\",\n  \"open_issues\": 0,\n  \"open_prs\": 0,\n  \"latest_dev_ci\": \"green\",\n  \"active_sessions_needing_action\": 0,\n  \"release_hold\": false,\n  \"raw_log\": \"different private log must not affect fingerprint\"\n}\n",
        )
            .expect("write pretty");

        let compact_eval = evaluate_state_file(&compact).expect("compact eval");
        let pretty_eval = evaluate_state_file(&pretty).expect("pretty eval");
        assert_eq!(
            compact_eval.fingerprint, pretty_eval.fingerprint,
            "whitespace-only formatting changes must not count as a delta"
        );
    }

    #[test]
    fn healthy_receipt_marks_validated_github_api_source() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("repo.json");
        write_zero_backlog_receipt(&path, None, false).expect("write receipt");

        let eval = evaluate_state_file(&path).expect("evaluation");
        assert!(eval.zero_backlog, "healthy validated receipt has authority");
        assert!(!eval.github_api_fallback);
        assert_eq!(eval.observation_source, "github-api");
        assert_eq!(eval.observation_confidence, "validated");
    }

    #[test]
    fn rate_limited_fallback_loses_zero_backlog_authority() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("repo.json");
        let value = json!({
            "family": "runtime-followup-receipt",
            "status": "validated",
            "receipt_id": "public-1",
            "open_issues": 0,
            "open_prs": 0,
            "latest_dev_ci": "green",
            "active_sessions_needing_action": 0,
            "release_hold": false,
            "github_api_status": "rate_limited",
        });
        fs::write(&path, serde_json::to_string(&value).unwrap()).expect("write receipt");

        let eval = evaluate_state_file(&path).expect("evaluation");
        assert!(
            !eval.zero_backlog,
            "a rate-limited fallback without evidence must not carry merge/close authority"
        );
        assert!(eval.github_api_fallback);
        assert_eq!(eval.observation_source, "github-api-fallback");
        assert_eq!(eval.observation_confidence, "fallback-unverified");
    }

    #[test]
    fn rate_limited_fallback_with_evidence_keeps_authority() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("repo.json");
        let value = json!({
            "family": "runtime-followup-receipt",
            "status": "validated",
            "receipt_id": "public-1",
            "open_issues": 0,
            "open_prs": 0,
            "latest_dev_ci": "green",
            "active_sessions_needing_action": 0,
            "release_hold": false,
            "github_api_status": "rate_limited",
            "observation_source": "public-repo-pages",
            "fallback_evidence": true,
        });
        fs::write(&path, serde_json::to_string(&value).unwrap()).expect("write receipt");

        let eval = evaluate_state_file(&path).expect("evaluation");
        assert!(
            eval.zero_backlog,
            "a fallback advertising the required evidence retains authority"
        );
        assert!(eval.github_api_fallback);
        assert_eq!(eval.observation_source, "public-repo-pages");
        assert_eq!(eval.observation_confidence, "validated");
    }

    #[test]
    fn api_fallback_is_distinct_from_empty_backlog() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("repo.json");
        // No backlog counters at all, but the API failed: the failure is detected
        // separately from an (absent) empty-backlog claim.
        fs::write(&path, r#"{"github_api_status":"error"}"#).expect("write receipt");

        let eval = evaluate_state_file(&path).expect("evaluation");
        assert!(!eval.zero_backlog);
        assert!(eval.github_api_fallback);
        assert_eq!(eval.observation_source, "github-api-fallback");
        assert_eq!(eval.observation_confidence, "fallback-unverified");
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

    fn write_lightweight_checkpoint(path: &Path, open_prs: u64) -> std::io::Result<()> {
        let checkpoint = crate::gajae::zero_backlog_followup_checkpoint(
            crate::gajae::ZeroBacklogCheckpointRequest {
                repo: "Yeachan-Heo/clawhip".into(),
                open_issues: 0,
                open_prs,
                action_needed_sessions: 0,
                observation_source: "github-api".into(),
                approval_hold: false,
                release_hold: false,
            },
        )
        .expect("build checkpoint");
        fs::write(path, serde_json::to_string(&checkpoint).expect("serialize"))
    }

    #[tokio::test]
    async fn lightweight_checkpoint_suppresses_repeated_followups_until_ttl() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        write_lightweight_checkpoint(&repo_state, 0).expect("write checkpoint");

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
            .expect("second tick suppressed");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 1, "routine follow-up should be silent");
        assert_eq!(events[0].payload["repo_state_zero_backlog"], json!(true));
    }

    #[tokio::test]
    async fn lightweight_checkpoint_with_open_backlog_does_not_suppress() {
        let dir = tempdir().expect("tempdir");
        let state_path = dir.path().join("cron-state.json");
        let repo_state = dir.path().join("repo.json");
        write_lightweight_checkpoint(&repo_state, 1).expect("write checkpoint");

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
            .expect("second tick");

        let events = emitter.events.lock().expect("events lock");
        assert_eq!(events.len(), 2, "nonzero backlog must keep emitting");
        assert_eq!(events[0].payload["repo_state_zero_backlog"], json!(false));
    }

    fn sample_config(schedule: &str) -> AppConfig {
        sample_config_with_state(schedule, None)
    }

    fn sample_config_with_state(schedule: &str, state_file: Option<PathBuf>) -> AppConfig {
        AppConfig {
            defaults: DefaultsConfig {
                channel: Some("ops".into()),
                channel_name: None,
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
                    zero_backlog_suppression_ttl_secs: 60 * 60,
                    kind: CronJobKind::CustomMessage {
                        message: "check open PRs".into(),
                    },
                }],
            },
            ..AppConfig::default()
        }
    }

    fn write_zero_backlog_receipt(
        path: &Path,
        new_event_id: Option<&str>,
        release_hold: bool,
    ) -> std::io::Result<()> {
        let mut value = json!({
            "family": "runtime-followup-receipt",
            "status": "validated",
            "receipt_id": "public-1",
            "open_issues": 0,
            "open_prs": 0,
            "latest_dev_ci": "green",
            "active_sessions_needing_action": 0,
            "release_hold": release_hold,
            "raw_log": "secret-token private channel transcript must not leak",
        });
        if let Some(new_event_id) = new_event_id {
            value["new_event_id"] = json!(new_event_id);
        }
        fs::write(path, serde_json::to_string(&value)?)
    }

    fn dt(year: i32, month: Month, day: u8, hour: u8, minute: u8, second: u8) -> OffsetDateTime {
        let date = Date::from_calendar_date(year, month, day).expect("date");
        let time = Time::from_hms(hour, minute, second).expect("time");
        PrimitiveDateTime::new(date, time).assume_utc()
    }
}
