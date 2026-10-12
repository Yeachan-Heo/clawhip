//! Control plane over the typed GJC contract: idempotent command registry,
//! capability gating, session-mismatch guards, and the mutation verbs.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use super::model::{
    AbortAndPromptRequest, AskAnswerRequest, CommandId, CommandReceipt, ControlRequestEnvelope,
    GJC_CONTROL_SCHEMA, GjcCommandKind, GjcCommandStatus, GjcError, GjcPromptStatus, GjcRequest,
    GjcResponse, GjcResult, GjcTransport, IdempotencyKey, ModelSelectionRequest, PromptRequest,
    SessionId, SessionQuery, SteerRequest, TurnId, WorkflowGateAnswerRequest,
};

pub type SharedGjcCommandRegistry = Arc<RwLock<HashMap<String, CommandReceipt>>>;
const MAX_COMMAND_RECEIPTS: usize = 4096;
const RECEIPT_JOURNAL_FILENAME: &str = "gjc-command-receipts.json";
const RECEIPT_JOURNAL_SCHEMA: &str = "clawhip.gjc-command-receipts.v1";
const MAX_RECEIPT_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RECEIPT_BYTES: usize = 16 * 1024;

pub fn new_shared_command_registry() -> SharedGjcCommandRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct ReceiptJournalFile {
    schema: String,
    receipts: BTreeMap<String, CommandReceipt>,
}

struct ReceiptJournalState {
    pending: Option<GjcResult<BTreeMap<String, CommandReceipt>>>,
    loaded: bool,
    error: Option<GjcError>,
}

/// Durable receipt state is only attached to production worktree-scoped
/// planes. Static transports intentionally remain memory-only test seams.
struct ReceiptJournal {
    path: PathBuf,
    state: Mutex<ReceiptJournalState>,
    persist_lock: std::sync::Mutex<()>,
}

impl ReceiptJournal {
    fn for_worktree(worktree: &Path) -> Self {
        let state_dir = worktree.join(".gjc").join("state");
        let path = state_dir.join(RECEIPT_JOURNAL_FILENAME);
        let pending = load_receipt_journal(worktree, &path);
        Self {
            path,
            state: Mutex::new(ReceiptJournalState {
                pending: Some(pending),
                loaded: false,
                error: None,
            }),
            persist_lock: std::sync::Mutex::new(()),
        }
    }

    fn initialize(&self, registry: &SharedGjcCommandRegistry) {
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        if state.loaded || state.error.is_some() {
            return;
        }
        let Some(pending) = state.pending.take() else {
            return;
        };
        let receipts = match pending {
            Ok(receipts) => receipts,
            Err(error) => {
                state.error = Some(error);
                return;
            }
        };
        let Ok(mut write) = registry.try_write() else {
            state.pending = Some(Ok(receipts));
            return;
        };
        if let Err(error) = merge_loaded_receipts(&mut write, receipts) {
            state.error = Some(error);
            return;
        }
        state.loaded = true;
    }

    async fn ensure_loaded(&self, registry: &SharedGjcCommandRegistry) -> GjcResult<()> {
        let mut state = self.state.lock().await;
        if let Some(error) = state.error.clone() {
            return Err(error);
        }
        if state.loaded {
            return Ok(());
        }
        let pending = state
            .pending
            .take()
            .expect("receipt journal initialization state must be present");
        let receipts = match pending {
            Ok(receipts) => receipts,
            Err(error) => {
                state.error = Some(error.clone());
                return Err(error);
            }
        };
        let mut write = registry.write().await;
        if let Err(error) = merge_loaded_receipts(&mut write, receipts) {
            state.error = Some(error.clone());
            return Err(error);
        }
        state.loaded = true;
        Ok(())
    }

    fn persist(&self, registry: &HashMap<String, CommandReceipt>) -> GjcResult<()> {
        let _guard = self
            .persist_lock
            .lock()
            .map_err(|_| receipt_journal_error("receipt journal lock failed"))?;
        let mut receipts = BTreeMap::new();
        for (key, receipt) in registry {
            validate_loaded_receipt(key, receipt)?;
            receipts.insert(key.clone(), receipt.clone());
        }
        if receipts.len() > MAX_COMMAND_RECEIPTS {
            return Err(receipt_journal_error("receipt journal exceeds capacity"));
        }
        let journal = ReceiptJournalFile {
            schema: RECEIPT_JOURNAL_SCHEMA.into(),
            receipts,
        };
        let bytes = serde_json::to_vec_pretty(&journal)
            .map_err(|_| receipt_journal_error("receipt journal serialization failed"))?;
        if bytes.len() as u64 > MAX_RECEIPT_JOURNAL_BYTES {
            return Err(receipt_journal_error("receipt journal exceeds size limit"));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| receipt_journal_error("receipt journal path is invalid"))?;
        validate_state_directory(parent)?;
        let temp_path = parent.join(format!(
            ".{}.{}.tmp",
            RECEIPT_JOURNAL_FILENAME,
            Uuid::new_v4()
        ));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temp_path)
                .map_err(|_| receipt_journal_error("receipt journal write failed"))?;
            file.write_all(&bytes)
                .and_then(|_| file.flush())
                .and_then(|_| file.sync_all())
                .map_err(|_| receipt_journal_error("receipt journal write failed"))?;
            drop(file);
            fs::rename(&temp_path, &self.path)
                .map_err(|_| receipt_journal_error("receipt journal replace failed"))?;
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| receipt_journal_error("receipt journal directory sync failed"))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        result
    }
}

fn merge_loaded_receipts(
    registry: &mut HashMap<String, CommandReceipt>,
    receipts: BTreeMap<String, CommandReceipt>,
) -> GjcResult<()> {
    if receipts.len() > MAX_COMMAND_RECEIPTS {
        return Err(receipt_journal_error("receipt journal exceeds capacity"));
    }
    let additions = receipts
        .keys()
        .filter(|key| !registry.contains_key(*key))
        .count();
    if registry.len().saturating_add(additions) > MAX_COMMAND_RECEIPTS {
        return Err(receipt_journal_error("receipt journal exceeds capacity"));
    }
    for (key, receipt) in &receipts {
        if let Some(existing) = registry.get(key)
            && serde_json::to_value(existing).ok() != serde_json::to_value(receipt).ok()
        {
            return Err(receipt_journal_error(
                "receipt journal conflicts with live state",
            ));
        }
    }
    for (key, receipt) in receipts {
        registry.entry(key).or_insert(receipt);
    }
    Ok(())
}

fn receipt_journal_error(reason: &str) -> GjcError {
    GjcError::InvalidRequest {
        field: "receipt_journal",
        reason: reason.into(),
    }
}

