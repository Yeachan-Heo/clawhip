//! Adapter binding the #323 control plane to the landed #322 transport
//! (`crate::gjc_sdk`). Discovery is lane/worktree-scoped; every exchange
//! maps the transport taxonomy onto the control-plane error taxonomy.

use std::path::Path;

use async_trait::async_trait;
use serde_json::Value;

use super::model::{GjcError, GjcRequest, GjcResponse, GjcResult, GjcTransport};
use crate::gjc_sdk::{
    self, Discovery, EndpointMetadata, SdkClient, SdkRequest, SdkTransportError,
    SdkTransportLimits, StateRoot,
};

/// Control-plane transport backed by the landed SDK websocket client.
///
/// Each round trip opens one authenticated connection through
/// [`SdkClient::request`]; reconnects, frame bounds, and correlation are
/// enforced by the #322 transport itself.
#[derive(Debug)]
pub struct SdkEndpointTransport {
    metadata: EndpointMetadata,
    /// Discovery root used to revalidate the endpoint lease around mutating
    /// exchanges. Static construction remains available for unit-test seams;
    /// production discovery always supplies this root.
    state_root: Option<StateRoot>,
}

impl SdkEndpointTransport {
    #[allow(dead_code)]
    pub fn new(metadata: EndpointMetadata) -> Self {
        Self {
            metadata,
            state_root: None,
        }
    }

    fn from_discovery(state_root: &StateRoot, metadata: EndpointMetadata) -> Self {
        Self {
            metadata,
            state_root: Some(state_root.clone()),
        }
    }

    /// Public-safe session identifier this endpoint is bound to.
    pub fn session_id(&self) -> &str {
        self.metadata.session_id()
    }

    pub fn endpoint_generation(&self) -> u64 {
        self.metadata.generation()
    }

    /// Re-read the enrolled endpoint metadata and reject any rotation or
    /// identity change before trusting a mutating exchange. The metadata
    /// itself is the lease: a changed generation means the old transport is
    /// no longer an unambiguous peer for this command.
    fn validate_lease(&self) -> GjcResult<()> {
        let Some(state_root) = self.state_root.as_ref() else {
            return Ok(());
        };
        let current = gjc_sdk::discover_all(state_root)
            .map_err(|error| map_transport_error("endpoint.validate", &error))?
            .into_iter()
            .find(|metadata| metadata.session_id() == self.session_id());
        let Some(current) = current else {
            return Err(GjcError::StaleEndpoint {
                capability: crate::gjc::model::CAP_ENDPOINT.into(),
            });
        };
        if current.generation() != self.endpoint_generation() {
            return Err(GjcError::StaleEndpoint {
                capability: crate::gjc::model::CAP_ENDPOINT.into(),
            });
        }
        Ok(())
    }

    fn validate_request_session(&self, request: &GjcRequest) -> GjcResult<()> {
        let Some(requested) = request.params.get("session_id").and_then(Value::as_str) else {
            return Err(GjcError::InvalidPeerReply {
                method: request.method.clone(),
                reason: "mutating request session identity is missing".into(),
            });
        };
        if requested != self.session_id() {
            return Err(GjcError::SessionMismatch {
                expected: self.session_id().to_string(),
            });
        }
        Ok(())
    }
}

fn map_transport_error(method: &str, error: &crate::DynError) -> GjcError {
    let Some(sdk_error) = error.downcast_ref::<SdkTransportError>() else {
        return GjcError::TransportUnavailable;
    };
    match sdk_error {
        SdkTransportError::Timeout => GjcError::Timeout {
            method: method.into(),
        },
        SdkTransportError::CorrelationMismatch => GjcError::AmbiguousAck {
            method: method.into(),
        },
        SdkTransportError::EndpointUnauthorized => GjcError::StaleEndpoint {
            capability: crate::gjc::model::CAP_ENDPOINT.into(),
        },
        SdkTransportError::EndpointMalformed => GjcError::InvalidPeerReply {
            method: method.into(),
            reason: "endpoint metadata malformed".into(),
        },
        SdkTransportError::FrameRejected | SdkTransportError::InvalidHello => {
            GjcError::InvalidPeerReply {
                method: method.into(),
                reason: "frame rejected by transport bounds".into(),
            }
        }
        SdkTransportError::EndpointUnavailable
        | SdkTransportError::ConnectionClosed
        | SdkTransportError::RetryExhausted => GjcError::TransportUnavailable,
    }
}

