use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::Result;
use crate::config::LedgerConfig;
use crate::events::IncomingEvent;

const SCHEMA_VERSION: u8 = 1;
const PUBLIC_STRING_FIELDS: &[&str] = &[
    "repo",
    "repo_name",
    "repo_path",
    "worktree",
    "worktree_path",
    "session_id",
    "project",
    "provider",
    "tool",
    "branch",
    "status",
    "source",
    "source_url",
    "html_url",
    "url",
    "commit",
    "sha",
    "number",
    "event_id",
    "idempotency_key",
    "timestamp",
    "event_timestamp",
    "observed_at",
    "created_at",
];
const PRIVATE_FIELD_MARKERS: &[&str] = &[
    "raw",
    "private",
    "secret",
    "token",
    "webhook",
    "prompt",
    "message",
    "content",
    "body",
    "command",
    "output",
    "stderr",
    "stdout",
    "authorization",
    "cookie",
];

pub type SharedEventLedger = std::sync::Arc<std::sync::Mutex<EventLedger>>;

type SummaryGroupKey = (i64, Option<String>, Option<String>, Option<String>);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LedgerRecord {
    pub schema_version: u8,
    pub id: String,
    pub dedupe_key: String,
    pub timestamp: String,
    pub timestamp_unix: i64,
    pub event_type: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_links: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LedgerSummary {
    pub schema_version: u8,
    pub shard_id: String,
    pub day: i64,
    pub repo: Option<String>,
    pub worktree: Option<String>,
    pub session_id: Option<String>,
    pub first_timestamp_unix: i64,
    pub last_timestamp_unix: i64,
    pub event_counts: BTreeMap<String, usize>,
    pub top_keywords: Vec<String>,
    pub source_record_ids: Vec<String>,
    #[serde(default)]
    pub source_dedupe_keys: Vec<String>,
    pub source_links: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LedgerStatus {
    pub enabled: bool,
    pub records: usize,
    pub appended: u64,
    pub duplicates: u64,
    pub rejected: u64,
    pub append_failures: u64,
    pub query_count: u64,
    pub compacted_records: u64,
    pub summary_shards: usize,
    pub raw_segments: usize,
    pub last_compaction_unix: Option<i64>,

    pub degraded: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LedgerQuery {
    pub repo: Option<String>,
    pub worktree: Option<String>,
    pub session_id: Option<String>,
    pub event_type: Option<String>,
    pub since_unix: Option<i64>,
    pub until_unix: Option<i64>,
    pub keywords: Vec<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Appended,
    Duplicate,
}

#[derive(Default)]
struct Indexes {
    repo: HashMap<String, BTreeSet<usize>>,
    worktree: HashMap<String, BTreeSet<usize>>,
    session: HashMap<String, BTreeSet<usize>>,
    event_type: HashMap<String, BTreeSet<usize>>,
    keyword: HashMap<String, BTreeSet<usize>>,
}

pub struct EventLedger {
    config: LedgerConfig,
    root: PathBuf,
    records: Vec<LedgerRecord>,
    dedupe: HashSet<String>,
    indexes: Indexes,
    status: LedgerStatus,
    last_compaction_unix: i64,
}

impl EventLedger {
    pub fn open(config: LedgerConfig, default_root: &Path) -> Result<Self> {
        let root = config
            .path
            .clone()
            .unwrap_or_else(|| default_root.join("event-ledger"));
        let mut ledger = Self {
            status: LedgerStatus {
                enabled: config.enabled,
                ..LedgerStatus::default()
            },
            config,
            root,
            records: Vec::new(),
            dedupe: HashSet::new(),
            indexes: Indexes::default(),
            last_compaction_unix: 0,
        };
        if ledger.config.enabled {
            fs::create_dir_all(ledger.raw_dir())?;
            fs::create_dir_all(ledger.summary_dir())?;
            ledger.load()?;
        }
        Ok(ledger)
    }

    pub fn append(&mut self, event: &IncomingEvent) -> Result<AppendOutcome> {
        if !self.config.enabled {
            return Ok(AppendOutcome::Appended);
        }
        let record = match LedgerRecord::from_event(event, &self.config) {
            Ok(record) => record,
            Err(error) => {
                self.status.rejected += 1;
                return Err(error);
            }
        };
        if self.dedupe.contains(&record.dedupe_key) {
            self.status.duplicates += 1;
            return Ok(AppendOutcome::Duplicate);
        }
        if self.records.len() >= self.config.max_records {
            self.status.rejected += 1;
            return Err("ledger_max_records_exceeded".into());
        }
        let encoded = serde_json::to_vec(&record)?;
        if encoded.len() > self.config.max_record_bytes {
            self.status.rejected += 1;
            return Err("ledger_record_too_large".into());
        }
        let path = self.segment_path(record.timestamp_unix.div_euclid(86_400));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new().create(true).append(true).open(path)?;
            file.write_all(&encoded)?;
            file.write_all(b"\n")?;
            file.flush()?;
            file.sync_data()?;
            Ok(())
        })();
        if let Err(error) = result {
            self.status.append_failures += 1;
            self.status.degraded = Some("append_failed".into());
            return Err(error);
        }
        self.insert(record);
        self.status.appended += 1;
        Ok(AppendOutcome::Appended)
    }

    pub fn query(&mut self, query: &LedgerQuery) -> Vec<LedgerRecord> {
        self.status.query_count += 1;
        let mut candidate: Option<BTreeSet<usize>> = None;
        let mut intersect = |set: Option<&BTreeSet<usize>>| {
            let next = set.cloned().unwrap_or_default();
            candidate = Some(match candidate.take() {
                Some(current) => current.intersection(&next).copied().collect(),
                None => next,
            });
        };
        if let Some(value) = query.repo.as_deref() {
            intersect(self.indexes.repo.get(value));
        }
        if let Some(value) = query.worktree.as_deref() {
            intersect(self.indexes.worktree.get(value));
        }
        if let Some(value) = query.session_id.as_deref() {
            intersect(self.indexes.session.get(value));
        }
        if let Some(value) = query.event_type.as_deref() {
            intersect(self.indexes.event_type.get(value));
        }
        for keyword in &query.keywords {
            intersect(self.indexes.keyword.get(&normalize_keyword(keyword)));
        }
        let indexes: Box<dyn Iterator<Item = usize>> = match candidate {
            Some(set) => Box::new(set.into_iter()),
            None => Box::new(0..self.records.len()),
        };
        let limit = query
            .limit
            .unwrap_or(self.config.max_query_results)
            .min(self.config.max_query_results);
        indexes
            .filter_map(|index| self.records.get(index))
            .filter(|record| {
                query
                    .since_unix
                    .is_none_or(|since| record.timestamp_unix >= since)
            })
            .filter(|record| {
                query
                    .until_unix
                    .is_none_or(|until| record.timestamp_unix <= until)
            })
            .take(limit)
            .cloned()
            .collect()
    }

    pub fn status(&self) -> LedgerStatus {
        let mut status = self.status.clone();
        status.records = self.records.len();
        status.raw_segments = jsonl_files(&self.raw_dir())
            .map(|paths| paths.len())
            .unwrap_or(0);
        status.last_compaction_unix =
            (self.last_compaction_unix > 0).then_some(self.last_compaction_unix);

        status.summary_shards = fs::read_dir(self.summary_dir())
            .map(|entries| entries.flatten().count())
            .unwrap_or(0);
        status
    }

    pub fn verify(&self) -> Result<LedgerStatus> {
        let mut verified = LedgerStatus {
            enabled: self.config.enabled,
            ..LedgerStatus::default()
        };
        if !self.config.enabled {
            return Ok(verified);
        }
        for path in jsonl_files(&self.raw_dir())? {
            let file = File::open(path)?;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let record: LedgerRecord = serde_json::from_str(&line)?;
                record.validate(&self.config)?;
                verified.records += 1;
            }
        }
        for path in json_files(&self.summary_dir())? {
            let summary: LedgerSummary = serde_json::from_reader(File::open(path)?)?;
            summary.validate(&self.config)?;
            verified.summary_shards += 1;
        }
        verified.raw_segments = jsonl_files(&self.raw_dir())?.len();

        Ok(verified)
    }

    pub fn compact_if_due(&mut self, now_unix: i64) -> Result<usize> {
        if !self.config.enabled
            || now_unix - self.last_compaction_unix < self.config.compaction_interval_secs as i64
        {
            return Ok(0);
        }
        self.last_compaction_unix = now_unix;
        let raw_cutoff = now_unix - (self.config.raw_retention_days as i64 * 86_400);
        let mut groups: BTreeMap<SummaryGroupKey, Vec<&LedgerRecord>> = BTreeMap::new();
        for record in self
            .records
            .iter()
            .filter(|record| record.timestamp_unix < raw_cutoff)
            .take(self.config.max_records_per_compaction)
        {
            groups
                .entry((
                    record.timestamp_unix.div_euclid(86_400),
                    record.repo.clone(),
                    record.worktree.clone(),
                    record.session_id.clone(),
                ))
                .or_default()
                .push(record);
        }
        let selected_ids: HashSet<String> = groups
            .values()
            .flatten()
            .map(|record| record.id.clone())
            .collect();

        let mut compacted = 0usize;
        for ((day, repo, worktree, session_id), records) in groups {
            for chunk in records.chunks(64) {
                let summary = LedgerSummary::from_records(
                    day,
                    repo.clone(),
                    worktree.clone(),
                    session_id.clone(),
                    chunk,
                    self.config.max_keywords,
                );
                let final_path = self
                    .summary_dir()
                    .join(format!("{}.json", summary.shard_id));
                let temp_path = final_path.with_extension("tmp");
                let encoded = serde_json::to_vec(&summary)?;
                let mut file = File::create(&temp_path)?;
                file.write_all(&encoded)?;
                file.write_all(b"\n")?;
                file.flush()?;
                file.sync_data()?;
                fs::rename(temp_path, final_path)?;
                compacted += chunk.len();
            }
        }
        if compacted > 0 {
            self.status.compacted_records += compacted as u64;
            self.remove_compacted_records(&selected_ids)?;
        }
        self.prune_summary_before(now_unix - self.config.summary_retention_days as i64 * 86_400)?;
        if compacted > 0 {
            self.load()?;
        }
        Ok(compacted)
    }

    fn load(&mut self) -> Result<()> {
        self.records.clear();
        self.dedupe.clear();
        self.indexes = Indexes::default();
        let mut loaded = Vec::new();
        for path in jsonl_files(&self.raw_dir())? {
            let file = File::open(path)?;
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let record: LedgerRecord = serde_json::from_str(&line)?;
                record.validate(&self.config)?;
                loaded.push(record);
            }
        }
        loaded.sort_by_key(|record| (record.timestamp_unix, record.id.clone()));
        if loaded.len() > self.config.max_records {
            loaded.drain(0..loaded.len() - self.config.max_records);
            self.status.degraded = Some("startup_record_cap_applied".into());
        }
        for record in loaded {
            if self.dedupe.insert(record.dedupe_key.clone()) {
                self.insert_indexed(record);
            }
        }
        let mut summaries = Vec::new();
        for path in json_files(&self.summary_dir())? {
            let summary: LedgerSummary = serde_json::from_reader(File::open(path)?)?;
            summary.validate(&self.config)?;
            summaries.push(summary);
        }
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.last_timestamp_unix));
        let mut dedupe_history_capped = false;
        for summary in summaries {
            for key in summary.source_dedupe_keys {
                if self.dedupe.len() >= self.config.max_records {
                    dedupe_history_capped = true;
                    break;
                }
                self.dedupe.insert(key);
            }
            if dedupe_history_capped {
                break;
            }
        }
        if dedupe_history_capped {
            self.status.degraded = Some("dedupe_history_cap_applied".into());
        }
        Ok(())
    }

    fn insert(&mut self, record: LedgerRecord) {
        self.dedupe.insert(record.dedupe_key.clone());
        self.insert_indexed(record);
    }

    fn insert_indexed(&mut self, record: LedgerRecord) {
        let index = self.records.len();
        index_value(&mut self.indexes.repo, record.repo.as_deref(), index);
        index_value(
            &mut self.indexes.worktree,
            record.worktree.as_deref(),
            index,
        );
        index_value(
            &mut self.indexes.session,
            record.session_id.as_deref(),
            index,
        );
        index_value(
            &mut self.indexes.event_type,
            Some(&record.event_type),
            index,
        );
        for keyword in &record.keywords {
            index_value(&mut self.indexes.keyword, Some(keyword), index);
        }
        self.records.push(record);
    }

    fn raw_dir(&self) -> PathBuf {
        self.root.join("raw")
    }
    fn summary_dir(&self) -> PathBuf {
        self.root.join("summaries")
    }
    fn segment_path(&self, day: i64) -> PathBuf {
        self.raw_dir().join(format!("events-{day}.jsonl"))
    }

    fn remove_compacted_records(&self, selected_ids: &HashSet<String>) -> Result<()> {
        let mut by_day = BTreeMap::<i64, Vec<&LedgerRecord>>::new();
        for record in &self.records {
            by_day
                .entry(record.timestamp_unix.div_euclid(86_400))
                .or_default()
                .push(record);
        }
        for (day, records) in by_day {
            if !records
                .iter()
                .any(|record| selected_ids.contains(&record.id))
            {
                continue;
            }
            let path = self.segment_path(day);
            let remaining = records
                .into_iter()
                .filter(|record| !selected_ids.contains(&record.id))
                .collect::<Vec<_>>();
            if remaining.is_empty() {
                if path.exists() {
                    fs::remove_file(path)?;
                }
                continue;
            }
            let temp_path = path.with_extension("tmp");
            let mut file = File::create(&temp_path)?;
            for record in remaining {
                serde_json::to_writer(&mut file, record)?;
                file.write_all(b"\n")?;
            }
            file.flush()?;
            file.sync_data()?;
            fs::rename(temp_path, path)?;
        }
        Ok(())
    }

    fn prune_summary_before(&self, cutoff: i64) -> Result<()> {
        if !self.summary_dir().exists() {
            return Ok(());
        }
        for entry in fs::read_dir(self.summary_dir())? {
            let entry = entry?;
            if entry
                .metadata()?
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(i64::MAX)
                < cutoff
            {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
}

impl LedgerRecord {
    fn from_event(event: &IncomingEvent, config: &LedgerConfig) -> Result<Self> {
        let object = event
            .payload
            .as_object()
            .ok_or("ledger_payload_must_be_object")?;
        reject_private_storage_requests(object)?;
        let timestamp = first_string(
            object,
            &["event_timestamp", "timestamp", "observed_at", "created_at"],
        )
        .and_then(|value| OffsetDateTime::parse(&value, &Rfc3339).ok())
        .unwrap_or_else(OffsetDateTime::now_utc);
        let repo = bounded(
            first_string(object, &["repo", "repo_name", "repo_path", "project"]),
            512,
        );
        let worktree = bounded(
            first_string(object, &["worktree", "worktree_path", "repo_path"]),
            1024,
        );
        let session_id = bounded(first_string(object, &["session_id"]), 256);
        let source = bounded(first_string(object, &["source", "provider", "tool"]), 128)
            .unwrap_or_else(|| "clawhip".into());
        let mut source_links = Vec::new();
        for key in ["source_url", "html_url", "url"] {
            if let Some(value) = object
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| is_public_link(value))
            {
                push_bounded_unique(&mut source_links, value, 1024, 8);
            }
        }
        let mut keywords = Vec::new();
        for token in event.canonical_kind().split(['.', '-', '_']) {
            push_keyword(&mut keywords, token, config);
        }
        for key in ["status", "branch", "repo", "repo_name", "provider", "tool"] {
            if let Some(value) = object.get(key).and_then(Value::as_str) {
                for token in
                    value.split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
                {
                    push_keyword(&mut keywords, token, config);
                }
            }
        }
        if let Some(values) = object.get("keywords").and_then(Value::as_array) {
            for value in values.iter().filter_map(Value::as_str) {
                push_keyword(&mut keywords, value, config);
            }
        }
        let supplied = first_string(object, &["idempotency_key", "event_id"]);
        let timestamp_text = timestamp.format(&Rfc3339)?;
        let identity = serde_json::json!({
            "event_type": event.canonical_kind(), "timestamp": timestamp_text, "source": source,
            "repo": repo, "worktree": worktree, "session_id": session_id, "source_links": source_links,
            "identity": PUBLIC_STRING_FIELDS.iter().filter_map(|key| object.get(*key).and_then(Value::as_str).map(|value| (*key, value))).collect::<BTreeMap<_, _>>()
        });
        let dedupe_key = supplied
            .map(|value| hash_bytes(value.as_bytes()))
            .unwrap_or_else(|| {
                hash_bytes(
                    serde_json::to_string(&identity)
                        .unwrap_or_default()
                        .as_bytes(),
                )
            });
        let id = hash_bytes(format!("{dedupe_key}:{timestamp_text}").as_bytes());
        let record = Self {
            schema_version: SCHEMA_VERSION,
            id,
            dedupe_key,
            timestamp: timestamp_text,
            timestamp_unix: timestamp.unix_timestamp(),
            event_type: event.canonical_kind().to_string(),
            source,
            repo,
            worktree,
            session_id,
            source_links,
            keywords,
        };
        record.validate(config)?;
        Ok(record)
    }

    fn validate(&self, config: &LedgerConfig) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION
            || self.id.len() != 64
            || self.dedupe_key.len() != 64
            || self.event_type.is_empty()
        {
            return Err("invalid_ledger_record".into());
        }
        if self.keywords.len() > config.max_keywords
            || self
                .keywords
                .iter()
                .any(|value| value.len() > config.max_keyword_bytes)
        {
            return Err("invalid_ledger_keywords".into());
        }
        let encoded = serde_json::to_vec(self)?;
        if encoded.len() > config.max_record_bytes {
            return Err("ledger_record_too_large".into());
        }
        Ok(())
    }
}

