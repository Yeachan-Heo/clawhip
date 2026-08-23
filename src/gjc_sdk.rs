//! Safe worktree-local GJC SDK endpoint discovery and typed websocket transport.
//!
//! This module owns the reusable GJC SDK v3 transport layer (#322, repaired by
//! #328):
//!
//! - [`discover`] reads `<worktree>/.gjc/state/sdk/<session-id>.json` session
//!   endpoint metadata for one registered lane/worktree, binds identity to the
//!   endpoint filename, honors supported `version`/`stale` record semantics,
//!   never scans unrelated roots, and never exposes tokens through errors or
//!   debug output.
//! - [`SdkClient`] speaks the installed SDK v3 wire contract: one authenticated
//!   (`?token=` query parameter), hello-gated persistent websocket connection
//!   per client, strict `hello` type/identity validation, typed
//!   [`SdkRequest::query`] / [`SdkRequest::control`] / [`SdkRequest::broker`]
//!   frames with object inputs and their matching `*_response` envelopes,
//!   bounded timeouts, bounded frame/output sizes, a typed error taxonomy, and
//!   bounded reconnect with re-authentication.
//!
//! The trust boundary is strictly local: only `ws://` loopback endpoints read
//! from worktree-local metadata under the same user are accepted, and the
//! metadata file itself must be a regular file (no symlinks) owned-readable by
//! the current user with conservative permissions.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;

use crate::Result;

/// Maximum accepted size of a session endpoint metadata file.
const MAX_METADATA_BYTES: u64 = 4096;
/// Maximum accepted length of the token read from metadata.
const MAX_TOKEN_CHARS: usize = 256;
/// Maximum accepted length of the URL read from metadata.
const MAX_URL_CHARS: usize = 512;
/// Maximum accepted session-id length (UUID-shaped strings).
const MAX_SESSION_ID_CHARS: usize = 64;
/// Ceiling applied to every inbound frame regardless of configuration.
const ABSOLUTE_MAX_FRAME_BYTES: usize = 262_144;
/// Endpoint record schema version supported by this transport.
///
/// The installed GJC host publishes `{"version":1, ...}` endpoint records;
/// anything else fails discovery closed.
const SUPPORTED_ENDPOINT_RECORD_VERSION: u32 = 1;
/// Maximum accepted hello `connectionId` length.
const MAX_CONNECTION_ID_CHARS: usize = 128;
/// Maximum accepted query/operation name length.
const MAX_OPERATION_NAME_CHARS: usize = 128;
/// Non-mutating, always-installed query used by the diagnostic probe.
const PROBE_QUERY: &str = "session.metadata";

/// Typed, public-safe transport error taxonomy.
///
/// Variants never embed endpoint URLs, tokens, or raw frames; diagnostics are
/// stable reason strings safe to surface in clawhip events and status output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdkTransportError {
    /// No endpoint metadata exists under the lane's state root yet.
    EndpointUnavailable,
    /// Endpoint metadata exists but failed validation (schema, url, path).
    EndpointMalformed,
    /// The websocket handshake or authentication was rejected.
    EndpointUnauthorized,
    /// The post-connect `hello` frame violated the v3 contract.
    InvalidHello,
    /// The operation exceeded its bounded timeout.
    Timeout,
    /// The server closed or errored the connection mid-exchange.
    ConnectionClosed,
    /// A frame violated size or JSON-shape bounds.
    FrameRejected,
    /// The response envelope did not match the sent correlation ID.
    CorrelationMismatch,
    /// Bounded reconnect attempts were exhausted.
    RetryExhausted,
}

impl SdkTransportError {
    /// Stable, public-safe reason string for events and diagnostics.
    pub fn reason(self) -> &'static str {
        match self {
            Self::EndpointUnavailable => "endpoint_unavailable",
            Self::EndpointMalformed => "endpoint_malformed",
            Self::EndpointUnauthorized => "endpoint_unauthorized",
            Self::InvalidHello => "invalid_hello",
            Self::Timeout => "timeout",
            Self::ConnectionClosed => "connection_closed",
            Self::FrameRejected => "frame_rejected",
            Self::CorrelationMismatch => "correlation_mismatch",
            Self::RetryExhausted => "retry_exhausted",
        }
    }
}

impl std::fmt::Display for SdkTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

impl std::error::Error for SdkTransportError {}

/// A validated, token-bearing endpoint descriptor for one SDK session.
///
/// The token never implements `Debug`-visible exposure: [`std::fmt::Debug`] is
/// hand-written to redact it.
#[derive(Clone, PartialEq, Eq)]
pub struct EndpointMetadata {
    session_id: String,
    url: String,
    token: String,
    pid: Option<u32>,
    stale: bool,
}

impl std::fmt::Debug for EndpointMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointMetadata")
            .field("session_id", &self.session_id)
            .field("url", &"<redacted>")
            .field("token", &"<redacted>")
            .field("pid", &self.pid)
            .field("stale", &self.stale)
            .finish()
    }
}

impl EndpointMetadata {
    /// Session identifier bound to this endpoint.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Owning process id when metadata recorded one.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Whether the endpoint record was explicitly marked stale by its writer.
    pub fn stale(&self) -> bool {
        self.stale
    }

    /// Authenticated websocket URL with the token applied as a query param.
    fn authenticated_url(&self) -> Result<Url> {
        let mut url = Url::parse(&self.url).map_err(|_| SdkTransportError::EndpointMalformed)?;
        url.query_pairs_mut().append_pair("token", &self.token);
        Ok(url)
    }
}

/// Raw metadata file schema (`<state-root>/sdk/<session-id>.json`).
///
/// GJC publishes `version: 1` records keyed by the `<sessionId>.json`
/// filename; `stale` marks records superseded before removal. Unknown fields
/// are ignored so newer writers stay readable.
#[derive(Deserialize)]
struct RawEndpointMetadata {
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: String,
    url: String,
    token: String,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    version: Option<u32>,
    #[serde(default)]
    stale: bool,
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_ID_CHARS
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TOKEN_CHARS
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn valid_operation_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_OPERATION_NAME_CHARS
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b'-'))
}

/// Validate a loopback-only endpoint URL from metadata.
///
/// `ws://` is accepted only for loopback IPs; `wss://` and non-loopback hosts
/// are rejected because the SDK trust boundary is the local machine.
fn validate_loopback_url(raw: &str) -> Result<()> {
    if raw.is_empty() || raw.len() > MAX_URL_CHARS {
        return Err(SdkTransportError::EndpointMalformed.into());
    }
    let url = Url::parse(raw).map_err(|_| SdkTransportError::EndpointMalformed)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !url.query().is_none_or(|query| query.is_empty())
    {
        return Err(SdkTransportError::EndpointMalformed.into());
    }
    let host = url.host_str().ok_or(SdkTransportError::EndpointMalformed)?;
    // Strip RFC 3986 brackets so IPv6 literals parse as IPs.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match url.scheme() {
        "ws" => {
            let ip: IpAddr = host
                .parse()
                .map_err(|_| SdkTransportError::EndpointMalformed)?;
            if ip.is_loopback() {
                Ok(())
            } else {
                Err(SdkTransportError::EndpointMalformed.into())
            }
        }
        _ => Err(SdkTransportError::EndpointMalformed.into()),
    }
}

/// State root anchor for a registered lane/worktree.
///
/// SDK session metadata lives at `<worktree>/.gjc/state/sdk/*.json`. The
/// worktree path is retained so discovery can prove every trust-boundary
/// component under it (`.gjc`, `.gjc/state`, `.gjc/state/sdk`) is a real
/// directory and not a symlink before reading anything.
#[derive(Debug, Clone)]
pub struct StateRoot {
    worktree: PathBuf,
}

impl StateRoot {
    /// Build the state root for an explicit worktree path.
    pub fn for_worktree(worktree: &Path) -> Self {
        Self {
            worktree: worktree.to_path_buf(),
        }
    }

    /// The state-root directory on disk.
    pub fn path(&self) -> PathBuf {
        self.worktree.join(".gjc").join("state")
    }

    /// Validate every trust-boundary component under the worktree.
    ///
    /// Missing components mean "no metadata yet" ([`SdkTransportError::EndpointUnavailable`]);
    /// symlinked or non-directory components violate the local-filesystem
    /// trust boundary ([`SdkTransportError::EndpointMalformed`]). Symlinks
    /// above the worktree root are out of scope: the worktree location is
    /// chosen by the operator when the lane is registered.
    fn validate_components(&self) -> Result<PathBuf> {
        let gjc_dir = self.worktree.join(".gjc");
        let state_dir = self.path();
        let sdk_dir = state_dir.join("sdk");
        for component in [&gjc_dir, &state_dir, &sdk_dir] {
            match std::fs::symlink_metadata(component) {
                Ok(metadata) if !metadata.is_symlink() && metadata.is_dir() => {}
                Ok(_) => return Err(SdkTransportError::EndpointMalformed.into()),
                Err(_) => {
                    return Err(SdkTransportError::EndpointUnavailable.into());
                }
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&sdk_dir)
                .map_err(|_| SdkTransportError::EndpointUnavailable)?
                .permissions()
                .mode();
            if mode & 0o002 != 0 {
                return Err(SdkTransportError::EndpointMalformed.into());
            }
        }
        Ok(sdk_dir)
    }
}