#[async_trait]
impl GjcTransport for SdkEndpointTransport {
    fn endpoint_generation(&self) -> Option<u64> {
        Some(self.endpoint_generation())
    }

    async fn round_trip(&self, request: GjcRequest) -> std::result::Result<GjcResponse, GjcError> {
        let mutating = request.method.starts_with("control.");
        // The lease must still describe the enrolled session immediately
        // before opening the authenticated connection. A stale command stays
        // reserved and is never replayed against a replacement endpoint.
        if mutating {
            self.validate_request_session(&request)?;
            self.validate_lease()?;
        }
        // v3 wire mapping: `control.*` methods become control_request
        // frames; every other method is a query_request. The correlation ID
        // is minted inside the typed frame and echoed by the peer.
        let mut sdk_request = if let Some(operation) = request.method.strip_prefix("control.") {
            SdkRequest::control(operation, request.params.clone())
        } else {
            SdkRequest::query(request.method.clone(), request.params.clone())
        };
        // The caller owns correlation identity: `GjcControlPlane::round_trip`
        // validates `reply.correlation_id == request.correlation_id`, so the
        // wire frame must carry the caller's id verbatim instead of the
        // transport-minted default.
        sdk_request.id = request.correlation_id.clone();
        let correlation_id = sdk_request.correlation_id().to_string();
        // The caller's bounded budget is clamped into the transport limits
        // so `timeout_ms` is authoritative for this exchange (sanitized()
        // bounds it to [100ms, MAX_TRANSPORT_TIMEOUT]).
        let limits = SdkTransportLimits {
            request_timeout: std::time::Duration::from_millis(request.timeout_ms),
            ..SdkTransportLimits::default()
        };
        let mut client = SdkClient::new(self.metadata.clone()).with_limits(limits);
        let reply = client
            .request(&sdk_request)
            .await
            .map_err(|error| map_transport_error(&request.method, &error))?;

        // A peer may rotate its metadata while the exchange is in flight. Do
        // not accept an otherwise valid ack from an endpoint whose identity
        // changed during this non-idempotent operation.
        if mutating {
            self.validate_lease()?;
        }

        // Defense in depth: the transport already enforces correlation;
        // re-check before trusting the payload.
        if reply.id.as_deref() != Some(correlation_id.as_str()) {
            return Err(GjcError::AmbiguousAck {
                method: request.method.clone(),
            });
        }
        if reply.ok == Some(false) {
            let reason = reply
                .error
                .and_then(|error| error.code)
                .unwrap_or_else(|| "unknown".into());
            if reason == "session_not_found" {
                return Err(GjcError::SessionNotFound {
                    session_id: request
                        .params
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                });
            }
            return Err(GjcError::InvalidPeerReply {
                method: request.method.clone(),
                reason: format!("peer rejected request: {reason}"),
            });
        }
        Ok(GjcResponse {
            correlation_id,
            result: reply.result,
        })
    }
}

/// Resolve the live endpoint for one lane/worktree without scanning any
/// unrelated root. Outcomes map directly onto control-plane failures so the
/// daemon never guesses.
pub fn discover_endpoint(worktree: &Path) -> GjcResult<SdkEndpointTransport> {
    let state_root = StateRoot::for_worktree(worktree);
    let discovery = gjc_sdk::discover(&state_root)
        .map_err(|error| map_transport_error("endpoint.discover", &error))?;
    match discovery {
        Discovery::Live(metadata) => {
            Ok(SdkEndpointTransport::from_discovery(&state_root, metadata))
        }
        Discovery::Stale { .. } => Err(GjcError::StaleEndpoint {
            capability: super::model::CAP_ENDPOINT.into(),
        }),
        Discovery::Malformed | Discovery::NoMetadata => Err(GjcError::TransportUnavailable),
    }
}