impl LedgerSummary {
    fn from_records(
        day: i64,
        repo: Option<String>,
        worktree: Option<String>,
        session_id: Option<String>,
        records: &[&LedgerRecord],
        max_keywords: usize,
    ) -> Self {
        let mut event_counts = BTreeMap::new();
        let mut keyword_counts = BTreeMap::<String, usize>::new();
        let mut source_record_ids = Vec::new();
        let mut source_dedupe_keys = Vec::new();
        let mut source_links = Vec::new();
        for record in records {
            *event_counts.entry(record.event_type.clone()).or_default() += 1;
            for keyword in &record.keywords {
                *keyword_counts.entry(keyword.clone()).or_default() += 1;
            }
            if source_record_ids.len() < 64 {
                source_record_ids.push(record.id.clone());
                source_dedupe_keys.push(record.dedupe_key.clone());
            }
            for link in &record.source_links {
                push_bounded_unique(&mut source_links, link, 1024, 32);
            }
        }
        let mut ranked: Vec<_> = keyword_counts.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let top_keywords = ranked
            .into_iter()
            .take(max_keywords)
            .map(|(keyword, _)| keyword)
            .collect();
        let first_timestamp_unix = records
            .iter()
            .map(|record| record.timestamp_unix)
            .min()
            .unwrap_or(day * 86_400);
        let last_timestamp_unix = records
            .iter()
            .map(|record| record.timestamp_unix)
            .max()
            .unwrap_or(day * 86_400);
        let shard_id = hash_bytes(
            serde_json::to_string(&(day, &repo, &worktree, &session_id, &source_record_ids))
                .unwrap_or_default()
                .as_bytes(),
        );

        Self {
            schema_version: SCHEMA_VERSION,
            shard_id,
            day,
            repo,
            worktree,
            session_id,
            first_timestamp_unix,
            last_timestamp_unix,
            event_counts,
            top_keywords,
            source_record_ids,
            source_dedupe_keys,
            source_links,
        }
    }
}