#[cfg(unix)]
fn current_uid() -> Option<u32> {
    Some(unsafe { libc::getuid() })
}

#[cfg(not(unix))]
fn current_uid() -> Option<u32> {
    None
}

/// Validate one endpoint metadata file's filesystem properties.
///
/// Rejects symlinks, non-regular files, oversized files, foreign owners, and
/// permissive group/world permissions.
fn validate_metadata_file(path: &Path) -> Result<std::fs::Metadata> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| SdkTransportError::EndpointUnavailable)?;
    if metadata.is_symlink() || !metadata.is_file() {
        return Err(SdkTransportError::EndpointMalformed.into());
    }
    if metadata.len() > MAX_METADATA_BYTES {
        return Err(SdkTransportError::EndpointMalformed.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if let (Some(file_uid), Some(current)) = (
            non_zero_uid(metadata.uid()),
            current_uid().and_then(non_zero_uid),
        ) && file_uid != current
        {
            return Err(SdkTransportError::EndpointUnauthorized.into());
        }
        let mode = metadata.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(SdkTransportError::EndpointMalformed.into());
        }
    }
    Ok(metadata)
}

#[cfg(unix)]
fn non_zero_uid(uid: u32) -> Option<u32> {
    (uid != 0).then_some(uid)
}

/// Parse and validate one endpoint record.
///
/// Supported semantics: `version` must name a schema this transport
/// understands ([`SUPPORTED_ENDPOINT_RECORD_VERSION`]); the payload session id
/// must be well-formed; the URL must be loopback-only.
fn parse_metadata(contents: &[u8]) -> Result<EndpointMetadata> {
    let raw: RawEndpointMetadata =
        serde_json::from_slice(contents).map_err(|_| SdkTransportError::EndpointMalformed)?;
    if raw.version != Some(SUPPORTED_ENDPOINT_RECORD_VERSION) {
        return Err(SdkTransportError::EndpointMalformed.into());
    }
    if !valid_session_id(&raw.session_id) {
        return Err(SdkTransportError::EndpointMalformed.into());
    }
    if !valid_token(&raw.token) {
        return Err(SdkTransportError::EndpointMalformed.into());
    }
    validate_loopback_url(&raw.url)?;
    Ok(EndpointMetadata {
        session_id: raw.session_id,
        url: raw.url,
        token: raw.token,
        pid: raw.pid,
        stale: raw.stale,
    })
}

/// Parse one candidate metadata file and bind it to its filename identity.
///
/// The endpoint filename is authority: `<session-id>.json` must exactly match
/// the recorded `sessionId`. A disagreement fails closed as malformed so a
/// renamed/copied record can never impersonate another session's endpoint.
fn parse_metadata_file(path: &Path, contents: &[u8]) -> Result<EndpointMetadata> {
    let metadata = parse_metadata(contents)?;
    let matches_filename = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem == metadata.session_id());
    if !matches_filename {
        return Err(SdkTransportError::EndpointMalformed.into());
    }
    Ok(metadata)
}

/// Check whether the endpoint's owning process is still alive.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // kill(pid, 0) probes existence without signaling.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn process_alive(_: u32) -> bool {
    // Windows refresh of liveness is delegated to connect failure; metadata is
    // accepted without a pid-based staleness check.
    true
}

/// Discovery outcome for one lane.
#[derive(Debug)]
pub enum Discovery {
    /// A validated, live endpoint for the lane's most recent session.
    Live(EndpointMetadata),
    /// The lane has no endpoint metadata at all.
    NoMetadata,
    /// Metadata exists but its owning process is gone or the record was
    /// explicitly marked stale by its writer.
    Stale {
        /// Public-safe session identifier of the stale metadata.
        session_id: String,
    },
    /// Metadata files exist but none survived validation.
    Malformed,
}

/// Discover endpoint metadata for one registered lane/worktree.
///
/// Only `<session-id>.json` files directly inside `<state-root>/sdk` are
/// considered — unrelated roots are never scanned. Identity is bound to the
/// filename; the most recently modified valid metadata file wins. Liveness of
/// its recorded pid and its explicit `stale` flag fence dead or superseded
/// sessions as [`Discovery::Stale`]. Outcome precedence is
/// Live > Stale > Malformed > NoMetadata.
pub fn discover(root: &StateRoot) -> Result<Discovery> {
    // Expected filesystem states are outcomes, not errors: a missing lane
    // layout is "no metadata yet"; symlinked/non-directory components under
    // the worktree violate the trust boundary.
    let sdk_dir = match root.validate_components() {
        Ok(sdk_dir) => sdk_dir,
        Err(error)
            if error
                .downcast_ref::<SdkTransportError>()
                .is_some_and(|e| *e == SdkTransportError::EndpointUnavailable) =>
        {
            return Ok(Discovery::NoMetadata);
        }
        Err(_) => return Ok(Discovery::Malformed),
    };
    let mut best: Option<(std::time::SystemTime, EndpointMetadata)> = None;
    let mut stale: Option<String> = None;
    let mut seen_any_file = false;
    let entries = match std::fs::read_dir(&sdk_dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(Discovery::NoMetadata),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        seen_any_file = true;
        if validate_metadata_file(&path).is_err() {
            continue;
        }
        let Ok(contents) = std::fs::read(&path) else {
            continue;
        };
        let Ok(metadata) = parse_metadata_file(&path, &contents) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if metadata.stale() || metadata.pid().is_some_and(|pid| !process_alive(pid)) {
            stale = Some(metadata.session_id().to_string());
            continue;
        }
        if best.as_ref().is_none_or(
            |(existing, _): &(std::time::SystemTime, EndpointMetadata)| modified > *existing,
        ) {
            best = Some((modified, metadata));
        }
    }
    if let Some((_, metadata)) = best {
        return Ok(Discovery::Live(metadata));
    }
    if let Some(session_id) = stale {
        return Ok(Discovery::Stale { session_id });
    }
    if seen_any_file {
        return Ok(Discovery::Malformed);
    }
    Ok(Discovery::NoMetadata)
}

/// Bounded transport configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdkTransportLimits {
    /// Connect + hello handshake timeout.
    pub connect_timeout: Duration,
    /// Per-request response timeout.
    pub request_timeout: Duration,
    /// Maximum accepted inbound text frame size.
    pub max_frame_bytes: usize,
    /// Maximum accepted inbound payload size after envelope decoding.
    pub max_payload_bytes: usize,
    /// Maximum reconnect attempts per request.
    pub max_reconnect_attempts: u64,
}

impl Default for SdkTransportLimits {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(10),
            max_frame_bytes: 65_536,
            max_payload_bytes: 65_536,
            max_reconnect_attempts: 2,
        }
    }
}

impl SdkTransportLimits {
    fn sanitized(self) -> Self {
        Self {
            connect_timeout: self.connect_timeout.max(Duration::from_millis(100)),
            request_timeout: self.request_timeout.max(Duration::from_millis(100)),
            max_frame_bytes: self.max_frame_bytes.clamp(1024, ABSOLUTE_MAX_FRAME_BYTES),
            max_payload_bytes: self.max_payload_bytes.clamp(1024, ABSOLUTE_MAX_FRAME_BYTES),
            max_reconnect_attempts: self.max_reconnect_attempts.min(5),
        }
    }
}

/// Typed SDK v3 request frame sent to the endpoint.
///
/// The v3 wire contract carries exactly one of `query` (for
/// `query_request`) or `operation` (for `control_request`/`broker_request`)
/// plus a JSON-object `input`; responses echo the correlation `id` on the
/// matching `*_response` frame.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SdkRequest {
    /// Correlation ID echoed by the server on the matching response.
    pub id: String,
    /// Frame discriminator: `query_request`, `control_request`, or
    /// `broker_request`.
    #[serde(rename = "type")]
    pub frame_type: &'static str,
    /// Query name; set only on `query_request` frames.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Operation name; set only on `control_request`/`broker_request` frames.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// Object-shaped call input; serialized as `{}` when absent.
    pub input: Value,
}

impl SdkRequest {
    /// Build a typed v3 request with a fresh correlation ID.
    fn v3(
        frame_type: &'static str,
        selector: Option<(&'static str, String)>,
        input: Value,
    ) -> Self {
        let (query, operation) = match selector {
            Some(("query", name)) => (Some(name), None),
            Some(("operation", name)) => (None, Some(name)),
            _ => (None, None),
        };
        Self {
            id: new_correlation_id(),
            frame_type,
            query,
            operation,
            input: normalize_input(input),
        }
    }

    /// Build a `query_request` for one host-published query.
    pub fn query(query: impl Into<String>, input: Value) -> Self {
        Self::v3("query_request", Some(("query", query.into())), input)
    }

    /// Build a `control_request` for one session control operation.
    /// Reusable v3 surface for the planned control/event consumers (#323-#326);
    /// the diagnostic probe itself only exercises queries.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn control(operation: impl Into<String>, input: Value) -> Self {
        Self::v3(
            "control_request",
            Some(("operation", operation.into())),
            input,
        )
    }

