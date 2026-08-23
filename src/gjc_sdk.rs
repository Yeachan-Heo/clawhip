//! Safe worktree-local GJC SDK endpoint discovery and typed websocket transport.
//!
//! This module owns issue #322's reusable transport layer:
//!
//! - [`discover`] reads `<worktree>/.gjc/state/sdk/*.json` session endpoint
//!   metadata for one registered lane/worktree, never scanning unrelated
//!   roots, and never exposing tokens through errors or debug output.
//! - [`SdkClient`] performs authenticated (`?token=` query parameter), framed,
//!   bounded websocket requests against the discovered loopback endpoint with
//!   correlation IDs, bounded timeouts, bounded frame/output sizes, a typed
//!   error taxonomy, and bounded reconnect.
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
}

impl std::fmt::Debug for EndpointMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointMetadata")
            .field("session_id", &self.session_id)
            .field("url", &"<redacted>")
            .field("token", &"<redacted>")
            .field("pid", &self.pid)
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

    /// Authenticated websocket URL with the token applied as a query param.
    fn authenticated_url(&self) -> Result<Url> {
        let mut url = Url::parse(&self.url).map_err(|_| SdkTransportError::EndpointMalformed)?;
        url.query_pairs_mut().append_pair("token", &self.token);
        Ok(url)
    }
}

/// Raw metadata file schema (`<state-root>/sdk/<session-id>.json`).
///
/// GJC writes `sessionId` (camelCase); the snake_case alias is accepted for
/// hand-authored fixtures.
#[derive(Deserialize)]
struct RawEndpointMetadata {
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: String,
    url: String,
    token: String,
    #[serde(default)]
    pid: Option<u32>,
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

fn parse_metadata(contents: &[u8]) -> Result<EndpointMetadata> {
    let raw: RawEndpointMetadata =
        serde_json::from_slice(contents).map_err(|_| SdkTransportError::EndpointMalformed)?;
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
    })
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
    /// Metadata exists but no owning process is alive.
    Stale {
        /// Public-safe session identifier of the stale metadata.
        session_id: String,
    },
    /// Metadata files exist but none survived validation.
    Malformed,
}

/// Discover endpoint metadata for one registered lane/worktree.
///
/// Only `*.json` files directly inside `<state-root>/sdk` are considered —
/// unrelated roots are never scanned. The most recently modified valid
/// metadata file wins; liveness of its recorded pid is checked so dead
/// sessions surface as [`Discovery::Stale`]. Outcome precedence is
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
        let Ok(metadata) = parse_metadata(&contents) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if metadata.pid().is_some_and(|pid| !process_alive(pid)) {
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

/// Typed request envelope sent to the SDK endpoint.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SdkRequest {
    /// Correlation ID echoed by the server on the matching response.
    pub id: String,
    /// Frame discriminator; always `request` for client frames.
    #[serde(rename = "type")]
    pub frame_type: &'static str,
    /// SDK method name.
    pub method: String,
    /// Method parameters.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: serde_json::Value,
}

impl SdkRequest {
    /// Build a request with a fresh correlation ID.
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            id: new_correlation_id(),
            frame_type: "request",
            method: method.into(),
            params,
        }
    }

    /// Correlation ID of this request.
    pub fn correlation_id(&self) -> &str {
        &self.id
    }

    fn encode(&self) -> Result<String> {
        let frame = serde_json::to_string(self).map_err(|_| SdkTransportError::FrameRejected)?;
        if frame.len() > ABSOLUTE_MAX_FRAME_BYTES {
            return Err(SdkTransportError::FrameRejected.into());
        }
        Ok(frame)
    }
}

/// Typed response envelope received from the SDK endpoint.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SdkResponse {
    /// Correlation ID matching the request this responds to.
    pub id: Option<String>,
    /// Frame discriminator from the server (`response`, `hello`, `error`...).
    #[serde(rename = "type")]
    pub frame_type: String,
    /// Server-reported success flag when present.
    #[serde(default)]
    pub ok: Option<bool>,
    /// Response payload.
    #[serde(default)]
    pub payload: Value,
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

/// Hello frame emitted by the server after authentication.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct HelloFrame {
    /// Server-assigned connection identifier.
    #[serde(default, rename = "connectionId")]
    pub connection_id: Option<String>,
}