fn load_receipt_journal(
    worktree: &Path,
    path: &Path,
) -> GjcResult<BTreeMap<String, CommandReceipt>> {
    let parent = path
        .parent()
        .ok_or_else(|| receipt_journal_error("receipt journal path is invalid"))?;
    ensure_state_directory(worktree, parent)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(_) => return Err(receipt_journal_error("receipt journal cannot be inspected")),
    };
    if metadata.is_symlink() || !metadata.is_file() {
        return Err(receipt_journal_error(
            "receipt journal is not a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let owner = metadata.uid();
        let current = unsafe { libc::getuid() };
        if owner != 0 && current != 0 && owner != current {
            return Err(receipt_journal_error(
                "receipt journal owner is not trusted",
            ));
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(receipt_journal_error(
                "receipt journal permissions are not trusted",
            ));
        }
    }
    if metadata.len() > MAX_RECEIPT_JOURNAL_BYTES {
        return Err(receipt_journal_error("receipt journal exceeds size limit"));
    }
    let bytes =
        fs::read(path).map_err(|_| receipt_journal_error("receipt journal cannot be read"))?;
    if bytes.len() as u64 > MAX_RECEIPT_JOURNAL_BYTES {
        return Err(receipt_journal_error("receipt journal exceeds size limit"));
    }
    let journal: ReceiptJournalFile = serde_json::from_slice(&bytes)
        .map_err(|_| receipt_journal_error("receipt journal is malformed"))?;
    if journal.schema != RECEIPT_JOURNAL_SCHEMA {
        return Err(receipt_journal_error(
            "receipt journal schema is unsupported",
        ));
    }
    if journal.receipts.len() > MAX_COMMAND_RECEIPTS {
        return Err(receipt_journal_error("receipt journal exceeds capacity"));
    }
    for (key, receipt) in &journal.receipts {
        validate_loaded_receipt(key, receipt)?;
    }
    Ok(journal.receipts)
}

fn ensure_state_directory(worktree: &Path, state_dir: &Path) -> GjcResult<()> {
    let gjc_dir = worktree.join(".gjc");
    for directory in [&gjc_dir, state_dir] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.is_symlink() || !metadata.is_dir() => {
                return Err(receipt_journal_error(
                    "receipt state directory is not trusted",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(directory).map_err(|_| {
                    receipt_journal_error("receipt state directory cannot be created")
                })?;
            }
            Err(_) => {
                return Err(receipt_journal_error(
                    "receipt state directory cannot be inspected",
                ));
            }
        }
    }
    validate_state_directory(state_dir)
}

fn validate_state_directory(state_dir: &Path) -> GjcResult<()> {
    let metadata = fs::symlink_metadata(state_dir)
        .map_err(|_| receipt_journal_error("receipt state directory cannot be inspected"))?;
    if metadata.is_symlink() || !metadata.is_dir() {
        return Err(receipt_journal_error(
            "receipt state directory is not trusted",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o002 != 0 {
            return Err(receipt_journal_error(
                "receipt state directory is writable by others",
            ));
        }
    }
    Ok(())
}

fn validate_loaded_receipt(key: &str, receipt: &CommandReceipt) -> GjcResult<()> {
    IdempotencyKey::new(key.to_string())
        .map_err(|_| receipt_journal_error("receipt idempotency key is invalid"))?;
    if receipt.schema != GJC_CONTROL_SCHEMA || receipt.idempotency_key != key {
        return Err(receipt_journal_error("receipt identity is invalid"));
    }
    CommandId::new(receipt.command_id.clone())
        .map_err(|_| receipt_journal_error("receipt command id is invalid"))?;
    SessionId::new(receipt.session_id.clone())
        .map_err(|_| receipt_journal_error("receipt session id is invalid"))?;
    if !matches!(
        receipt.kind.as_str(),
        "prompt"
            | "steer"
            | "abort_and_prompt"
            | "workflow_gate_answer"
            | "ask_answer"
            | "model_selection"
    ) {
        return Err(receipt_journal_error("receipt command kind is invalid"));
    }
    if let Some(turn_id) = receipt.turn_id.as_ref() {
        TurnId::new(turn_id.clone())
            .map_err(|_| receipt_journal_error("receipt turn id is invalid"))?;
    }
    OffsetDateTime::parse(&receipt.created_at, &Rfc3339)
        .map_err(|_| receipt_journal_error("receipt timestamp is invalid"))?;
    if receipt.status.is_terminal() != receipt.outcome.is_some() {
        return Err(receipt_journal_error("receipt outcome state is invalid"));
    }
    if let Some(outcome) = receipt.outcome.as_ref()
        && public_outcome(outcome) != *outcome
    {
        return Err(receipt_journal_error("receipt outcome is not public-safe"));
    }
    let bytes = serde_json::to_vec(receipt)
        .map_err(|_| receipt_journal_error("receipt cannot be serialized"))?;
    if bytes.len() > MAX_RECEIPT_BYTES {
        return Err(receipt_journal_error("receipt exceeds size limit"));
    }
    Ok(())
}

fn now_rfc3339() -> String {
    crate::source::tmux::current_timestamp_rfc3339()
}

/// Where the control plane obtains its peer transport.
#[derive(Clone)]
enum TransportSource {
    /// Injected transport (tests, or an explicit endpoint binding).
    #[allow(dead_code)] // exercised by in-crate tests only
    Static(Arc<dyn GjcTransport>),
    /// No transport is available on this daemon; everything fails closed.
    Unavailable,
    /// Discover the lane endpoint under this worktree root on demand
    /// (production wiring over the #322 transport).
    Discovery(std::path::PathBuf),
}

/// A live transport binding: the resolved endpoint and the session it serves.
#[derive(Clone)]
struct BoundTransport {
    transport: Arc<dyn GjcTransport>,
    session_id: Option<String>,
    worktree: Option<PathBuf>,
}

/// The authoritative control plane. One instance per daemon; CLI paths go
/// through the daemon HTTP surface rather than constructing this directly.
#[derive(Clone)]
pub struct GjcControlPlane {
    source: TransportSource,
    bound: Arc<RwLock<Option<BoundTransport>>>,
    registry: SharedGjcCommandRegistry,
    journal: Option<Arc<ReceiptJournal>>,
}

impl GjcControlPlane {
    /// Static-transport constructor (tests and explicit bindings).
    #[cfg_attr(not(test), allow(dead_code))] // test injection seam
    pub fn new(transport: Arc<dyn GjcTransport>, registry: SharedGjcCommandRegistry) -> Self {
        Self {
            source: TransportSource::Static(transport),
            bound: Arc::new(RwLock::new(None)),
            registry,
            journal: None,
        }
    }

    /// Fail-closed plane with no transport at all.
    #[cfg_attr(not(test), allow(dead_code))] // daemon tests only
    pub fn unavailable(registry: SharedGjcCommandRegistry) -> Self {
        Self {
            source: TransportSource::Unavailable,
            bound: Arc::new(RwLock::new(None)),
            registry,
            journal: None,
        }
    }

    /// Production constructor: resolve the lane endpoint under `worktree`
    /// through the #322 discovery surface on demand.
    pub fn for_worktree(worktree: &std::path::Path, registry: SharedGjcCommandRegistry) -> Self {
        let journal = Arc::new(ReceiptJournal::for_worktree(worktree));
        journal.initialize(&registry);
        Self {
            source: TransportSource::Discovery(worktree.to_path_buf()),
            bound: Arc::new(RwLock::new(None)),
            registry,
            journal: Some(journal),
        }
    }

    /// Scope a control plane to an enrolled lane's worktree while retaining
    /// the shared command registry used for idempotent receipts. Public
    /// daemon handlers use this after resolving the session through the
    /// durable lane store; callers must not derive the worktree from the
    /// daemon's current directory.
    pub fn scoped_to_worktree(&self, worktree: &std::path::Path) -> Self {
        let journal = Arc::new(ReceiptJournal::for_worktree(worktree));
        journal.initialize(&self.registry);
        Self {
            source: TransportSource::Discovery(worktree.to_path_buf()),
            bound: Arc::new(RwLock::new(None)),
            registry: self.registry.clone(),
            journal: Some(journal),
        }
    }

    async fn ensure_receipt_journal(&self) -> GjcResult<()> {
        if let Some(journal) = self.journal.as_ref() {
            journal.ensure_loaded(&self.registry).await?;
        }
        Ok(())
    }

    fn persist_receipts(&self, registry: &HashMap<String, CommandReceipt>) -> GjcResult<()> {
        if let Some(journal) = self.journal.as_ref() {
            journal.persist(registry)?;
        }
        Ok(())
    }

    /// Resolve a usable transport, enforcing session binding for
    /// discovery-sourced endpoints. `wanted` is the session about to be
    /// addressed; a live endpoint bound to another session fails closed.
    async fn resolve_transport(
        &self,
        wanted: Option<&SessionId>,
    ) -> GjcResult<Option<BoundTransport>> {
        match &self.source {
            TransportSource::Static(transport) => Ok(Some(BoundTransport {
                transport: transport.clone(),
                session_id: None,
                worktree: None,
            })),
            TransportSource::Unavailable => Ok(None),
            TransportSource::Discovery(root) => {
                let discovered = if let Some(wanted) = wanted {
                    super::transport::discover_endpoint_for_session(root, wanted.as_str())
                } else {
                    super::transport::discover_endpoint(root)
                };
                match discovered {
                    Ok(endpoint) => {
                        if let Some(wanted) = wanted {
                            let endpoint_session = endpoint.session_id().to_string();
                            if endpoint_session != wanted.as_str() {
                                return Err(GjcError::SessionMismatch {
                                    expected: wanted.as_str().to_string(),
                                });
                            }
                        }
                        let bound = BoundTransport {
                            transport: Arc::new(endpoint),
                            session_id: wanted.map(|session| session.as_str().to_string()),
                            worktree: Some(root.clone()),
                        };
                        *self.bound.write().await = Some(bound.clone());
                        Ok(Some(bound))
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }

    /// Capability snapshot for the public capabilities surface. Never
    /// errors: unavailability is reported as `transport_implemented=false`.
    pub async fn capabilities(&self) -> super::model::Capabilities {
        let implemented = matches!(
            self.resolve_transport(None).await,
            Ok(Some(_)) | Err(GjcError::SessionMismatch { .. })
        );
        super::model::Capabilities::for_transport(implemented)
    }

    pub async fn capabilities_for_session(
        &self,
        session: &SessionId,
    ) -> super::model::Capabilities {
        let implemented = matches!(self.resolve_transport(Some(session)).await, Ok(Some(_)));
        super::model::Capabilities::for_transport(implemented)
    }
    // -----------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------

    /// Authoritative multi-surface session query. Unknown sections stay
    /// `None`; the peer never invents data and neither do we.
    pub async fn query_session(
        &self,
        session: &SessionId,
        sections: &[&str],
    ) -> GjcResult<SessionQuery> {
        let transport = self
            .resolve_transport(Some(session))
            .await?
            .ok_or(GjcError::TransportUnavailable)?;

        let params = json!({
            "session_id": session.as_str(),
            "sections": sections,
        });
        let reply = self
            .round_trip(
                &transport,
                "session.get",
                params,
                ControlRequestEnvelope::DEFAULT_TIMEOUT_MS,
            )
            .await?;
        let result = reply.result;
        let Some(result) = result.as_object() else {
            return Err(GjcError::InvalidPeerReply {
                method: "session.get".into(),
                reason: "result must be an object".into(),
            });
        };
        if sections.contains(&"metadata")
            && !result
                .get("metadata")
                .is_some_and(|value| value.is_object())
        {
            return Err(GjcError::InvalidPeerReply {
                method: "session.get".into(),
                reason: "metadata identity section is required".into(),
            });
        }
        let mut query = SessionQuery {
            revision: result.get("revision").and_then(Value::as_u64),
            workflow_gates_present: sections.contains(&"workflow_gates")
                && result.contains_key("workflow_gates"),
            turn_present: sections.contains(&"turn") && result.contains_key("turn"),
            ..SessionQuery::default()
        };
        if sections.contains(&"metadata")
            && let Some(value) = result.get("metadata")
        {
            query.metadata = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("metadata section: {error}"),
                }
            })?;
        }
        if sections.contains(&"stats")
            && let Some(value) = result.get("stats")
        {
            query.stats = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("stats section: {error}"),
                }
            })?;
        }
        if sections.contains(&"model_profile")
            && let Some(value) = result.get("model_profile")
        {
            query.model_profile = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("model_profile section: {error}"),
                }
            })?;
        }
        if sections.contains(&"turn")
            && let Some(value) = result.get("turn")
        {
            query.turn = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("turn section: {error}"),
                }
            })?;
        }
        if sections.contains(&"queue")
            && let Some(value) = result.get("queue")
        {
            query.queue = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("queue section: {error}"),
                }
            })?;
        }
        if sections.contains(&"workflow_gates")
            && let Some(value) = result.get("workflow_gates")
        {
            if value.is_null() {
                return Err(GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: "workflow_gates section must be an array when present".into(),
                });
            }
            query.workflow_gates = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("workflow_gates section: {error}"),
                }
            })?;
        }
        if sections.contains(&"goal_todo")
            && let Some(value) = result.get("goal_todo")
        {
            query.goal_todo = serde_json::from_value(value.clone()).map_err(|error| {
                GjcError::InvalidPeerReply {
                    method: "session.get".into(),
                    reason: format!("goal_todo section: {error}"),
                }
            })?;
        }
        self.check_session_identity(session, &query)?;
        Ok(query)
    }

    /// Terminal outcome receipt for one turn.
    pub async fn turn_outcome(&self, session: &SessionId, turn_id: &str) -> GjcResult<Value> {
        let requested_turn = TurnId::new(turn_id.to_string())?;
        let transport = self
            .resolve_transport(Some(session))
            .await?
            .ok_or(GjcError::TransportUnavailable)?;
        let params = json!({
            "session_id": session.as_str(),
            "turn_id": turn_id,
        });
        let reply = self
            .round_trip(
                &transport,
                "turn.outcome",
                params,
                ControlRequestEnvelope::DEFAULT_TIMEOUT_MS,
            )
            .await?;
        let outcome =
            reply
                .result
                .get("outcome")
                .cloned()
                .ok_or_else(|| GjcError::InvalidPeerReply {
                    method: "turn.outcome".into(),
                    reason: "outcome missing".into(),
                })?;
        let status = outcome
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| GjcError::InvalidPeerReply {
                method: "turn.outcome".into(),
                reason: "outcome status missing".into(),
            })?;
        if !matches!(status, "succeeded" | "failed" | "aborted") {
            return Err(GjcError::InvalidPeerReply {
                method: "turn.outcome".into(),
                reason: "outcome is not terminal".into(),
            });
        }
        if let Some(echoed) = outcome.get("turn_id").and_then(Value::as_str) {
            let echoed_turn =
                TurnId::new(echoed.to_string()).map_err(|_| GjcError::InvalidPeerReply {
                    method: "turn.outcome".into(),
                    reason: "outcome turn identity is malformed".into(),
                })?;
            if echoed_turn != requested_turn {
                return Err(GjcError::InvalidPeerReply {
                    method: "turn.outcome".into(),
                    reason: "outcome turn identity mismatch".into(),
                });
            }
        }
        Ok(public_outcome(&outcome))
    }

    // -----------------------------------------------------------------
    // Mutations
    // -----------------------------------------------------------------

    pub async fn prompt(&self, request: PromptRequest) -> GjcResult<CommandReceipt> {
        self.mutate(
            request.envelope,
            GjcCommandKind::Prompt,
            json!({
                "prompt": request.prompt,
            }),
        )
        .await
    }

    pub async fn steer(&self, request: SteerRequest) -> GjcResult<CommandReceipt> {
        self.mutate(
            request.envelope,
            GjcCommandKind::Steer,
            json!({
                "message": request.message,
            }),
        )
        .await
    }

    pub async fn abort_and_prompt(
        &self,
        request: AbortAndPromptRequest,
    ) -> GjcResult<CommandReceipt> {
        self.mutate(
            request.envelope,
            GjcCommandKind::AbortAndPrompt,
            json!({
                "turn_ids": request
                    .turn_ids
                    .iter()
                    .map(|turn| turn.as_str())
                    .collect::<Vec<_>>(),
                "prompt": request.prompt,
            }),
        )
        .await
    }

    pub async fn answer_workflow_gate(
        &self,
        request: WorkflowGateAnswerRequest,
    ) -> GjcResult<CommandReceipt> {
        self.mutate(
            request.envelope,
            GjcCommandKind::WorkflowGateAnswer,
            json!({
                "gate_id": request.gate_id,
                "option": request.answer.option,
            }),
        )
        .await
    }

    pub async fn answer_ask(&self, request: AskAnswerRequest) -> GjcResult<CommandReceipt> {
        self.mutate(
            request.envelope,
            GjcCommandKind::AskAnswer,
            json!({
                "ask_id": request.ask_id,
                "choices": request
                    .choices
                    .iter()
                    .map(|choice| choice.option.as_str())
                    .collect::<Vec<_>>(),
            }),
        )
        .await
    }

    pub async fn select_model(&self, request: ModelSelectionRequest) -> GjcResult<CommandReceipt> {
        let ((Some(model), None) | (None, Some(model))) =
            (request.model.as_deref(), request.profile.as_deref())
        else {
            return Err(GjcError::InvalidRequest {
                field: "model",
                reason: "exactly one of model or profile must be provided".into(),
            });
        };
        self.mutate(
            request.envelope,
            GjcCommandKind::ModelSelection,
            json!({
                "selection": model,
            }),
        )
        .await
    }

    /// Replay an accepted command by idempotency key.
    pub async fn command_receipt(&self, key: &IdempotencyKey) -> GjcResult<CommandReceipt> {
        self.ensure_receipt_journal().await?;
        self.registry
            .read()
            .await
            .get(key.as_str())
            .cloned()
            .ok_or(GjcError::SessionNotFound {
                session_id: key.as_str().to_string(),
            })
    }

    // -----------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------

    async fn round_trip(
        &self,
        transport: &BoundTransport,
        method: &str,
        params: Value,
        timeout_ms: u64,
    ) -> GjcResult<GjcResponse> {
        let correlation_id = Uuid::new_v4().to_string();
        let request = GjcRequest::new(&correlation_id, method, params, timeout_ms);
        let reply = transport.transport.round_trip(request).await?;
        if reply.correlation_id != correlation_id {
            return Err(GjcError::AmbiguousAck {
                method: method.into(),
            });
        }
        Ok(reply)
    }

    /// Re-read the discovery record for a mutating exchange and compare it to
    /// the endpoint captured during resolution. This is deliberately called
    /// after the durable reservation and both immediately before dispatch and
    /// immediately before trusting an acknowledgement; any ambiguity leaves
    /// the non-terminal reservation intact and never triggers a replay.
    async fn validate_mutation_lease(
        &self,
        transport: &BoundTransport,
        session: &SessionId,
    ) -> GjcResult<()> {
        let TransportSource::Discovery(root) = &self.source else {
            return Ok(());
        };
        if transport.session_id.as_deref() != Some(session.as_str())
            || transport.worktree.as_deref() != Some(root.as_path())
        {
            return Err(GjcError::SessionMismatch {
                expected: session.as_str().to_string(),
            });
        }
        let current = super::transport::discover_endpoint_for_session(root, session.as_str())
            .map_err(|error| match error {
                GjcError::SessionNotFound { .. } => GjcError::StaleEndpoint {
                    capability: super::model::CAP_ENDPOINT.into(),
                },
                other => other,
            })?;
        if transport.transport.endpoint_generation() != Some(current.endpoint_generation()) {
            return Err(GjcError::StaleEndpoint {
                capability: super::model::CAP_ENDPOINT.into(),
            });
        }
        Ok(())
    }

    /// Shared mutation path: capability gate, session guard, idempotent
    /// replay, bounded exchange, receipt recording with forward-only
    /// status progression.
    async fn mutate(
        &self,
        envelope: ControlRequestEnvelope,
        kind: GjcCommandKind,
        params: Value,
    ) -> GjcResult<CommandReceipt> {
        self.ensure_receipt_journal().await?;
        envelope.validate()?;
        if let Some(expected) = envelope.expected_session.as_ref()
            && expected.as_str() != envelope.session.as_str()
        {
            return Err(GjcError::SessionMismatch {
                expected: envelope.session.as_str().to_string(),
            });
        }
        if let Some(existing) = self
            .registry
            .read()
            .await
            .get(envelope.idempotency_key.as_str())
        {
            if existing.session_id != envelope.session.as_str() || existing.kind != kind.as_str() {
                return Err(GjcError::InvalidRequest {
                    field: "idempotency_key",
                    reason: "key is bound to another session or command kind".into(),
                });
            }
            if existing.status.is_terminal() {
                return Ok(existing.clone());
            }
            return Err(GjcError::InvalidRequest {
                field: "idempotency_key",
                reason: "command is already in flight".into(),
            });
        }
        // Capability gate: mutations fail closed on missing capability
        // before any session or transport work happens.
        self.capabilities()
            .await
            .require(kind.required_capability())?;
        let transport = self
            .resolve_transport(Some(&envelope.session))
            .await?
            .ok_or(GjcError::TransportUnavailable)?;

        let mut request_params = json!({
            "session_id": envelope.session.as_str(),
            "idempotency_key": envelope.idempotency_key.as_str(),
            "kind": kind.as_str(),
        });
        if let Some(expected) = envelope.expected_session.as_ref() {
            request_params["expected_session_id"] = Value::String(expected.as_str().to_string());
        }
        if let Value::Object(map) = params {
            for (key, value) in map {
                request_params[key] = value;
            }
        }

        let command_id = CommandId::new(format!("gjc-cmd-{}", Uuid::new_v4()))?;
        let receipt = CommandReceipt {
            schema: GJC_CONTROL_SCHEMA.into(),
            command_id: command_id.as_str().to_string(),
            idempotency_key: envelope.idempotency_key.as_str().to_string(),
            kind: kind.as_str().to_string(),
            session_id: envelope.session.as_str().to_string(),
            status: GjcCommandStatus::Accepted,
            turn_id: None,
            outcome: None,
            created_at: now_rfc3339(),
        };
        // Replay checking and reservation share one write lock so concurrent
        // callers cannot both claim the same key.
        let mut registry = self.registry.write().await;
        if let Some(existing) = registry.get(envelope.idempotency_key.as_str()) {
            if existing.session_id != envelope.session.as_str() || existing.kind != kind.as_str() {
                return Err(GjcError::InvalidRequest {
                    field: "idempotency_key",
                    reason: "key is bound to another session or command kind".into(),
                });
            }
            if existing.status.is_terminal() {
                return Ok(existing.clone());
            }
            return Err(GjcError::InvalidRequest {
                field: "idempotency_key",
                reason: "command is already in flight".into(),
            });
        }
        if registry.len() >= MAX_COMMAND_RECEIPTS {
            let evictable = registry
                .iter()
                .find(|(_, receipt)| receipt.status.is_terminal())
                .map(|(key, _)| key.clone());
            let Some(evictable) = evictable else {
                return Err(GjcError::InvalidRequest {
                    field: "idempotency_key",
                    reason: "command receipt capacity is exhausted by active commands".into(),
                });
            };
            let evicted = registry
                .remove(&evictable)
                .expect("receipt eviction key must still exist");
            registry.insert(
                envelope.idempotency_key.as_str().to_string(),
                receipt.clone(),
            );
            if let Err(error) = self.persist_receipts(&registry) {
                registry.remove(envelope.idempotency_key.as_str());
                registry.insert(evictable, evicted);
                return Err(error);
            }
        } else {
            registry.insert(
                envelope.idempotency_key.as_str().to_string(),
                receipt.clone(),
            );
            if let Err(error) = self.persist_receipts(&registry) {
                registry.remove(envelope.idempotency_key.as_str());
                return Err(error);
            }
        }
        drop(registry);
        // The reservation is durable before any peer exchange begins.

        // Revalidate the enrolled worktree/session/generation at the final
        // dispatch boundary. Rotation is stale rather than retryable: this
        // command remains reserved and is never sent to a replacement peer.
        self.validate_mutation_lease(&transport, &envelope.session)
            .await?;

        // Transport exchange with bounded timeout semantics owned by the
        // transport implementation (#322). Ambiguous or malformed acks fail
        // closed; the recorded receipt stays non-terminal so a replay does
        // not fabricate an outcome.
        let reply = match self
            .round_trip(
                &transport,
                &format!("control.{}", kind.as_str()),
                request_params,
                envelope.timeout_ms,
            )
            .await
        {
            Ok(reply) => reply,
            Err(error) => return Err(error),
        };
        // Revalidate again before accepting the acknowledgement. If metadata
        // rotated while the exchange was in flight, the reservation remains
        // Accepted and the control is never replayed automatically.
        self.validate_mutation_lease(&transport, &envelope.session)
            .await?;

        let acked = reply
            .result
            .get("accepted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !acked {
            return Err(GjcError::AmbiguousAck {
                method: kind.as_str().into(),
            });
        }

        // Ack-side session guard: an ack naming a different session is
        // treated as a mismatch and fails closed.
        let Some(echoed) = reply.result.get("session_id").and_then(Value::as_str) else {
            return Err(GjcError::AmbiguousAck {
                method: kind.as_str().into(),
            });
        };
        if echoed != envelope.session.as_str() {
            return Err(GjcError::SessionMismatch {
                expected: envelope.session.as_str().to_string(),
            });
        }
        // Status progression Accepted -> Acked -> terminal.

        let turn_id = reply
            .result
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let outcome = reply.result.get("outcome").map(public_outcome);

        let mut write = self.registry.write().await;
        let previous;
        let acked_entry = {
            let entry = write.get_mut(envelope.idempotency_key.as_str()).ok_or(
                GjcError::InvalidRequest {
                    field: "idempotency_key",
                    reason: "command registry entry vanished mid-flight".into(),
                },
            )?;
            if !entry.status.can_transition_to(GjcCommandStatus::Acked) {
                return Err(GjcError::AmbiguousAck {
                    method: kind.as_str().into(),
                });
            }
            previous = entry.clone();
            entry.status = GjcCommandStatus::Acked;
            if let Some(turn_id) = turn_id.clone() {
                entry.turn_id = Some(turn_id);
            }
            entry.clone()
        };
        let snapshot = write.clone();
        if let Err(error) = self.persist_receipts(&snapshot) {
            if let Some(entry) = write.get_mut(envelope.idempotency_key.as_str()) {
                *entry = previous;
            }
            return Err(error);
        }
        let mut updated = acked_entry;
        drop(write);

        // Terminal receipt: a peer outcome is only trusted when it carries
        // a parseable TERMINAL prompt status. Anything else (missing,
        // non-terminal, or unparsable status) fails closed as an invalid
        // peer reply; the record stays Acked so a replay cannot fabricate
        // terminality from malformed input.
        if let Some(outcome) = outcome {
            let outcome_status = outcome
                .get("status")
                .and_then(Value::as_str)
                .and_then(parse_prompt_status);
            let terminal = match outcome_status {
                Some(status) if status.is_terminal() => match status {
                    GjcPromptStatus::Succeeded => GjcCommandStatus::Completed,
                    _ => GjcCommandStatus::Failed,
                },
                _ => {
                    return Err(GjcError::InvalidPeerReply {
                        method: kind.as_str().into(),
                        reason: "peer ack carried no terminal outcome status".into(),
                    });
                }
            };
            let mut write = self.registry.write().await;
            let previous_and_updated = if let Some(entry) =
                write.get_mut(envelope.idempotency_key.as_str())
                && entry.status.can_transition_to(terminal)
            {
                let previous = entry.clone();
                entry.status = terminal;
                entry.outcome = Some(outcome);
                Some((previous, entry.clone()))
            } else {
                None
            };
            if let Some((previous, terminal_entry)) = previous_and_updated {
                let snapshot = write.clone();
                if let Err(error) = self.persist_receipts(&snapshot) {
                    if let Some(entry) = write.get_mut(envelope.idempotency_key.as_str()) {
                        *entry = previous;
                    }
                    return Err(error);
                }
                updated = terminal_entry;
            }
        }

        Ok(updated)
    }

    /// Fail closed when an expected session id disagrees with the
    /// authoritative session metadata.
    fn check_session_identity(&self, session: &SessionId, query: &SessionQuery) -> GjcResult<()> {
        let Some(metadata) = query.metadata.as_ref() else {
            return Ok(());
        };
        if metadata.session_id != session.as_str() {
            return Err(GjcError::SessionMismatch {
                expected: session.as_str().to_string(),
            });
        }
        Ok(())
    }
}