    /// Build a `broker_request` for one broker-global operation.
    /// Reusable v3 surface for the planned control/event consumers (#323-#326);
    /// the diagnostic probe itself only exercises queries.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn broker(operation: impl Into<String>, input: Value) -> Self {
        Self::v3(
            "broker_request",
            Some(("operation", operation.into())),
            input,
        )
    }

    /// Correlation ID of this request.
    pub fn correlation_id(&self) -> &str {
        &self.id
    }

    /// The `*_response` frame family that correlates to this request.
    fn response_frame_type(&self) -> &'static str {
        match self.frame_type {
            "control_request" => "control_response",
            "broker_request" => "broker_response",
            _ => "query_response",
        }
    }

    fn encode(&self) -> Result<String> {
        if !self.input.is_object() {
            return Err(SdkTransportError::FrameRejected.into());
        }
        let selector_ok = match (self.frame_type, &self.query, &self.operation) {
            ("query_request", Some(query), None) => valid_operation_name(query),
            ("control_request", None, Some(operation))
            | ("broker_request", None, Some(operation)) => valid_operation_name(operation),
            _ => false,
        };
        if !selector_ok {
            return Err(SdkTransportError::FrameRejected.into());
        }
        let frame = serde_json::to_string(self).map_err(|_| SdkTransportError::FrameRejected)?;
        if frame.len() > ABSOLUTE_MAX_FRAME_BYTES {
            return Err(SdkTransportError::FrameRejected.into());
        }
        Ok(frame)
    }
}

/// Coerce absent inputs to the empty object the v3 contract requires.
fn normalize_input(input: Value) -> Value {
    if input.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        input
    }
}

/// Typed response envelope received from the SDK endpoint.
///
/// Successful v3 responses carry `ok: true` plus a `result` and/or paged
/// `page` payload; failures carry `ok: false` with a structured `error`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SdkResponse {
    /// Correlation ID matching the request this responds to.
    pub id: Option<String>,
    /// Response discriminator (`query_response`, `control_response`,
    /// `broker_response`, ...).
    #[serde(rename = "type")]
    pub frame_type: String,
    /// Server-reported success flag when present.
    #[serde(default)]
    pub ok: Option<bool>,
    /// Unpaged response result.
    #[serde(default)]
    pub result: Value,
    /// Paged response envelope (`items`, `complete`, ...).
    #[serde(default)]
    pub page: Value,
    /// Server-reported error code when present.
    #[serde(default)]
    pub error: Option<SdkServerError>,
}

/// Server-reported error block inside a response envelope.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SdkServerError {
    /// Stable error code string.
    #[serde(default)]
    pub code: Option<String>,
    /// Redacted, bounded message (never trusted verbatim for routing).
    #[serde(default)]
    pub message: Option<String>,
}

/// Generate a bounded, opaque correlation ID.
pub fn new_correlation_id() -> String {
    let uuid = uuid::Uuid::new_v4();
    let mut id = String::with_capacity(36);
    for (index, byte) in uuid.as_bytes().iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            id.push('-');
        }
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

/// Hello frame emitted by the server immediately after authentication.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct HelloFrame {
    /// Server-assigned connection identifier.
    #[serde(default, rename = "connectionId")]
    pub connection_id: Option<String>,
}

fn valid_connection_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CONNECTION_ID_CHARS
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Strictly parse the post-authentication `hello` frame.
///
/// The first server frame MUST be a JSON object with `type == "hello"` and a
/// bounded, well-formed `connectionId`; anything else violates the v3
/// handshake contract and fails closed as [`SdkTransportError::InvalidHello`].
fn parse_hello(frame: &str, limits: &SdkTransportLimits) -> Result<HelloFrame> {
    if frame.len() > limits.max_frame_bytes {
        return Err(SdkTransportError::FrameRejected.into());
    }
    let value: Value = serde_json::from_str(frame).map_err(|_| SdkTransportError::InvalidHello)?;
    if value.get("type").and_then(Value::as_str) != Some("hello") {
        return Err(SdkTransportError::InvalidHello.into());
    }
    let hello: HelloFrame =
        serde_json::from_value(value).map_err(|_| SdkTransportError::InvalidHello)?;
    let connection_id = hello
        .connection_id
        .as_deref()
        .filter(|id| valid_connection_id(id))
        .ok_or(SdkTransportError::InvalidHello)?;
    debug_assert!(connection_id.len() <= MAX_CONNECTION_ID_CHARS);
    Ok(hello)
}

fn parse_response(frame: &str, limits: &SdkTransportLimits) -> Result<SdkResponse> {
    if frame.len() > limits.max_frame_bytes {
        return Err(SdkTransportError::FrameRejected.into());
    }
    let response: SdkResponse =
        serde_json::from_str(frame).map_err(|_| SdkTransportError::FrameRejected)?;
    for payload in [&response.result, &response.page] {
        if serde_json::to_string(payload)
            .is_ok_and(|encoded| encoded.len() > limits.max_payload_bytes)
        {
            return Err(SdkTransportError::FrameRejected.into());
        }
    }
    Ok(response)
}

/// Authenticated, typed websocket client for one SDK endpoint.
///
/// The client owns one hello-gated connection: [`SdkClient::connect`]
/// establishes it and validates the server `hello`, and every
/// [`SdkClient::request`] flows over that same authenticated connection until
/// it drops. A lost connection is re-established (hello gate included) up to
/// the bounded reconnect budget before [`SdkTransportError::RetryExhausted`].
pub struct SdkClient {
    metadata: EndpointMetadata,
    limits: SdkTransportLimits,
    stream: Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    hello: Option<HelloFrame>,
}

impl SdkClient {
    /// Build a client over validated endpoint metadata.
    pub fn new(metadata: EndpointMetadata) -> Self {
        Self {
            metadata,
            limits: SdkTransportLimits::default(),
            stream: None,
            hello: None,
        }
    }

    /// Override transport bounds.
    pub fn with_limits(mut self, limits: SdkTransportLimits) -> Self {
        self.limits = limits.sanitized();
        self
    }

    async fn connect_once(&self) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
        let url = self.metadata.authenticated_url()?;
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| SdkTransportError::EndpointMalformed)?;
        // Defense in depth: bearer header mirrors the query-param credential.
        let header_value = tokio_tungstenite::tungstenite::http::HeaderValue::from_str(&format!(
            "Bearer {}",
            self.metadata.token
        ))
        .map_err(|_| SdkTransportError::EndpointMalformed)?;
        request.headers_mut().append(AUTHORIZATION, header_value);
        let attempt = tokio::time::timeout(
            self.limits.connect_timeout,
            tokio_tungstenite::connect_async_with_config(
                request,
                Some(
                    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
                        .max_message_size(Some(self.limits.max_frame_bytes))
                        .max_frame_size(Some(self.limits.max_frame_bytes)),
                ),
                false,
            ),
        )
        .await;
        match attempt {
            Err(_elapsed) => Err(SdkTransportError::Timeout.into()),
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(response))) => {
                if response.status().is_client_error() {
                    Err(SdkTransportError::EndpointUnauthorized.into())
                } else {
                    Err(SdkTransportError::EndpointUnavailable.into())
                }
            }
            Ok(Err(_)) => Err(SdkTransportError::EndpointUnavailable.into()),
            Ok(Ok((stream, _))) => Ok(stream),
        }
    }

    /// Open an authenticated connection, await the server `hello`, and keep
    /// the connection for subsequent requests.
    ///
    /// No request frame may precede the validated hello: the connection is
    /// returned to the caller only after the server's identity frame passed
    /// strict type/identity validation.
    pub async fn connect(&mut self) -> Result<HelloFrame> {
        let mut stream = self.connect_once().await?;
        let next = tokio::time::timeout(self.limits.connect_timeout, stream.next()).await;
        let message = match next {
            Err(_elapsed) => {
                return Err(SdkTransportError::Timeout.into());
            }
            Ok(None) => {
                return Err(SdkTransportError::ConnectionClosed.into());
            }
            Ok(Some(message)) => message.map_err(|_| SdkTransportError::ConnectionClosed)?,
        };
        let Message::Text(text) = message else {
            return Err(SdkTransportError::InvalidHello.into());
        };
        let hello = parse_hello(&text, &self.limits)?;
        self.stream = Some(stream);
        self.hello = Some(hello.clone());
        Ok(hello)
    }

    /// Identity of the currently pooled authenticated connection, when the
    /// hello gate has most recently passed.
    pub fn hello(&self) -> Option<&HelloFrame> {
        self.hello.as_ref()
    }

    /// Send one typed v3 request over the authenticated connection and await
    /// its correlated `*_response`.
    ///
    /// The first call connects and completes the hello gate implicitly. A
    /// connection lost mid-exchange is re-established (re-authenticated) up to
    /// `max_reconnect_attempts` times before surfacing
    /// [`SdkTransportError::RetryExhausted`]; connect failures are terminal
    /// for the call.
    pub async fn request(&mut self, request: &SdkRequest) -> Result<SdkResponse> {
        let frame = request.encode()?;
        let mut attempts = 0u64;
        loop {
            attempts = attempts.saturating_add(1);
            if self.stream.is_none() {
                self.connect().await?;
            }
            let Some(stream) = self.stream.as_mut() else {
                return Err(SdkTransportError::ConnectionClosed.into());
            };
            match exchange(stream, request, &frame, &self.limits).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    // Any mid-exchange failure poisons the pooled connection;
                    // the next attempt starts from a fresh authenticated
                    // handshake instead of trusting a half-dead socket.
                    self.stream = None;
                    self.hello = None;
                    let connection_lost = error
                        .downcast_ref::<SdkTransportError>()
                        .is_some_and(|e| *e == SdkTransportError::ConnectionClosed);
                    if !(connection_lost && attempts <= self.limits.max_reconnect_attempts) {
                        if connection_lost {
                            return Err(SdkTransportError::RetryExhausted.into());
                        }
                        return Err(error);
                    }
                }
            }
        }
    }
}