pub fn discover_endpoint_for_session(
    worktree: &Path,
    session_id: &str,
) -> GjcResult<SdkEndpointTransport> {
    let state_root = StateRoot::for_worktree(worktree);
    let metadata = gjc_sdk::discover_all(&state_root)
        .map_err(|error| map_transport_error("endpoint.discover", &error))?
        .into_iter()
        .find(|metadata| metadata.session_id() == session_id)
        .ok_or_else(|| GjcError::SessionNotFound {
            session_id: session_id.to_string(),
        })?;
    Ok(SdkEndpointTransport::from_discovery(&state_root, metadata))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_metadata(
        worktree: &Path,
        session_id: &str,
        url: &str,
        token: &str,
    ) -> std::path::PathBuf {
        let sdk_dir = worktree.join(".gjc").join("state").join("sdk");
        std::fs::create_dir_all(&sdk_dir).unwrap();
        // v3 contract: the file stem must equal the recorded session id.
        let path = sdk_dir.join(format!("{session_id}.json"));
        std::fs::write(
            &path,
            json!({
                "version": 1,
                "sessionId": session_id,
                "url": url,
                "token": token,
            })
            .to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    #[test]
    fn discovery_maps_live_stale_and_missing_outcomes() {
        let temp = tempfile::tempdir().unwrap();

        // No metadata at all -> transport_unavailable.
        let error = discover_endpoint(temp.path()).unwrap_err();
        assert_eq!(error.error_code(), "transport_unavailable");

        // Malformed layout (file instead of directory) -> the discovery
        // surface degrades this to a non-live outcome, mapped fail-closed.
        std::fs::create_dir_all(temp.path().join(".gjc")).unwrap();
        std::fs::write(temp.path().join(".gjc").join("state"), "not-a-dir").unwrap();
        let error = discover_endpoint(temp.path()).unwrap_err();
        assert_eq!(error.error_code(), "transport_unavailable");

        // Live metadata binds to its recorded session. The v3 contract
        // requires the metadata file stem to equal the session id.
        std::fs::remove_file(temp.path().join(".gjc").join("state")).unwrap();
        std::fs::create_dir_all(temp.path().join(".gjc").join("state")).unwrap();
        write_metadata(temp.path(), "sess-lane-1", "ws://127.0.0.1:1/", "tok-1");
        let transport = discover_endpoint(temp.path()).unwrap();
        assert_eq!(transport.session_id(), "sess-lane-1");

        // Dead pid recorded -> stale endpoint. Remove the live record so
        // the only remaining candidate is the dead-owner one.
        let sdk_dir = temp.path().join(".gjc").join("state").join("sdk");
        let _ = std::fs::remove_file(sdk_dir.join("sess-lane-1.json"));
        write_metadata(temp.path(), "sess-dead", "ws://127.0.0.1:1/", "tok-1");
        let dead_path = sdk_dir.join("sess-dead.json");
        let contents = std::fs::read_to_string(&dead_path).unwrap().replace(
            "\"token\":\"tok-1\"",
            &format!("\"token\":\"tok-1\",\"pid\":{}", 0xFFFF_FFF1u32),
        );
        std::fs::write(&dead_path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dead_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let error = discover_endpoint(temp.path()).unwrap_err();
        assert_eq!(error.error_code(), "stale_endpoint");
    }

    #[tokio::test]
    async fn round_trip_echoes_caller_correlation_id_verbatim() {
        use futures_util::{SinkExt, StreamExt};
        use tokio::net::TcpListener;

        // Loopback fixture: server hello, then echo the request id back on
        // the matching response family with an accepted verdict.
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio_tungstenite::tungstenite::Message;
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut sink, mut stream) = ws.split();
            sink.send(Message::text(
                r#"{"type":"hello","connectionId":"corr-fixture"}"#,
            ))
            .await
            .unwrap();
            if let Some(Ok(Message::Text(text))) = stream.next().await {
                let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                let reply = serde_json::json!({
                    "type": "control_response",
                    "id": value["id"],
                    "ok": true,
                    "result": {"accepted": true},
                });
                sink.send(Message::text(reply.to_string())).await.unwrap();
            }
        });

        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".gjc").join("state").join("sdk")).unwrap();
        write_metadata(
            temp.path(),
            "sess-lane-1",
            &format!("ws://{addr}/"),
            "tok-1",
        );
        let transport = discover_endpoint(temp.path()).unwrap();

        let reply = transport
            .round_trip(GjcRequest::new(
                "caller-corr-326",
                "control.prompt",
                json!({"session_id": "sess-lane-1"}),
                10_000,
            ))
            .await
            .expect("caller correlation id must be echoed verbatim");
        assert_eq!(reply.correlation_id, "caller-corr-326");
        assert_eq!(reply.result["accepted"], true);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn unreachable_endpoint_maps_to_transport_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".gjc").join("state").join("sdk")).unwrap();
        write_metadata(temp.path(), "sess-lane-1", "ws://127.0.0.1:1/", "tok-1");
        let transport = discover_endpoint(temp.path()).unwrap();
        let error = transport
            .round_trip(GjcRequest::new("corr-1", "session.get", json!({}), 10_000))
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "transport_unavailable");
    }

    #[tokio::test]
    async fn mutating_exchange_rejects_rotation_before_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_metadata(
            temp.path(),
            "sess-lane-1",
            "ws://127.0.0.1:1/",
            "tok-before",
        );
        let transport = discover_endpoint_for_session(temp.path(), "sess-lane-1").unwrap();

        // Resolve the endpoint first, then rotate its metadata before the
        // mutation reaches the websocket. The lease check must fail closed
        // without attempting a connection to either endpoint.
        std::fs::write(
            &path,
            json!({
                "version": 1,
                "sessionId": "sess-lane-1",
                "url": "ws://127.0.0.1:2/",
                "token": "tok-after",
            })
            .to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let error = transport
            .round_trip(GjcRequest::new(
                "rotation-corr",
                "control.prompt",
                json!({"session_id": "sess-lane-1"}),
                1_000,
            ))
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "stale_endpoint");
    }

    #[tokio::test]
    async fn mutating_exchange_rejects_foreign_session_before_dispatch() {
        let temp = tempfile::tempdir().unwrap();
        let _ = write_metadata(
            temp.path(),
            "sess-lane-1",
            "ws://127.0.0.1:1/",
            "tok-session",
        );
        let transport = discover_endpoint_for_session(temp.path(), "sess-lane-1").unwrap();
        let error = transport
            .round_trip(GjcRequest::new(
                "identity-corr",
                "control.prompt",
                json!({"session_id": "sess-foreign"}),
                1_000,
            ))
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "session_mismatch");
    }

    #[tokio::test]
    async fn mutating_exchange_rejects_rotation_before_acknowledgement() {
        use futures_util::{SinkExt, StreamExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let metadata = write_metadata(
            temp.path(),
            "sess-lane-1",
            &format!("ws://{addr}/"),
            "tok-before-ack",
        );
        let server_metadata = metadata.clone();
        let server = tokio::spawn(async move {
            use tokio_tungstenite::tungstenite::Message;
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut sink, mut stream) = ws.split();
            sink.send(Message::text(
                r#"{"type":"hello","connectionId":"ack-rotation"}"#,
            ))
            .await
            .unwrap();
            let Some(Ok(Message::Text(text))) = stream.next().await else {
                return;
            };
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            std::fs::write(
                &server_metadata,
                json!({
                    "version": 1,
                    "sessionId": "sess-lane-1",
                    "url": format!("ws://{addr}/"),
                    "token": "tok-after-ack",
                })
                .to_string(),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&server_metadata, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            sink.send(Message::text(
                json!({
                    "type": "control_response",
                    "id": value["id"],
                    "ok": true,
                    "result": {
                        "accepted": true,
                        "session_id": "sess-lane-1"
                    }
                })
                .to_string(),
            ))
            .await
            .unwrap();
        });

        let transport = discover_endpoint_for_session(temp.path(), "sess-lane-1").unwrap();
        let error = transport
            .round_trip(GjcRequest::new(
                "ack-rotation-corr",
                "control.prompt",
                json!({"session_id": "sess-lane-1"}),
                1_000,
            ))
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "stale_endpoint");
        server.await.unwrap();
    }
}