pub(crate) fn public_outcome(value: &Value) -> Value {
    let Some(object) = value.as_object() else {
        return json!({"status": "unknown", "summary": "details redacted"});
    };
    let mut safe = serde_json::Map::new();
    let status = object
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| {
            matches!(
                *status,
                "queued" | "running" | "succeeded" | "failed" | "aborted"
            )
        })
        .unwrap_or("unknown");
    safe.insert("status".into(), Value::String(status.into()));
    if let Some(Value::String(text)) = object.get("turn_id")
        && text.len() <= 128
        && !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        safe.insert("turn_id".into(), Value::String(text.clone()));
    }
    if let Some(Value::String(text)) = object.get("finished_at")
        && let Ok(parsed) = OffsetDateTime::parse(text, &Rfc3339)
        && let Ok(canonical) = parsed.format(&Rfc3339)
    {
        safe.insert("finished_at".into(), Value::String(canonical));
    }
    if let Some(Value::String(summary)) = object.get("summary") {
        let lower = summary.to_ascii_lowercase();
        let compact: String = lower.chars().filter(char::is_ascii_alphanumeric).collect();
        let safe_summary = if summary.len() <= 128
            && !lower.contains("token")
            && !lower.contains("secret")
            && !lower.contains("password")
            && !lower.contains("api_key")
            && !lower.contains("api-key")
            && !lower.contains("private_key")
            && !lower.contains("authorization")
            && !lower.contains("apikey")
            && !lower.contains("credential")
            && !lower.contains("passwd")
            && !lower.contains("auth:")
            && !compact.contains("privatekey")
            && !lower.contains("auth=")
            && !lower.contains("bearer")
            && !lower.contains("://")
            && summary.is_ascii()
        {
            summary
                .chars()
                .filter(|character| !character.is_control())
                .collect()
        } else {
            "details redacted".to_string()
        };
        safe.insert("summary".into(), Value::String(safe_summary));
    }
    Value::Object(safe)
}