async fn exchange(
    stream: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    request: &SdkRequest,
    frame: &str,
    limits: &SdkTransportLimits,
) -> Result<SdkResponse> {
    let send = stream.send(Message::Text(frame.into()));
    tokio::time::timeout(limits.request_timeout, send)
        .await
        .map_err(|_| SdkTransportError::Timeout)?
        .map_err(|_| SdkTransportError::ConnectionClosed)?;
    // Bounded tolerance for id-less server frames (notifications, events) that
    // cannot be correlated to this request.
    let mut uncorrelated = 0u8;
    loop {
        let next = tokio::time::timeout(limits.request_timeout, stream.next()).await;
        let message = match next {
            Ok(Some(message)) => message.map_err(|_| SdkTransportError::ConnectionClosed)?,
            Ok(None) => return Err(SdkTransportError::ConnectionClosed.into()),
            Err(_) => return Err(SdkTransportError::Timeout.into()),
        };
        let Message::Text(text) = message else {
            return Err(SdkTransportError::FrameRejected.into());
        };
        let response = parse_response(&text, limits)?;
        match response.id.as_deref() {
            Some(id) if id == request.correlation_id() => {
                if response.frame_type == request.response_frame_type() {
                    return Ok(response);
                }
                // A correlated frame of the wrong family is a protocol
                // violation, not a matchable response.
                return Err(SdkTransportError::FrameRejected.into());
            }
            // A correlated frame for a different request is a protocol
            // violation on a single-request diagnostic transport.
            Some(_) => return Err(SdkTransportError::CorrelationMismatch.into()),
            None => match response.frame_type.as_str() {
                "hello" => continue,
                _ => {
                    uncorrelated = uncorrelated.saturating_add(1);
                    if uncorrelated >= 16 {
                        return Err(SdkTransportError::CorrelationMismatch.into());
                    }
                }
            },
        }
    }
}

/// Public-safe diagnostic summarizing a discovery/transport failure.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SdkDiagnostic {
    /// Stable reason code (see [`SdkTransportError::reason`]).
    pub reason: &'static str,
    /// Public-safe session identifier when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Human-safe bounded detail; never contains URLs or tokens.
    pub detail: String,
}

/// Render a transport error into a redacted diagnostic.
pub fn redact_error(error: &crate::DynError, session_id: Option<&str>) -> SdkDiagnostic {
    let reason = error
        .downcast_ref::<SdkTransportError>()
        .map(|error: &SdkTransportError| error.reason())
        .unwrap_or("transport_failed");
    let detail = match reason {
        "endpoint_unavailable" => "no SDK endpoint metadata under the lane state root",
        "endpoint_malformed" => "SDK endpoint metadata failed validation",
        "endpoint_unauthorized" => "SDK endpoint rejected authentication",
        "invalid_hello" => "SDK endpoint hello violated the v3 handshake contract",
        "timeout" => "SDK transport exceeded its bounded timeout",
        "connection_closed" => "SDK endpoint closed the connection",
        "frame_rejected" => "SDK frame violated size or shape bounds",
        "correlation_mismatch" => "SDK response correlation did not match the request",
        "retry_exhausted" => "SDK transport exhausted bounded reconnect attempts",
        _ => "SDK transport failed",
    };
    SdkDiagnostic {
        reason,
        session_id: session_id.map(str::to_string),
        detail: detail.to_string(),
    }
}

/// Options for `clawhip gjc inspect`.
#[derive(Debug, Clone)]
pub struct InspectOptions {
    /// Explicit worktree root; defaults to the current directory.
    pub worktree: Option<PathBuf>,
    /// Open the discovered endpoint and issue one typed probe request.
    pub probe: bool,
    /// Emit machine-readable JSON instead of text.
    pub json_output: bool,
}

/// Public-safe inspection snapshot for one lane.
#[derive(Debug, Serialize)]
pub struct InspectSnapshot {
    /// `live`, `stale`, `unavailable`, or `malformed`.
    pub status: &'static str,
    /// Validated session identifier when discovery succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Owning pid when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Redacted diagnostic when status is not `live`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<SdkDiagnostic>,
    /// Redacted transport probe result when requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeReport>,
}

/// Public-safe result of one typed transport probe.
#[derive(Debug, Serialize)]
pub struct ProbeReport {
    /// Server-assigned connection id from the authenticated hello handshake.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hello_connection_id: Option<String>,
    /// Handshake failure reason code when the hello exchange failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hello_reason: Option<&'static str>,
    /// Whether the correlated request round-trip succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_ok: Option<bool>,
    /// Whether the response carried this request's correlation ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_correlated: Option<bool>,
    /// Request failure reason code when the round-trip failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_reason: Option<&'static str>,
    /// Server-reported stable error code for an application-level failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_error_code: Option<String>,
}

impl ProbeReport {
    fn failed(reason: &'static str) -> Self {
        Self {
            hello_connection_id: None,
            hello_reason: Some(reason),
            request_ok: None,
            request_correlated: None,
            request_reason: None,
            request_error_code: None,
        }
    }
}