fn parse_response(frame: &str, limits: &SdkTransportLimits) -> Result<SdkResponse> {
    if frame.len() > limits.max_frame_bytes {
        return Err(SdkTransportError::FrameRejected.into());
    }
    let response: SdkResponse =
        serde_json::from_str(frame).map_err(|_| SdkTransportError::FrameRejected)?;
    if serde_json::to_string(&response.payload)
        .is_ok_and(|encoded| encoded.len() > limits.max_payload_bytes)
    {
        return Err(SdkTransportError::FrameRejected.into());
    }
    Ok(response)
}

/// Authenticated, typed websocket client for one SDK endpoint.
pub struct SdkClient {
    metadata: EndpointMetadata,
    limits: SdkTransportLimits,
}

impl SdkClient {
    /// Build a client over validated endpoint metadata.
    pub fn new(metadata: EndpointMetadata) -> Self {
        Self {
            metadata,
            limits: SdkTransportLimits::default(),
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

    /// Open an authenticated connection and return the server `hello` frame.
    pub async fn connect(&mut self) -> Result<HelloFrame> {
        let mut stream = self.connect_once().await?;
        let hello = tokio::time::timeout(self.limits.connect_timeout, stream.next()).await;
        let Some(hello) = hello.map_err(|_| SdkTransportError::Timeout)? else {
            return Err(SdkTransportError::ConnectionClosed.into());
        };
        let message = hello.map_err(|_| SdkTransportError::ConnectionClosed)?;
        let Message::Text(text) = message else {
            return Err(SdkTransportError::FrameRejected.into());
        };
        if text.len() > self.limits.max_frame_bytes {
            return Err(SdkTransportError::FrameRejected.into());
        }
        serde_json::from_str::<HelloFrame>(&text)
            .map_err(|_| SdkTransportError::FrameRejected)
            .map_err(Into::into)
    }

    /// Send one typed request and await its correlated response.
    ///
    /// Each attempt opens one fresh authenticated connection. Connect failures
    /// are terminal for the call; a connection lost mid-exchange is retried up
    /// to `max_reconnect_attempts` times before surfacing
    /// [`SdkTransportError::RetryExhausted`].
    pub async fn request(&mut self, request: &SdkRequest) -> Result<SdkResponse> {
        let frame = request.encode()?;
        let mut attempts = 0u64;
        loop {
            attempts = attempts.saturating_add(1);
            let stream = self.connect_once().await?;
            let mut stream = stream;
            match exchange(&mut stream, request, &frame, &self.limits).await {
                Ok(response) => return Ok(response),
                Err(error)
                    if attempts <= self.limits.max_reconnect_attempts
                        && matches!(
                            error.downcast_ref::<SdkTransportError>(),
                            Some(SdkTransportError::ConnectionClosed)
                        ) =>
                {
                    continue;
                }
                Err(error)
                    if matches!(
                        error.downcast_ref::<SdkTransportError>(),
                        Some(SdkTransportError::ConnectionClosed)
                    ) =>
                {
                    return Err(SdkTransportError::RetryExhausted.into());
                }
                Err(error) => return Err(error),
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
    // Bounded tolerance for id-less server frames (notifications) that cannot
    // be correlated to this request.
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
        match response.frame_type.as_str() {
            "hello" => continue,
            _ => match response.id.as_deref() {
                Some(id) if id == request.correlation_id() => return Ok(response),
                // A correlated frame for a different request is a protocol
                // violation on a single-request diagnostic transport.
                Some(_) => return Err(SdkTransportError::CorrelationMismatch.into()),
                None => {
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
}

impl ProbeReport {
    fn failed(reason: &'static str) -> Self {
        Self {
            hello_connection_id: None,
            hello_reason: Some(reason),
            request_ok: None,
            request_correlated: None,
            request_reason: None,
        }
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
    let connection_id = match client.connect().await {
        Ok(hello) => hello.connection_id,
        Err(error) => {
            let reason = error
                .downcast_ref::<SdkTransportError>()
                .map(|error: &SdkTransportError| error.reason())
                .unwrap_or("transport_failed");
            return ProbeReport::failed(reason);
        }
    };
    let request = SdkRequest::new("status.get", serde_json::Value::Null);
    let correlation_id = request.correlation_id().to_string();
    match client.request(&request).await {
        Ok(response) => ProbeReport {
            hello_connection_id: connection_id,
            hello_reason: None,
            request_ok: Some(response.ok.unwrap_or(true)),
            request_correlated: Some(response.id.as_deref() == Some(correlation_id.as_str())),
            request_reason: None,
        },
        Err(error) => {
            let reason = error
                .downcast_ref::<SdkTransportError>()
                .map(|error: &SdkTransportError| error.reason())
                .unwrap_or("transport_failed");
            ProbeReport {
                hello_connection_id: connection_id,
                hello_reason: None,
                request_ok: Some(false),
                request_correlated: None,
                request_reason: Some(reason),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn token_and_session_id_charset_bounds() {
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
    }

    #[test]
    fn parse_metadata_accepts_camel_case_and_snake_alias() {
        let camel = metadata_json("ws://127.0.0.1:43065/", "tok0ECXWhpn", Some(42));
        let parsed = parse_metadata(camel.as_bytes()).unwrap();
        assert_eq!(parsed.session_id(), SESSION_ID);
        assert_eq!(parsed.pid(), Some(42));

        let snake = "{\"session_id\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"url\":\"ws://127.0.0.1:1/\",\"token\":\"tok\"}";
        let parsed = parse_metadata(snake.as_bytes()).unwrap();
        assert_eq!(parsed.session_id(), SESSION_ID);
        assert_eq!(parsed.pid(), None);

        // Missing fields / wrong types are malformed.
        for bad in [
            "{}",
            "{\"sessionId\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"url\":\"ws://127.0.0.1:1/\"}",
            "{\"sessionId\":\"01a02ccd-c754-7656-95c7-f40b5a140bc3\",\"token\":\"tok\"}",
        ] {
            assert!(parse_metadata(bad.as_bytes()).is_err(), "{bad}");
        }
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
        write_metadata_at(
            &root,
            "01a00000-0000-0000-0000-000000000001.json",
            &metadata_json("ws://127.0.0.1:11111/", "tokold", Some(std::process::id())),
        );
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
    fn discover_stale_when_pid_dead() {
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
    fn request_envelope_encodes_typed_frame_with_correlation() {
        let request = SdkRequest::new("status.get", serde_json::Value::Null);
        assert_eq!(request.frame_type, "request");
        let encoded = request.encode().unwrap();
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["type"], "request");
        assert_eq!(value["method"], "status.get");
        assert_eq!(value["id"], serde_json::json!(request.correlation_id()));
        // Correlation IDs are unique and UUID-shaped.
        let other = SdkRequest::new("status.get", serde_json::Value::Null);
        assert_ne!(request.correlation_id(), other.correlation_id());
        assert_eq!(request.correlation_id().len(), 36);
    }

    #[test]
    fn parse_response_enforces_frame_and_payload_bounds() {
        let limits = SdkTransportLimits {
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
            max_frame_bytes: 64,
            max_payload_bytes: 32,
            max_reconnect_attempts: 0,
        };
        let ok = r#"{"type":"response","id":"abc","ok":true,"payload":{"k":1}}"#;
        let parsed = parse_response(ok, &limits).unwrap();
        assert_eq!(parsed.id.as_deref(), Some("abc"));
        assert_eq!(parsed.ok, Some(true));

        // Oversized frame text is rejected before parsing.
        let oversized = format!("{{\"type\":\"response\",\"pad\":\"{}\"}}", "x".repeat(80));
        assert_eq!(
            parse_response(&oversized, &limits)
                .unwrap_err()
                .downcast_ref::<SdkTransportError>()
                .unwrap()
                .reason(),
            "frame_rejected"
        );

        // Payload beyond bounds is rejected even when framed small.
        let big_payload = format!(
            "{{\"type\":\"response\",\"payload\":{{\"p\":\"{}\"}}}}",
            "y".repeat(40)
        );
        assert_eq!(
            parse_response(&big_payload, &limits)
                .unwrap_err()
                .downcast_ref::<SdkTransportError>()
                .unwrap()
                .reason(),
            "frame_rejected"
        );

        // Non-JSON frames are rejected.
        assert!(parse_response("hello world", &limits).is_err());
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
}