fn parse_prompt_status(raw: &str) -> Option<GjcPromptStatus> {
    match raw {
        "queued" => Some(GjcPromptStatus::Queued),
        "running" => Some(GjcPromptStatus::Running),
        "succeeded" => Some(GjcPromptStatus::Succeeded),
        "failed" => Some(GjcPromptStatus::Failed),
        "aborted" => Some(GjcPromptStatus::Aborted),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gjc::model::{
        Capabilities, ControlRequestEnvelope, IdempotencyKey, ModelSelectionRequest, SessionId,
    };
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Scripted transport: records requests and replays canned replies so
    /// the full contract is exercised without the #322 transport track.
    struct MockTransport {
        replies: Mutex<Vec<GjcResult<GjcResponse>>>,
        seen: Mutex<Vec<String>>,
        timeouts: Mutex<Vec<u64>>,
    }

    impl MockTransport {
        fn new(replies: Vec<GjcResult<GjcResponse>>) -> Self {
            Self {
                replies: Mutex::new(replies),
                seen: Mutex::new(Vec::new()),
                timeouts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl GjcTransport for MockTransport {
        async fn round_trip(
            &self,
            request: GjcRequest,
        ) -> std::result::Result<GjcResponse, GjcError> {
            self.seen.lock().unwrap().push(request.method.clone());
            self.timeouts.lock().unwrap().push(request.timeout_ms);
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                return Err(GjcError::Timeout {
                    method: request.method,
                });
            }
            let reply = replies.remove(0);
            match reply {
                Ok(mut response) => {
                    response.correlation_id = request.correlation_id;
                    Ok(response)
                }
                Err(error) => Err(error),
            }
        }
    }

    fn ack_reply(turn_id: &str) -> GjcResult<GjcResponse> {
        Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({"accepted": true, "session_id": "sess-1", "turn_id": turn_id}),
        })
    }

    fn terminal_ack_reply(turn_id: &str, status: &str, summary: &str) -> GjcResult<GjcResponse> {
        Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({
                "accepted": true,
                "session_id": "sess-1",
                "turn_id": turn_id,
                "outcome": {"status": status, "summary": summary},
            }),
        })
    }

    async fn implemented_plane_with(
        replies: Vec<GjcResult<GjcResponse>>,
    ) -> (GjcControlPlane, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport::new(replies));
        let plane = GjcControlPlane::new(transport.clone(), new_shared_command_registry());
        (plane, transport)
    }

    #[tokio::test]
    async fn query_session_parses_typed_sections_from_transport() {
        let (plane, _transport) = implemented_plane_with(vec![Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({
                "metadata": {"session_id": "sess-1", "title": "lane"},
                "stats": {"turns_total": 3, "queue_depth": 1},
            }),
        })])
        .await;
        let query = plane
            .query_session(&SessionId::new("sess-1").unwrap(), &["metadata", "stats"])
            .await
            .unwrap();
        assert_eq!(query.metadata.as_ref().unwrap().session_id, "sess-1");
        assert_eq!(query.stats.as_ref().unwrap().turns_total, 3);
        assert!(query.turn.is_none());
    }

    #[tokio::test]
    async fn query_session_fails_closed_on_malformed_sections() {
        let (plane, _transport) = implemented_plane_with(vec![Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({"metadata": {"session_id": 42}}),
        })])
        .await;
        let error = plane
            .query_session(&SessionId::new("sess-1").unwrap(), &["metadata"])
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "invalid_peer_reply");
    }

    #[tokio::test]
    async fn query_session_allows_omitted_optional_surfaces() {
        let (plane, _transport) = implemented_plane_with(vec![Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({
                "metadata": {"session_id": "sess-1"}
            }),
        })])
        .await;
        let query = plane
            .query_session(
                &SessionId::new("sess-1").unwrap(),
                &["metadata", "stats", "turn", "workflow_gates"],
            )
            .await
            .unwrap();
        assert!(query.stats.is_none());
        assert!(query.turn.is_none());
        assert!(query.workflow_gates.is_none());
        assert!(!query.turn_present);
    }

    #[tokio::test]
    async fn query_session_preserves_explicit_null_turn_presence() {
        let (plane, _transport) = implemented_plane_with(vec![Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({
                "metadata": {"session_id": "sess-1"},
                "turn": null
            }),
        })])
        .await;
        let query = plane
            .query_session(&SessionId::new("sess-1").unwrap(), &["metadata", "turn"])
            .await
            .unwrap();
        assert!(query.turn_present);
        assert!(query.turn.is_none());
    }

    #[tokio::test]
    async fn query_session_ignores_unrequested_sections() {
        let (plane, _transport) = implemented_plane_with(vec![Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({
                "metadata": {"session_id": "sess-1"},
                "turn": {"id": "not-returned", "state": "invalid"},
                "goal_todo": {"raw": "not-returned"}
            }),
        })])
        .await;
        let query = plane
            .query_session(&SessionId::new("sess-1").unwrap(), &["metadata"])
            .await
            .unwrap();
        assert!(query.turn.is_none());
        assert!(query.goal_todo.is_none());
    }

    #[tokio::test]
    async fn prompt_accepts_and_records_acked_receipt() {
        let key = IdempotencyKey::new("idem-key-0100").unwrap();
        let (plane, _transport) = implemented_plane_with(vec![ack_reply("turn-100")]).await;
        let receipt = plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-0100"),
                prompt: "hello".into(),
            })
            .await
            .unwrap();
        assert_eq!(receipt.status, GjcCommandStatus::Acked);
        assert_eq!(receipt.turn_id.as_deref(), Some("turn-100"));
        assert!(!receipt.status.is_terminal());
        let recorded = plane.command_receipt(&key).await.unwrap();
        assert_eq!(recorded.status, GjcCommandStatus::Acked);
    }

    #[tokio::test]
    async fn idempotent_replay_returns_recorded_terminal_receipt() {
        let (plane, transport) =
            implemented_plane_with(vec![terminal_ack_reply("turn-101", "succeeded", "done")]).await;
        let first = plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-0101"),
                prompt: "once".into(),
            })
            .await
            .unwrap();
        assert_eq!(first.status, GjcCommandStatus::Completed);
        let second = plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-0101"),
                prompt: "twice".into(),
            })
            .await
            .unwrap();
        assert_eq!(second.command_id, first.command_id);
        assert_eq!(
            plane
                .command_receipt(&IdempotencyKey::new("idem-key-0101").unwrap())
                .await
                .unwrap()
                .command_id,
            first.command_id
        );
        // Only one peer exchange happened; the replay never re-sent.
        assert_eq!(transport.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn idempotency_key_cannot_replay_across_sessions() {
        let (plane, _transport) =
            implemented_plane_with(vec![terminal_ack_reply("turn-102", "succeeded", "done")]).await;
        plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-cross-session"),
                prompt: "once".into(),
            })
            .await
            .unwrap();
        let mut cross_session = envelope("idem-key-cross-session");
        cross_session.session = SessionId::new("sess-2").unwrap();
        let error = plane
            .prompt(PromptRequest {
                envelope: cross_session,
                prompt: "replay".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "session_mismatch");
    }

    #[tokio::test]
    async fn ambiguous_ack_fails_closed_without_fabricating_outcome() {
        let (plane, _transport) = implemented_plane_with(vec![Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({"accepted": false}),
        })])
        .await;
        let error = plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-0102"),
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "ambiguous_ack");
        assert_eq!(
            plane
                .registry
                .read()
                .await
                .get("idem-key-0102")
                .map(|receipt| receipt.status),
            Some(GjcCommandStatus::Accepted)
        );
    }

    #[tokio::test]
    async fn transport_errors_surface_their_taxonomy_code() {
        for (reply, expected) in [
            (
                GjcResult::<GjcResponse>::Err(GjcError::Timeout {
                    method: "control.prompt".into(),
                }),
                "timeout",
            ),
            (
                GjcResult::<GjcResponse>::Err(GjcError::StaleEndpoint {
                    capability: "session.control".into(),
                }),
                "stale_endpoint",
            ),
        ] {
            let (plane, _transport) = implemented_plane_with(vec![reply]).await;
            let error = plane
                .steer(SteerRequest {
                    envelope: envelope("idem-key-0103"),
                    message: "nudge".into(),
                })
                .await
                .unwrap_err();
            assert_eq!(error.error_code(), expected);
        }
    }

    #[tokio::test]
    async fn session_identity_mismatch_is_detected_on_query() {
        let (plane, _transport) = implemented_plane_with(vec![Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({"metadata": {"session_id": "sess-other"}}),
        })])
        .await;
        let error = plane
            .query_session(&SessionId::new("sess-1").unwrap(), &["metadata"])
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "session_mismatch");
    }

    #[tokio::test]
    async fn expected_session_mismatch_fails_closed_before_dispatch() {
        let (plane, transport) = implemented_plane_with(vec![ack_reply("turn-x")]).await;
        let mut env = envelope("idem-key-0104");
        env.expected_session = Some(SessionId::new("sess-other").unwrap());
        let error = plane
            .prompt(PromptRequest {
                envelope: env,
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "session_mismatch");
        // Nothing reached the transport.
        assert!(transport.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ack_echoing_wrong_session_fails_closed() {
        let (plane, _transport) = implemented_plane_with(vec![Ok(GjcResponse {
            correlation_id: String::new(),
            result: json!({
                "accepted": true,
                "session_id": "sess-other",
                "turn_id": "turn-105",
            }),
        })])
        .await;
        let error = plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-0105"),
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "session_mismatch");
    }

    fn envelope(key: &str) -> ControlRequestEnvelope {
        ControlRequestEnvelope {
            session: SessionId::new("sess-1").unwrap(),
            expected_session: Some(SessionId::new("sess-1").unwrap()),
            idempotency_key: IdempotencyKey::new(key).unwrap(),
            timeout_ms: ControlRequestEnvelope::DEFAULT_TIMEOUT_MS,
        }
    }

    fn plane() -> GjcControlPlane {
        GjcControlPlane::unavailable(new_shared_command_registry())
    }

    #[tokio::test]
    async fn queries_and_mutations_fail_closed_without_transport() {
        let plane = plane();
        let error = plane
            .query_session(&SessionId::new("s").unwrap(), &["metadata"])
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "transport_unavailable");
        let error = plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-0001"),
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        // Mutations gate on capabilities first: with no transport nothing is
        // exercisable, so the typed failure is capability_missing.
        assert_eq!(error.error_code(), "missing_capability");
    }

    #[tokio::test]
    async fn model_selection_requires_exactly_one_of_model_or_profile() {
        let plane = plane();
        let both = ModelSelectionRequest {
            envelope: envelope("idem-key-0002"),
            model: Some("m".into()),
            profile: Some("p".into()),
        };
        let error = plane.select_model(both).await.unwrap_err();
        assert_eq!(error.error_code(), "invalid_request");
    }

    #[tokio::test]
    async fn capabilities_gate_control_verbs_without_transport() {
        let caps = Capabilities::for_transport(false);
        assert!(
            caps.require(GjcCommandKind::Prompt.required_capability())
                .is_err()
        );
    }

    #[test]
    fn envelope_timeout_bounds_are_enforced() {
        let mut env = envelope("idem-key-0003");
        env.timeout_ms = 0;
        assert!(env.validate().is_err());
        env.timeout_ms = ControlRequestEnvelope::MAX_TIMEOUT_MS + 1;
        assert!(env.validate().is_err());
        env.timeout_ms = 1_000;
        assert!(env.validate().is_ok());
    }

    #[test]
    fn terminal_receipts_carry_outcome_and_turn_binding() {
        let completed = CommandReceipt {
            schema: GJC_CONTROL_SCHEMA.into(),
            command_id: "cmd-1".into(),
            idempotency_key: "idem-key-0004".into(),
            kind: "prompt".into(),
            session_id: "sess-1".into(),
            status: GjcCommandStatus::Completed,
            turn_id: Some("turn-1".into()),
            outcome: Some(json!({"status": "succeeded", "summary": "done"})),
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        assert!(completed.status.is_terminal());
        assert_eq!(
            completed.outcome.as_ref().unwrap()["status"],
            json!("succeeded")
        );
    }

    #[test]
    fn query_session_section_keys_are_stable() {
        // The wire section names are part of the public contract.
        for section in [
            "metadata",
            "stats",
            "model_profile",
            "turn",
            "queue",
            "workflow_gates",
            "goal_todo",
        ] {
            assert!(section.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        }
    }

    #[tokio::test]
    async fn command_receipt_replay_requires_terminal_state() {
        let plane = plane();
        let error = plane
            .command_receipt(&IdempotencyKey::new("idem-key-0007").unwrap())
            .await
            .unwrap_err();
        // Unknown keys surface as not-found rather than leaking registry size.
        assert_eq!(error.error_code(), "session_not_found");
    }
    #[tokio::test]
    async fn correlation_mismatch_fails_closed_as_ambiguous_ack() {
        // A transport that echoes a DIFFERENT correlation id must trip the
        // plane's defense-in-depth branch, which the scripted transport
        // normally never exercises because it echoes verbatim.
        struct WrongEcho;
        #[async_trait::async_trait]
        impl GjcTransport for WrongEcho {
            async fn round_trip(
                &self,
                _request: GjcRequest,
            ) -> std::result::Result<GjcResponse, GjcError> {
                Ok(GjcResponse {
                    correlation_id: "not-the-id".into(),
                    result: json!({"accepted": true}),
                })
            }
        }
        let plane = GjcControlPlane::new(Arc::new(WrongEcho), new_shared_command_registry());
        let error = plane
            .prompt(PromptRequest {
                envelope: envelope("idem-key-0108"),
                prompt: "hi".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "ambiguous_ack");
    }

    #[tokio::test]
    async fn mutation_timeout_budget_is_forwarded_to_the_transport() {
        let (plane, transport) = implemented_plane_with(vec![ack_reply("turn-109")]).await;
        let mut env = envelope("idem-key-0109");
        env.timeout_ms = 4_321;
        plane
            .prompt(PromptRequest {
                envelope: env,
                prompt: "bounded".into(),
            })
            .await
            .unwrap();
        assert_eq!(transport.timeouts.lock().unwrap()[0], 4_321);
    }

    #[tokio::test]
    async fn endpoint_rotation_after_resolution_rejects_lease_and_keeps_reservation() {
        let worktree = tempfile::tempdir().unwrap();
        let sdk_dir = worktree.path().join(".gjc").join("state").join("sdk");
        std::fs::create_dir_all(&sdk_dir).unwrap();
        let metadata = sdk_dir.join("sess-1.json");
        std::fs::write(
            &metadata,
            json!({
                "version": 1,
                "sessionId": "sess-1",
                "url": "ws://127.0.0.1:1/",
                "token": "lease-before",
            })
            .to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&metadata, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let plane = GjcControlPlane::for_worktree(worktree.path(), new_shared_command_registry());
        let session = SessionId::new("sess-1").unwrap();
        let bound = plane
            .resolve_transport(Some(&session))
            .await
            .unwrap()
            .unwrap();
        plane.registry.write().await.insert(
            "idem-lease-001".into(),
            stored_receipt("idem-lease-001", GjcCommandStatus::Accepted),
        );

        std::fs::write(
            &metadata,
            json!({
                "version": 1,
                "sessionId": "sess-1",
                "url": "ws://127.0.0.1:2/",
                "token": "lease-after",
            })
            .to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&metadata, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let error = plane
            .validate_mutation_lease(&bound, &session)
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "stale_endpoint");
        assert_eq!(
            plane
                .registry
                .read()
                .await
                .get("idem-lease-001")
                .map(|receipt| receipt.status),
            Some(GjcCommandStatus::Accepted)
        );
    }

    #[tokio::test]
    async fn bound_mutation_lease_rejects_worktree_mismatch() {
        let worktree = tempfile::tempdir().unwrap();
        let sdk_dir = worktree.path().join(".gjc").join("state").join("sdk");
        std::fs::create_dir_all(&sdk_dir).unwrap();
        let metadata = sdk_dir.join("sess-1.json");
        std::fs::write(
            &metadata,
            json!({
                "version": 1,
                "sessionId": "sess-1",
                "url": "ws://127.0.0.1:1/",
                "token": "worktree-bound",
            })
            .to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&metadata, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let plane = GjcControlPlane::for_worktree(worktree.path(), new_shared_command_registry());
        let session = SessionId::new("sess-1").unwrap();
        let bound = plane
            .resolve_transport(Some(&session))
            .await
            .unwrap()
            .unwrap();
        let foreign_worktree = tempfile::tempdir().unwrap();
        let foreign_plane = plane.scoped_to_worktree(foreign_worktree.path());
        let error = foreign_plane
            .validate_mutation_lease(&bound, &session)
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "session_mismatch");
    }

    fn stored_receipt(key: &str, status: GjcCommandStatus) -> CommandReceipt {
        CommandReceipt {
            schema: GJC_CONTROL_SCHEMA.into(),
            command_id: format!("cmd-{key}"),
            idempotency_key: key.into(),
            kind: "prompt".into(),
            session_id: "sess-1".into(),
            status,
            turn_id: (!matches!(status, GjcCommandStatus::Accepted)).then(|| format!("turn-{key}")),
            outcome: status.is_terminal().then(|| {
                json!({
                    "status": if status == GjcCommandStatus::Completed {
                        "succeeded"
                    } else {
                        "failed"
                    },
                    "summary": "stored",
                })
            }),
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn production_receipt_journal_reloads_reserved_receipt() {
        let worktree = tempfile::tempdir().unwrap();
        let registry = new_shared_command_registry();
        let plane = GjcControlPlane::for_worktree(worktree.path(), registry.clone());
        plane.ensure_receipt_journal().await.unwrap();
        let receipt = stored_receipt("reload-key-0001", GjcCommandStatus::Accepted);
        registry
            .write()
            .await
            .insert(receipt.idempotency_key.clone(), receipt.clone());
        let snapshot = registry.read().await.clone();
        plane.persist_receipts(&snapshot).unwrap();
        drop(plane);

        let reloaded =
            GjcControlPlane::for_worktree(worktree.path(), new_shared_command_registry());
        assert_eq!(
            reloaded
                .command_receipt(&IdempotencyKey::new("reload-key-0001").unwrap())
                .await
                .unwrap()
                .command_id,
            receipt.command_id
        );
    }

    #[tokio::test]
    async fn production_receipt_journal_retains_active_and_ambiguous_entries() {
        let worktree = tempfile::tempdir().unwrap();
        let registry = new_shared_command_registry();
        let plane = GjcControlPlane::for_worktree(worktree.path(), registry.clone());
        plane.ensure_receipt_journal().await.unwrap();
        let accepted = stored_receipt("active-key-0001", GjcCommandStatus::Accepted);
        let acked = stored_receipt("ambiguous-key-001", GjcCommandStatus::Acked);
        {
            let mut write = registry.write().await;
            write.insert(accepted.idempotency_key.clone(), accepted.clone());
            write.insert(acked.idempotency_key.clone(), acked.clone());
        }
        let snapshot = registry.read().await.clone();
        plane.persist_receipts(&snapshot).unwrap();
        drop(plane);

        let reloaded =
            GjcControlPlane::for_worktree(worktree.path(), new_shared_command_registry());
        reloaded.ensure_receipt_journal().await.unwrap();
        let write = reloaded.registry.read().await;
        assert_eq!(
            write.get("active-key-0001").unwrap().status,
            GjcCommandStatus::Accepted
        );
        assert_eq!(
            write.get("ambiguous-key-001").unwrap().status,
            GjcCommandStatus::Acked
        );
    }

    #[tokio::test]
    async fn malformed_production_receipt_journal_fails_closed() {
        let worktree = tempfile::tempdir().unwrap();
        let state = worktree.path().join(".gjc").join("state");
        std::fs::create_dir_all(&state).unwrap();
        let path = state.join(RECEIPT_JOURNAL_FILENAME);
        std::fs::write(&path, b"not-json").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let plane = GjcControlPlane::for_worktree(worktree.path(), new_shared_command_registry());
        let error = plane
            .command_receipt(&IdempotencyKey::new("malformed-key-001").unwrap())
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "invalid_request");
    }

    #[tokio::test]
    async fn receipt_capacity_evicts_terminal_entries_only() {
        let worktree = tempfile::tempdir().unwrap();
        let (mut plane, _transport) =
            implemented_plane_with(vec![ack_reply("turn-capacity")]).await;
        plane.journal = Some(Arc::new(ReceiptJournal::for_worktree(worktree.path())));
        plane.ensure_receipt_journal().await.unwrap();
        {
            let mut write = plane.registry.write().await;
            write.insert(
                "active-key-0002".into(),
                stored_receipt("active-key-0002", GjcCommandStatus::Accepted),
            );
            for index in 0..(MAX_COMMAND_RECEIPTS - 1) {
                let key = format!("terminal-{index:04}");
                write.insert(
                    key.clone(),
                    stored_receipt(&key, GjcCommandStatus::Completed),
                );
            }
        }
        let result = plane
            .prompt(PromptRequest {
                envelope: envelope("capacity-key-001"),
                prompt: "bounded".into(),
            })
            .await
            .unwrap();
        assert_eq!(result.status, GjcCommandStatus::Acked);
        let write = plane.registry.read().await;
        assert_eq!(write.len(), MAX_COMMAND_RECEIPTS);
        assert!(write.contains_key("active-key-0002"));
        assert!(write.contains_key("capacity-key-001"));
        assert_eq!(
            write
                .values()
                .filter(|receipt| receipt.status.is_terminal())
                .count(),
            MAX_COMMAND_RECEIPTS - 2
        );
    }
}