/// Bound a server-reported error code to a safe diagnostic token.
fn sanitize_error_code(code: Option<&str>) -> Option<String> {
    let mut sanitized: String = code?
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .take(MAX_OPERATION_NAME_CHARS)
        .collect();
    sanitized.truncate(MAX_OPERATION_NAME_CHARS);
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

/// Bounded defaults for the diagnostic probe path.
fn probe_limits() -> SdkTransportLimits {
    SdkTransportLimits {
        connect_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(2),
        max_frame_bytes: 65_536,
        max_payload_bytes: 65_536,
        max_reconnect_attempts: 1,
    }
}

#[allow(clippy::too_many_lines)]
async fn probe_transport(metadata: EndpointMetadata) -> ProbeReport {
    let mut client = SdkClient::new(metadata).with_limits(probe_limits());
    if let Err(error) = client.connect().await {
        let reason = error
            .downcast_ref::<SdkTransportError>()
            .map(|error: &SdkTransportError| error.reason())
            .unwrap_or("transport_failed");
        return ProbeReport::failed(reason);
    }
    let request = SdkRequest::query(PROBE_QUERY, serde_json::Value::Null);
    let correlation_id = request.correlation_id().to_string();
    match client.request(&request).await {
        Ok(response) => ProbeReport {
            hello_connection_id: client.hello().and_then(|hello| hello.connection_id.clone()),
            hello_reason: None,
            request_ok: Some(response.ok.unwrap_or(true)),
            request_correlated: Some(response.id.as_deref() == Some(correlation_id.as_str())),
            request_reason: None,
            request_error_code: (!response.ok.unwrap_or(true))
                .then(|| {
                    sanitize_error_code(response.error.as_ref().and_then(|e| e.code.as_deref()))
                })
                .flatten(),
        },
        Err(error) => {
            let reason = error
                .downcast_ref::<SdkTransportError>()
                .map(|error: &SdkTransportError| error.reason())
                .unwrap_or("transport_failed");
            ProbeReport {
                hello_connection_id: client.hello().and_then(|hello| hello.connection_id.clone()),
                hello_reason: None,
                request_ok: Some(false),
                request_correlated: None,
                request_reason: Some(reason),
                request_error_code: None,
            }
        }
    }
}

async fn inspect_snapshot(worktree: &Path, probe: bool) -> Result<InspectSnapshot> {
    let root = StateRoot::for_worktree(worktree);
    let (status, session_id, pid, diagnostic) = match discover(&root) {
        Ok(Discovery::Live(metadata)) => (
            "live",
            Some(metadata.session_id().to_string()),
            metadata.pid(),
            None,
        ),
        Ok(Discovery::Stale { session_id }) => (
            "stale",
            Some(session_id),
            None,
            Some(SdkDiagnostic {
                reason: "endpoint_stale",
                session_id: None,
                detail: "SDK endpoint metadata exists but its process is gone".to_string(),
            }),
        ),
        Ok(Discovery::Malformed) => (
            "malformed",
            None,
            None,
            Some(SdkDiagnostic {
                reason: "endpoint_malformed",
                session_id: None,
                detail: "SDK endpoint metadata failed validation".to_string(),
            }),
        ),
        Ok(Discovery::NoMetadata) => (
            "unavailable",
            None,
            None,
            Some(SdkDiagnostic {
                reason: "endpoint_unavailable",
                session_id: None,
                detail: "no SDK endpoint metadata under the lane state root".to_string(),
            }),
        ),
        Err(error) => {
            let diagnostic = redact_error(&error, None);
            (
                "malformed",
                diagnostic.session_id.clone(),
                None,
                Some(diagnostic),
            )
        }
    };
    // Only a live endpoint can be probed; everything else reports discovery
    // state only. The metadata never leaves this function unredacted.
    let probe_report = if probe {
        match discover(&root)? {
            Discovery::Live(metadata) => Some(probe_transport(metadata).await),
            _ => None,
        }
    } else {
        None
    };
    Ok(InspectSnapshot {
        status,
        session_id,
        pid,
        diagnostic,
        probe: probe_report,
    })
}

/// Run `clawhip gjc inspect`: discovery-only, redacted, read-only.
pub async fn run_inspect(options: InspectOptions) -> Result<()> {
    let worktree = match options.worktree {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let snapshot = inspect_snapshot(&worktree, options.probe).await?;
    if options.json_output {
        println!("{}", serde_json::to_string(&snapshot)?);
    } else {
        render_inspect(&snapshot);
    }
    Ok(())
}

fn render_inspect(snapshot: &InspectSnapshot) {
    println!("gjc sdk transport: {}", snapshot.status);
    if let Some(session_id) = &snapshot.session_id {
        println!("  session: {session_id}");
    }
    if let Some(pid) = snapshot.pid {
        println!("  pid: {pid}");
    }
    if let Some(diagnostic) = &snapshot.diagnostic {
        println!("  reason: {}", diagnostic.reason);
        println!("  detail: {}", diagnostic.detail);
    }
    if let Some(probe) = &snapshot.probe {
        if let Some(connection_id) = &probe.hello_connection_id {
            println!("  hello: {connection_id}");
        }
        if let Some(reason) = probe.hello_reason {
            println!("  hello_reason: {reason}");
        }
        if let Some(ok) = probe.request_ok {
            println!("  request_ok: {ok}");
        }
        if let Some(correlated) = probe.request_correlated {
            println!("  request_correlated: {correlated}");
        }
        if let Some(reason) = probe.request_reason {
            println!("  request_reason: {reason}");
        }
        if let Some(code) = &probe.request_error_code {
            println!("  request_error_code: {code}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    fn temp_worktree(name: &str) -> (tempfile::TempDir, StateRoot) {
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().join(name);
        std::fs::create_dir_all(worktree.join(".gjc").join("state").join("sdk")).unwrap();
        let root = StateRoot::for_worktree(&worktree);
        (dir, root)
    }

    fn write_metadata_at(root: &StateRoot, file_name: &str, contents: &str) -> PathBuf {
        let path = root.path().join("sdk").join(file_name);
        std::fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o600);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        path
    }

    const SESSION_ID: &str = "01a02ccd-c754-7656-95c7-f40b5a140bc3";

    fn metadata_json(url: &str, token: &str, pid: Option<u32>) -> String {
        let pid_field = pid.map(|pid| format!(",\"pid\":{pid}")).unwrap_or_default();
        format!(
            "{{\"version\":1,\"sessionId\":\"{SESSION_ID}\",\"url\":\"{url}\",\"token\":\"{token}\"{pid_field}}}"
        )
    }

    #[test]
    fn loopback_url_validation_accepts_only_loopback_ws() {
        assert!(validate_loopback_url("ws://127.0.0.1:1234/").is_ok());
        assert!(validate_loopback_url("ws://[::1]:1234/").is_ok());
        for bad in [
            "ws://10.0.0.5:1234/",
            "ws://192.168.1.2:1234/",
            "ws://localhost:1234/",
            "wss://127.0.0.1:1234/",
            "http://127.0.0.1:1234/",
            "ws://user:pass@127.0.0.1:1234/",
            "ws://127.0.0.1:1234/?token=x",
            "ws://127.0.0.1:1234/#frag",
            "",
            &format!("ws://127.0.0.1:1/{}", "x".repeat(600)),
        ] {
            assert!(
                validate_loopback_url(bad).is_err(),
                "expected rejection: {bad}"
            );
        }
    }

    #[test]
    fn token_session_id_and_operation_charset_bounds() {
        assert!(valid_token("abcABC123_-."));
        assert!(valid_token("0ECXWhpn0Bm6KtlhGxhqz8PEOX1Y6Xzi"));
        assert!(!valid_token(""));
        assert!(!valid_token("has space"));
        assert!(!valid_token("semicolon;"));
        assert!(!valid_token(&"x".repeat(MAX_TOKEN_CHARS + 1)));

        assert!(valid_session_id(SESSION_ID));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("../escape"));
        assert!(!valid_session_id("has_underscore"));
        assert!(!valid_session_id(&"x".repeat(MAX_SESSION_ID_CHARS + 1)));

        assert!(valid_operation_name("session.metadata"));
        assert!(valid_operation_name("providers.list/active"));
        assert!(valid_operation_name("turn.abort"));
        assert!(!valid_operation_name(""));
        assert!(!valid_operation_name("has space"));
        assert!(!valid_operation_name(
            &"x".repeat(MAX_OPERATION_NAME_CHARS + 1)
        ));
    }

    #[test]
    fn parse_metadata_enforces_supported_version_stale_and_identity() {
        let parsed = parse_metadata(
            metadata_json("ws://127.0.0.1:43065/", "tok0ECXWhpn", Some(42)).as_bytes(),
        )
        .unwrap();
        assert_eq!(parsed.session_id(), SESSION_ID);
        assert_eq!(parsed.pid(), Some(42));
        assert!(!parsed.stale());

        let snake = "{\"session_id\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"url\":\"ws://127.0.0.1:1/\",\"token\":\"tok\",\"version\":1}";
        let parsed = parse_metadata(snake.as_bytes()).unwrap();
        assert_eq!(parsed.session_id(), SESSION_ID);

        // Explicitly stale records stay parseable; discovery fences them.
        let stale = "{\"version\":1,\"sessionId\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"url\":\"ws://127.0.0.1:1/\",\"token\":\"tok\",\"stale\":true}";
        let parsed = parse_metadata(stale.as_bytes()).unwrap();
        assert!(parsed.stale());

        // Unsupported or missing record versions fail closed.
        for bad_version in [
            "{\"version\":2,\"sessionId\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"url\":\"ws://127.0.0.1:1/\",\"token\":\"tok\"}",
            "{\"version\":0,\"sessionId\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"url\":\"ws://127.0.0.1:1/\",\"token\":\"tok\"}",
            "{\"sessionId\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"url\":\"ws://127.0.0.1:1/\",\"token\":\"tok\"}",
        ] {
            assert!(
                parse_metadata(bad_version.as_bytes()).is_err(),
                "expected version rejection: {bad_version}"
            );
        }

        // Missing fields / wrong types are malformed.
        for bad in [
            "{}",
            "{\"version\":1,\"sessionId\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"url\":\"ws://127.0.0.1:1/\"}",
            "{\"version\":1,\"sessionId\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"token\":\"tok\"}",
        ] {
            assert!(parse_metadata(bad.as_bytes()).is_err(), "{bad}");
        }
    }

    #[test]
    fn endpoint_filename_is_the_identity_authority() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{SESSION_ID}.json"));
        std::fs::write(&path, metadata_json("ws://127.0.0.1:1/", "tok", None)).unwrap();
        let parsed = parse_metadata_file(&path, std::fs::read(&path).unwrap().as_slice()).unwrap();
        assert_eq!(parsed.session_id(), SESSION_ID);

        // Renamed/copied records cannot impersonate another session.
        let impostor = dir.path().join("01a00000-0000-0000-0000-000000000009.json");
        std::fs::write(&impostor, metadata_json("ws://127.0.0.1:1/", "tok", None)).unwrap();
        assert!(
            parse_metadata_file(&impostor, std::fs::read(&impostor).unwrap().as_slice()).is_err()
        );

        // Non-UTF8 stems can never match either.
        let odd = dir.path().join("weird name.json");
        std::fs::write(&odd, metadata_json("ws://127.0.0.1:1/", "tok", None)).unwrap();
        assert!(parse_metadata_file(&odd, std::fs::read(&odd).unwrap().as_slice()).is_err());
    }

    #[test]
    fn endpoint_metadata_debug_never_leaks_secrets() {
        let parsed = parse_metadata(
            metadata_json("ws://127.0.0.1:43065/", "secret-token-value", None).as_bytes(),
        )
        .unwrap();
        let debug = format!("{parsed:?}");
        assert!(!debug.contains("secret-token-value"));
        assert!(!debug.contains("127.0.0.1"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn discover_reports_no_metadata_for_empty_sdk_dir() {
        let (_dir, root) = temp_worktree("wt");
        match discover(&root).unwrap() {
            Discovery::NoMetadata => {}
            other => panic!("expected NoMetadata, got {other:?}"),
        }
    }

    #[test]
    fn discover_live_and_newest_wins() {
        let (_dir, root) = temp_worktree("wt");
        write_metadata_at(&root, "01a00000-0000-0000-0000-000000000001.json", &{
            let mut body =
                metadata_json("ws://127.0.0.1:11111/", "tokold", Some(std::process::id()));
            body = body.replace(SESSION_ID, "01a00000-0000-0000-0000-000000000001");
            body
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_metadata_at(
            &root,
            &format!("{SESSION_ID}.json"),
            &metadata_json("ws://127.0.0.1:22222/", "toknew", Some(std::process::id())),
        );
        match discover(&root).unwrap() {
            Discovery::Live(metadata) => {
                assert_eq!(metadata.session_id(), SESSION_ID);
                assert_eq!(metadata.pid(), Some(std::process::id()));
            }
            other => panic!("expected Live, got {other:?}"),
        }
    }

    #[test]
    fn discover_stale_when_pid_dead_or_record_flagged() {
        let (_dir, root) = temp_worktree("wt");
        write_metadata_at(
            &root,
            &format!("{SESSION_ID}.json"),
            &metadata_json("ws://127.0.0.1:1/", "tok", Some(u32::MAX - 15)),
        );
        match discover(&root).unwrap() {
            Discovery::Stale { session_id } => assert_eq!(session_id, SESSION_ID),
            other => panic!("expected Stale, got {other:?}"),
        }

        // An explicit stale flag fences even a live pid.
        let flagged = format!(
            "{{\"version\":1,\"sessionId\":\"{SESSION_ID}\",\"url\":\"ws://127.0.0.1:1/\",\"token\":\"tok\",\"pid\":{},\"stale\":true}}",
            std::process::id()
        );
        let (_dir2, root2) = temp_worktree("wt2");
        write_metadata_at(&root2, &format!("{SESSION_ID}.json"), &flagged);
        match discover(&root2).unwrap() {
            Discovery::Stale { session_id } => assert_eq!(session_id, SESSION_ID),
            other => panic!("expected Stale for flagged record, got {other:?}"),
        }
    }

    #[test]
    fn discover_fails_closed_on_filename_mismatch() {
        let (_dir, root) = temp_worktree("wt");
        // Payload claims another session than its filename authorizes.
        write_metadata_at(
            &root,
            "01a00000-0000-0000-0000-000000000002.json",
            &metadata_json("ws://127.0.0.1:1/", "tok", Some(std::process::id())),
        );
        match discover(&root).unwrap() {
            Discovery::Malformed => {}
            other => panic!("filename mismatch must fail closed, got {other:?}"),
        }
    }

    #[test]
    fn discover_malformed_when_files_exist_but_none_valid() {
        let (_dir, root) = temp_worktree("wt");
        write_metadata_at(&root, "a.json", "not json");
        match discover(&root).unwrap() {
            Discovery::Malformed => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn discover_skips_symlinks_and_permissive_files() {
        let (_dir, root) = temp_worktree("wt");

        // Symlinked metadata file.
        let outside = root.path().join("outside.json");
        std::fs::write(
            &outside,
            metadata_json("ws://127.0.0.1:1/", "tok", Some(std::process::id())),
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.path().join("sdk").join("linked.json")).unwrap();
        #[cfg(not(unix))]
        std::fs::copy(&outside, root.path().join("sdk").join("linked.json")).unwrap();
        match discover(&root).unwrap() {
            // A present-but-invalid entry is malformed metadata, never a
            // live endpoint.
            Discovery::Malformed => {}
            other => panic!("symlink must not yield metadata: {other:?}"),
        }
        #[cfg(unix)]
        std::fs::remove_file(root.path().join("sdk").join("linked.json")).unwrap();

        // Group/world readable metadata file is rejected by permission policy.
        let permissive = write_metadata_at(
            &root,
            &format!("{SESSION_ID}.json"),
            &metadata_json("ws://127.0.0.1:1/", "tok", Some(std::process::id())),
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&permissive).unwrap().permissions();
            permissions.set_mode(0o644);
            std::fs::set_permissions(&permissive, permissions).unwrap();
        }
        match discover(&root).unwrap() {
            Discovery::Malformed | Discovery::NoMetadata => {}
            other => panic!("permissive metadata must be rejected: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn discover_rejects_symlinked_or_writable_sdk_dir() {
        let dir = tempfile::tempdir().unwrap();

        // Symlinked state directory.
        let outside_state = dir.path().join("outside-state");
        std::fs::create_dir_all(&outside_state).unwrap();
        let wt = dir.path().join("wt-symlink");
        std::fs::create_dir_all(wt.join(".gjc")).unwrap();
        std::os::unix::fs::symlink(&outside_state, wt.join(".gjc").join("state")).unwrap();
        assert!(matches!(
            discover(&StateRoot::for_worktree(&wt)).unwrap(),
            Discovery::Malformed
        ));

        // World-writable sdk dir.
        let (_keep_alive, root) = temp_worktree("wt-mode");
        let sdk_dir = root.path().join("sdk");
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&sdk_dir).unwrap().permissions();
        permissions.set_mode(0o777);
        std::fs::set_permissions(&sdk_dir, permissions).unwrap();
        write_metadata_at(
            &root,
            &format!("{SESSION_ID}.json"),
            &metadata_json("ws://127.0.0.1:1/", "tok", Some(std::process::id())),
        );
        assert!(matches!(discover(&root).unwrap(), Discovery::Malformed));

        // Group-writable sdk dir is tolerated (umask 002 environments).
        let (_keep_alive_group, root) = temp_worktree("wt-group-mode");
        let sdk_dir = root.path().join("sdk");
        let mut permissions = std::fs::metadata(&sdk_dir).unwrap().permissions();
        permissions.set_mode(0o770);
        std::fs::set_permissions(&sdk_dir, permissions).unwrap();
        write_metadata_at(
            &root,
            &format!("{SESSION_ID}.json"),
            &metadata_json("ws://127.0.0.1:1/", "tok", Some(std::process::id())),
        );
        assert!(matches!(discover(&root).unwrap(), Discovery::Live(_)));
    }

    #[test]
    fn discovery_scopes_to_lane_state_root_only() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let unrelated = dir.path().join("unrelated");
        std::fs::create_dir_all(target.join(".gjc").join("state").join("sdk")).unwrap();
        std::fs::create_dir_all(unrelated.join(".gjc").join("state").join("sdk")).unwrap();
        write_metadata_at(
            &StateRoot::for_worktree(&unrelated),
            &format!("{SESSION_ID}.json"),
            &metadata_json(
                "ws://127.0.0.1:33333/",
                "tokother",
                Some(std::process::id()),
            ),
        );
        match discover(&StateRoot::for_worktree(&target)).unwrap() {
            Discovery::NoMetadata => {}
            other => panic!("unrelated lane leaked into target: {other:?}"),
        }
    }

    #[test]
    fn redact_error_maps_every_taxonomy_reason() {
        let cases = [
            (
                SdkTransportError::EndpointUnavailable,
                "endpoint_unavailable",
            ),
            (SdkTransportError::EndpointMalformed, "endpoint_malformed"),
            (
                SdkTransportError::EndpointUnauthorized,
                "endpoint_unauthorized",
            ),
            (SdkTransportError::InvalidHello, "invalid_hello"),
            (SdkTransportError::Timeout, "timeout"),
            (SdkTransportError::ConnectionClosed, "connection_closed"),
            (SdkTransportError::FrameRejected, "frame_rejected"),
            (
                SdkTransportError::CorrelationMismatch,
                "correlation_mismatch",
            ),
            (SdkTransportError::RetryExhausted, "retry_exhausted"),
        ];
        for (error, reason) in cases {
            let boxed: crate::DynError = error.into();
            let diagnostic = redact_error(&boxed, Some(SESSION_ID));
            assert_eq!(diagnostic.reason, reason);
            assert_eq!(diagnostic.session_id.as_deref(), Some(SESSION_ID));
            let encoded = serde_json::to_string(&diagnostic).unwrap();
            assert!(!encoded.contains("ws://"), "{encoded}");
            assert!(!encoded.contains("127.0.0.1"), "{encoded}");
            // Display mirrors the reason code.
            assert_eq!(boxed.to_string(), reason);
        }
    }

    #[test]
    fn typed_requests_encode_v3_frames_with_object_input() {
        let query = SdkRequest::query("session.metadata", Value::Null);
        assert_eq!(query.frame_type, "query_request");
        let encoded = query.encode().unwrap();
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["type"], json!("query_request"));
        assert_eq!(value["query"], json!("session.metadata"));
        assert_eq!(value["input"], json!({}));
        assert_eq!(value["id"], json!(query.correlation_id()));
        assert!(value.get("operation").is_none());
        assert_eq!(query.response_frame_type(), "query_response");

        let control = SdkRequest::control("turn.abort", json!({"mode": "terminal"}));
        let encoded = control.encode().unwrap();
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["type"], json!("control_request"));
        assert_eq!(value["operation"], json!("turn.abort"));
        assert_eq!(value["input"], json!({"mode": "terminal"}));
        assert!(value.get("query").is_none());
        assert_eq!(control.response_frame_type(), "control_response");

        let broker = SdkRequest::broker("session.list", Value::Null);
        let encoded = broker.encode().unwrap();
        let value: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["type"], json!("broker_request"));
        assert_eq!(value["operation"], json!("session.list"));
        assert_eq!(value["input"], json!({}));
        assert_eq!(broker.response_frame_type(), "broker_response");

        // Correlation IDs are unique and UUID-shaped across all builders.
        let other = SdkRequest::query("session.metadata", Value::Null);
        assert_ne!(query.correlation_id(), other.correlation_id());
        assert_eq!(query.correlation_id().len(), 36);
    }

    #[test]
    fn typed_requests_fail_closed_on_bad_shapes() {
        // Inputs must be objects (null normalizes to {}).
        let bad_input = SdkRequest::query("session.metadata", json!([1, 2]));
        assert_eq!(
            bad_input
                .encode()
                .unwrap_err()
                .downcast_ref::<SdkTransportError>()
                .unwrap()
                .reason(),
            "frame_rejected"
        );

        // Selector names must be bounded and well-formed.
        let bad_name = SdkRequest::query("has space", Value::Null);
        assert!(bad_name.encode().is_err());
        let long_name = SdkRequest::control("x".repeat(MAX_OPERATION_NAME_CHARS + 1), Value::Null);
        assert!(long_name.encode().is_err());

        // Frame selectors must match their family.
        let mismatched = SdkRequest {
            input: json!({}),
            ..SdkRequest::query("session.metadata", Value::Null)
        };
        let mismatched = SdkRequest {
            frame_type: "control_request",
            ..mismatched
        };
        assert!(mismatched.encode().is_err());

        // Oversized frames are rejected before serialization leaves the crate.
        let oversized = SdkRequest::query(
            "session.metadata",
            json!({ "pad": "x".repeat(ABSOLUTE_MAX_FRAME_BYTES) }),
        );
        assert!(oversized.encode().is_err());
    }

    #[test]
    fn parse_hello_requires_typed_bounded_identity() {
        let limits = SdkTransportLimits::default();
        let hello = parse_hello(
            r#"{"type":"hello","connectionId":"890c00fd-3800-4cef-b25b-2d3070aba530"}"#,
            &limits,
        )
        .unwrap();
        assert_eq!(
            hello.connection_id.as_deref(),
            Some("890c00fd-3800-4cef-b25b-2d3070aba530")
        );

        // Wrong frame type fails closed as invalid_hello...
        for wrong in [
            r#"{"type":"welcome","connectionId":"abc"}"#,
            r#"{"type":"server_hello","connectionId":"abc"}"#,
            r#"{"type":"query_response","id":"x"}"#,
            r#"{"method":"status.get"}"#,
            "not json",
            "[]",
        ] {
            let error = parse_hello(wrong, &limits).unwrap_err();
            assert_eq!(
                error.downcast_ref::<SdkTransportError>().unwrap().reason(),
                "invalid_hello",
                "{wrong}"
            );
        }

        // ...as does a missing, empty, oversized, or ill-formed identity.
        for bad_identity in [
            r#"{"type":"hello"}"#,
            r#"{"type":"hello","connectionId":""}"#,
            r#"{"type":"hello","connectionId":"has space"}"#,
            &format!(
                r#"{{"type":"hello","connectionId":"{}"}}"#,
                "x".repeat(MAX_CONNECTION_ID_CHARS + 1)
            ),
        ] {
            assert!(
                parse_hello(bad_identity, &limits).is_err(),
                "{bad_identity}"
            );
        }

        // Oversized hello frames are frame violations.
        let oversized_limits = SdkTransportLimits {
            max_frame_bytes: 32,
            ..SdkTransportLimits::default()
        };
        assert_eq!(
            parse_hello(
                r#"{"type":"hello","connectionId":"890c00fd-3800-4cef-b25b-2d3070aba530"}"#,
                &oversized_limits
            )
            .unwrap_err()
            .downcast_ref::<SdkTransportError>()
            .unwrap()
            .reason(),
            "frame_rejected"
        );
    }

    #[test]
    fn parse_response_enforces_frame_and_payload_bounds() {
        let limits = SdkTransportLimits {
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
            max_frame_bytes: 256,
            max_payload_bytes: 32,
            max_reconnect_attempts: 0,
        };
        let ok = r#"{"type":"query_response","id":"abc","ok":true,"result":{"k":1}}"#;
        let parsed = parse_response(ok, &limits).unwrap();
        assert_eq!(parsed.id.as_deref(), Some("abc"));
        assert_eq!(parsed.ok, Some(true));
        assert_eq!(parsed.frame_type, "query_response");

        let paged =
            r#"{"type":"query_response","id":"abc","ok":true,"page":{"items":[],"complete":true}}"#;
        assert!(parse_response(paged, &limits).is_ok());

        let errored = r#"{"type":"control_response","id":"e2","ok":false,"error":{"code":"unknown_operation","message":"no"}}"#;
        let parsed = parse_response(errored, &limits).unwrap();
        assert_eq!(parsed.ok, Some(false));
        assert_eq!(
            parsed.error.and_then(|error| error.code),
            Some("unknown_operation".to_string())
        );

        // Oversized frame text is rejected before parsing.
        let oversized = format!(
            "{{\"type\":\"query_response\",\"pad\":\"{}\"}}",
            "x".repeat(300)
        );
        assert_eq!(
            parse_response(&oversized, &limits)
                .unwrap_err()
                .downcast_ref::<SdkTransportError>()
                .unwrap()
                .reason(),
            "frame_rejected"
        );

        // Payload beyond bounds is rejected even when framed small.
        let big_result = format!(
            "{{\"type\":\"query_response\",\"result\":{{\"p\":\"{}\"}}}}",
            "y".repeat(40)
        );
        assert_eq!(
            parse_response(&big_result, &limits)
                .unwrap_err()
                .downcast_ref::<SdkTransportError>()
                .unwrap()
                .reason(),
            "frame_rejected"
        );
        let big_page = format!(
            "{{\"type\":\"query_response\",\"page\":{{\"p\":\"{}\"}}}}",
            "z".repeat(40)
        );
        assert!(parse_response(&big_page, &limits).is_err());

        // Non-JSON frames are rejected.
        assert!(parse_response("hello world", &limits).is_err());
    }

    #[test]
    fn sanitize_error_code_bounds_server_values() {
        assert_eq!(
            sanitize_error_code(Some("operation_not_session_owned")),
            Some("operation_not_session_owned".to_string())
        );
        assert_eq!(
            sanitize_error_code(Some("bad code; DROP TABLE")),
            Some("badcodeDROPTABLE".to_string())
        );
        assert_eq!(
            sanitize_error_code(Some(&"x".repeat(MAX_OPERATION_NAME_CHARS + 10))),
            Some("x".repeat(MAX_OPERATION_NAME_CHARS))
        );
        assert_eq!(sanitize_error_code(Some("!!!")), None);
        assert_eq!(sanitize_error_code(None), None);
    }

    #[test]
    fn limits_are_sanitized_into_safe_bounds() {
        let sanitized = SdkTransportLimits {
            connect_timeout: Duration::from_millis(0),
            request_timeout: Duration::from_millis(0),
            max_frame_bytes: 1,
            max_payload_bytes: ABSOLUTE_MAX_FRAME_BYTES * 10,
            max_reconnect_attempts: 999,
        }
        .sanitized();
        assert_eq!(sanitized.connect_timeout, Duration::from_millis(100));
        assert_eq!(sanitized.request_timeout, Duration::from_millis(100));
        assert_eq!(sanitized.max_frame_bytes, 1024);
        assert_eq!(sanitized.max_payload_bytes, ABSOLUTE_MAX_FRAME_BYTES);
        assert_eq!(sanitized.max_reconnect_attempts, 5);
    }

    #[tokio::test]
    async fn authenticated_url_appends_token_query_once() {
        let metadata =
            parse_metadata(metadata_json("ws://127.0.0.1:43065/", "tokvalue123", None).as_bytes())
                .unwrap();
        let url = metadata.authenticated_url().unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert!(
            url.query()
                .is_some_and(|query| query == "token=tokvalue123")
        );
    }

    // -----------------------------------------------------------------------
    // Focused v3 transport fixtures over a real in-process loopback server.
    //
    // The black-box CLI fixtures (tests/gjc_sdk_transport.rs) exercise the
    // query path through `gjc inspect --probe`; these white-box fixtures cover
    // the full hello gate, pre-hello ordering, and the control/broker frame
    // families on the shared client.
    // -----------------------------------------------------------------------

    mod loopback {
        use super::*;
        use std::net::{IpAddr, Ipv4Addr};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use tokio::net::{TcpListener, TcpStream};
        use tokio_tungstenite::accept_hdr_async;
        use tokio_tungstenite::tungstenite::Utf8Bytes;
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

        pub(super) struct Fixture {
            pub metadata: EndpointMetadata,
            /// True when any client frame reached the server before its hello.
            pub prehello_violation: Arc<AtomicBool>,
            /// Number of accepted connections (reconnect identity tracking).
            pub connections: Arc<AtomicUsize>,
        }

        fn fixture_metadata(port: u16) -> EndpointMetadata {
            let body = format!(
                "{{\"version\":1,\"sessionId\":\"{SESSION_ID}\",\"url\":\"ws://127.0.0.1:{port}/\",\"token\":\"tok_fixture_v3\",\"pid\":{}}}",
                std::process::id()
            );
            parse_metadata(body.as_bytes()).unwrap()
        }

        /// Start one fixture server; `mode` selects the wire behavior.
        pub(super) async fn spawn(mode: &'static str) -> Fixture {
            let listener = TcpListener::bind((IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .unwrap();
            let port = listener.local_addr().unwrap().port();
            let prehello_violation = Arc::new(AtomicBool::new(false));
            let connections = Arc::new(AtomicUsize::new(0));
            let violation = Arc::clone(&prehello_violation);
            let conn_counter = Arc::clone(&connections);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let violation = Arc::clone(&violation);
                    let conn_counter = Arc::clone(&conn_counter);
                    tokio::spawn(async move {
                        handle_connection(stream, mode, violation, conn_counter).await;
                    });
                }
            });
            Fixture {
                metadata: fixture_metadata(port),
                prehello_violation,
                connections,
            }
        }

        #[allow(clippy::result_large_err)] // fixture-only 401 rejection path
        async fn handle_connection(
            stream: TcpStream,
            mode: &'static str,
            prehello_violation: Arc<AtomicBool>,
            connections: Arc<AtomicUsize>,
        ) {
            let token = "tok_fixture_v3".to_string();
            let accepted = accept_hdr_async(stream, |request: &Request, response: Response| {
                let authenticated = request
                    .uri()
                    .query()
                    .and_then(|query| {
                        query.split('&').find_map(|pair| {
                            let (key, value) = pair.split_once('=')?;
                            (key == "token").then(|| value.to_string())
                        })
                    })
                    .is_some_and(|value| value == token);
                if !authenticated {
                    return Err(Response::builder()
                        .status(401)
                        .body::<Option<String>>(None)
                        .expect("static unauthorized response"));
                }
                Ok(response)
            })
            .await;
            let Ok(socket) = accepted else {
                return;
            };
            let connection_index = connections.fetch_add(1, Ordering::SeqCst);
            let (mut writer, mut reader) = socket.split();
            let connection_id = format!("conn-{connection_index}");

            if mode == "wrong-hello" {
                // Violate the handshake contract with a non-hello first frame.
                let _ = writer
                    .send(Message::Text(Utf8Bytes::from(
                        r#"{"type":"welcome","connectionId":"not-a-hello"}"#,
                    )))
                    .await;
                while let Some(Ok(_)) = reader.next().await {}
                return;
            }

            if mode == "prehello" {
                // Prove ordering: a correct client sends nothing until the
                // hello lands, so this bounded read must time out empty.
                let early = tokio::time::timeout(Duration::from_millis(300), reader.next()).await;
                if let Ok(Some(Ok(_))) = early {
                    prehello_violation.store(true, Ordering::SeqCst);
                }
            }

            let hello = format!(r#"{{"type":"hello","connectionId":"{connection_id}"}}"#);
            if writer
                .send(Message::Text(Utf8Bytes::from(hello)))
                .await
                .is_err()
            {
                return;
            }

            while let Some(Ok(message)) = reader.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let Ok(value) = serde_json::from_slice::<Value>(text.as_bytes()) else {
                    continue;
                };
                if mode == "close-first" && connection_index == 0 {
                    // Drop without answering so the client must reconnect and
                    // re-authenticate against a fresh identity.
                    return;
                }
                let response_type = match value.get("type").and_then(Value::as_str) {
                    Some("control_request") => "control_response",
                    Some("broker_request") => "broker_response",
                    _ => "query_response",
                };
                let mut response = if mode == "app-error" {
                    json!({
                        "type": response_type,
                        "id": value.get("id").cloned().unwrap_or(Value::Null),
                        "ok": false,
                        "error": {
                            "code": "operation_not_session_owned",
                            "message": "fixture error surface",
                        },
                    })
                } else {
                    json!({
                        "type": response_type,
                        "id": value.get("id").cloned().unwrap_or(Value::Null),
                        "ok": true,
                        "result": {"echo": value.get("query").or_else(|| value.get("operation")).cloned().unwrap_or(Value::Null)},
                    })
                };
                if mode == "paged" && response_type == "query_response" {
                    response["page"] = json!({"items": [{"n": 1}], "complete": true});
                }
                if writer
                    .send(Message::Text(Utf8Bytes::from(
                        serde_json::to_string(&response).unwrap(),
                    )))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }

        pub(super) fn probe_limits_short() -> SdkTransportLimits {
            SdkTransportLimits {
                connect_timeout: Duration::from_secs(2),
                request_timeout: Duration::from_secs(2),
                ..SdkTransportLimits::default()
            }
        }
    }

    #[tokio::test]
    async fn client_round_trips_all_typed_families_over_one_hello_gated_connection() {
        let fixture = loopback::spawn("standard").await;
        let mut client =
            SdkClient::new(fixture.metadata).with_limits(loopback::probe_limits_short());
        let hello = client.connect().await.unwrap();
        assert_eq!(hello.connection_id.as_deref(), Some("conn-0"));

        for request in [
            SdkRequest::query("session.metadata", Value::Null),
            SdkRequest::control("session.rename", json!({"name": "probe"})),
            SdkRequest::broker("session.list", Value::Null),
        ] {
            let correlation_id = request.correlation_id().to_string();
            let expected_family = request.response_frame_type();
            let response = client.request(&request).await.unwrap();
            assert_eq!(response.frame_type, expected_family);
            assert_eq!(response.id.as_deref(), Some(correlation_id.as_str()));
            assert_eq!(response.ok, Some(true));
        }
        // Every exchange reused the single authenticated connection.
        assert_eq!(fixture.connections.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn client_surfaces_paged_query_and_application_error_codes() {
        for (mode, expect_ok, expect_code) in [
            ("paged", true, None),
            (
                "app-error",
                false,
                Some("operation_not_session_owned".to_string()),
            ),
        ] {
            let fixture = loopback::spawn(mode).await;
            let mut client =
                SdkClient::new(fixture.metadata).with_limits(loopback::probe_limits_short());
            let hello = client.connect().await.unwrap();
            assert_eq!(hello.connection_id.as_deref(), Some("conn-0"));
            let request = SdkRequest::query("session.metadata", Value::Null);
            let correlation_id = request.correlation_id().to_string();
            let response = client.request(&request).await.unwrap();
            assert_eq!(response.id.as_deref(), Some(correlation_id.as_str()));
            assert_eq!(response.ok, Some(expect_ok));
            assert_eq!(
                response.error.and_then(|error| error.code),
                expect_code,
                "{mode}"
            );
        }
    }

    #[tokio::test]
    async fn client_fails_closed_on_wrong_hello_type() {
        let fixture = loopback::spawn("wrong-hello").await;
        let mut client =
            SdkClient::new(fixture.metadata).with_limits(loopback::probe_limits_short());
        let error = client.connect().await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<SdkTransportError>().unwrap().reason(),
            "invalid_hello"
        );
    }

    #[tokio::test]
    async fn client_never_sends_a_frame_before_the_hello_gate() {
        let fixture = loopback::spawn("prehello").await;
        let mut client =
            SdkClient::new(fixture.metadata).with_limits(loopback::probe_limits_short());
        let hello = client.connect().await.unwrap();
        assert_eq!(hello.connection_id.as_deref(), Some("conn-0"));
        let response = client
            .request(&SdkRequest::query("session.metadata", Value::Null))
            .await
            .unwrap();
        assert_eq!(response.ok, Some(true));
        assert!(
            !fixture.prehello_violation.load(Ordering::SeqCst),
            "client transmitted before the server hello"
        );
    }

    #[tokio::test]
    async fn client_reauthenticates_with_new_identity_after_mid_exchange_loss() {
        let fixture = loopback::spawn("close-first").await;
        let mut client = SdkClient::new(fixture.metadata).with_limits(SdkTransportLimits {
            max_reconnect_attempts: 3,
            ..loopback::probe_limits_short()
        });
        let first_hello = client.connect().await.unwrap();
        assert_eq!(first_hello.connection_id.as_deref(), Some("conn-0"));

        // The first connection drops mid-exchange; the bounded retry must
        // re-handshake (new hello identity) and still complete the request.
        let request = SdkRequest::query("session.metadata", Value::Null);
        let correlation_id = request.correlation_id().to_string();
        let response = client.request(&request).await.unwrap();
        assert_eq!(response.id.as_deref(), Some(correlation_id.as_str()));
        assert_eq!(response.ok, Some(true));
        assert_eq!(fixture.connections.load(Ordering::SeqCst), 2);
        let second_hello = client.hello().and_then(|hello| hello.connection_id.clone());
        assert_eq!(second_hello.as_deref(), Some("conn-1"));
    }
}