impl LedgerSummary {
    fn validate(&self, config: &LedgerConfig) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION
            || self.shard_id.len() != 64
            || self.source_record_ids.len() > 64
            || self.source_dedupe_keys.len() != self.source_record_ids.len()
            || self.source_dedupe_keys.iter().any(|key| key.len() != 64)
            || self.source_links.len() > 32
            || self.top_keywords.len() > config.max_keywords
        {
            return Err("invalid_ledger_summary".into());
        }
        Ok(())
    }
}

fn reject_private_storage_requests(object: &serde_json::Map<String, Value>) -> Result<()> {
    for key in object.keys() {
        let lower = key.to_ascii_lowercase();
        if PRIVATE_FIELD_MARKERS
            .iter()
            .any(|marker| lower.contains(marker))
            && matches!(
                lower.as_str(),
                "raw" | "raw_payload" | "private" | "private_payload" | "retain_raw" | "store_raw"
            )
        {
            return Err("ledger_raw_private_payload_rejected".into());
        }
    }
    Ok(())
}

fn first_string(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str).map(str::to_owned))
}
fn bounded(value: Option<String>, max: usize) -> Option<String> {
    value
        .map(|value| value.chars().take(max).collect())
        .filter(|value: &String| !value.is_empty())
}
fn normalize_keyword(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}
fn push_keyword(values: &mut Vec<String>, value: &str, config: &LedgerConfig) {
    let value = normalize_keyword(value);
    if value.len() < 2
        || value.len() > config.max_keyword_bytes
        || values.len() >= config.max_keywords
        || values.contains(&value)
    {
        return;
    }
    values.push(value);
}
fn push_bounded_unique(values: &mut Vec<String>, value: &str, max_bytes: usize, max_count: usize) {
    if values.len() >= max_count
        || value.len() > max_bytes
        || values.iter().any(|existing| existing == value)
    {
        return;
    }
    values.push(value.to_owned());
}
fn is_public_link(value: &str) -> bool {
    if value.len() > 1024 {
        return false;
    }
    reqwest::Url::parse(value).ok().is_some_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

fn hash_bytes(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}
fn index_value(map: &mut HashMap<String, BTreeSet<usize>>, value: Option<&str>, index: usize) {
    if let Some(value) = value {
        map.entry(value.to_owned()).or_default().insert(index);
    }
}
fn jsonl_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

fn json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect::<Vec<_>>();
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn config(path: &Path) -> LedgerConfig {
        LedgerConfig {
            enabled: true,
            path: Some(path.to_path_buf()),
            raw_retention_days: 7,
            summary_retention_days: 30,
            compaction_interval_secs: 1,
            max_records: 100,
            max_record_bytes: 4096,
            max_keywords: 8,
            max_keyword_bytes: 32,
            max_query_results: 50,
            max_records_per_compaction: 50,
        }
    }

    #[test]
    fn append_is_public_safe_indexed_and_deduped_across_restart() {
        let dir = tempdir().unwrap();
        let cfg = config(dir.path());
        let event = IncomingEvent {
            kind: "agent.finished".into(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: json!({"event_id":"evt-1","repo":"owner/repo","worktree_path":"/tmp/repo","session_id":"s1","status":"finished","summary":"must-not-appear","token":"secret","source_url":"https://github.com/owner/repo/issues/304","timestamp":"2026-08-01T12:00:00Z"}),
        };
        let mut ledger = EventLedger::open(cfg.clone(), dir.path()).unwrap();
        assert_eq!(ledger.append(&event).unwrap(), AppendOutcome::Appended);
        assert_eq!(ledger.append(&event).unwrap(), AppendOutcome::Duplicate);
        let bytes = fs::read_to_string(ledger.segment_path(20666)).unwrap();
        assert!(!bytes.contains("must-not-appear"));
        assert!(!bytes.contains("secret"));
        assert!(!bytes.contains("summary"));
        let mut reopened = EventLedger::open(cfg, dir.path()).unwrap();
        assert_eq!(reopened.append(&event).unwrap(), AppendOutcome::Duplicate);
        let records = reopened.query(&LedgerQuery {
            repo: Some("owner/repo".into()),
            session_id: Some("s1".into()),
            event_type: Some("agent.finished".into()),
            keywords: vec!["finished".into()],
            ..LedgerQuery::default()
        });
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn rejects_explicit_raw_private_retention() {
        let dir = tempdir().unwrap();
        let mut ledger = EventLedger::open(config(dir.path()), dir.path()).unwrap();
        let event = IncomingEvent {
            kind: "custom".into(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: json!({"event_id":"evt-2","raw_payload":{"secret":true}}),
        };
        assert!(
            ledger
                .append(&event)
                .unwrap_err()
                .to_string()
                .contains("raw_private")
        );
        assert_eq!(ledger.status().records, 0);
    }

    #[test]
    fn compaction_writes_source_linked_summary_then_prunes_raw() {
        let dir = tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.raw_retention_days = 1;
        let mut ledger = EventLedger::open(cfg.clone(), dir.path()).unwrap();
        let event = IncomingEvent {
            kind: "github.issue-opened".into(),
            channel: None,
            mention: None,
            format: None,
            template: None,
            payload: json!({"event_id":"old","repo":"owner/repo","session_id":"s1","source_url":"https://github.com/owner/repo/issues/304","timestamp":"2026-07-01T00:00:00Z"}),
        };
        ledger.append(&event).unwrap();
        let now = OffsetDateTime::parse("2026-08-03T00:00:00Z", &Rfc3339)
            .unwrap()
            .unix_timestamp();
        assert_eq!(ledger.compact_if_due(now).unwrap(), 1);
        let summaries = fs::read_dir(ledger.summary_dir()).unwrap().count();
        assert_eq!(summaries, 1);
        assert_eq!(ledger.status().records, 0);
        let mut reopened = EventLedger::open(cfg, dir.path()).unwrap();
        assert_eq!(reopened.append(&event).unwrap(), AppendOutcome::Duplicate);
    }

    #[test]
    fn compaction_chunks_summaries_and_retains_every_dedupe_key() {
        let dir = tempdir().unwrap();
        let mut cfg = config(dir.path());
        cfg.raw_retention_days = 1;
        cfg.max_records = 200;
        cfg.max_records_per_compaction = 100;
        let mut ledger = EventLedger::open(cfg.clone(), dir.path()).unwrap();
        let mut events = Vec::new();
        for index in 0..65 {
            let event = IncomingEvent {
                kind: "agent.finished".into(),
                channel: None,
                mention: None,
                format: None,
                template: None,
                payload: json!({
                    "event_id": format!("old-{index}"),
                    "repo": "owner/repo",
                    "session_id": "s1",
                    "timestamp": "2026-07-01T00:00:00Z"
                }),
            };
            ledger.append(&event).unwrap();
            events.push(event);
        }
        let now = OffsetDateTime::parse("2026-08-03T00:00:00Z", &Rfc3339)
            .unwrap()
            .unix_timestamp();
        assert_eq!(ledger.compact_if_due(now).unwrap(), 65);
        assert_eq!(fs::read_dir(ledger.summary_dir()).unwrap().count(), 2);

        let mut reopened = EventLedger::open(cfg, dir.path()).unwrap();
        for event in events {
            assert_eq!(reopened.append(&event).unwrap(), AppendOutcome::Duplicate);
        }
    }
}
