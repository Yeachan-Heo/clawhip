//! Adapter binding the #323 control plane to the landed #322 transport
//! (`crate::gjc_sdk`). Discovery is lane/worktree-scoped; every exchange
//! maps the transport taxonomy onto the control-plane error taxonomy.

use std::path::Path;

use async_trait::async_trait;

use super::model::{
    GjcError, GjcRequest, GjcResponse, GjcResult, GjcTransport,
};
use crate::gjc_sdk::{
    self, Discovery, EndpointMetadata, SdkClient, SdkRequest, SdkTransportError, StateRoot,
};

/// Control-plane transport backed by the landed SDK websocket client.
///
/// Each round trip opens one authenticated connection through
/// [`SdkClient::request`]; reconnects, frame bounds, and correlation are
/// enforced by the #322 transport itself.
#[derive(Debug)]
pub struct SdkEndpointTransport {
    metadata: EndpointMetadata,
}

impl SdkEndpointTransport {
    pub fn new(metadata: EndpointMetadata) -> Self {
        Self { metadata }
    }

    /// Public-safe session identifier this endpoint is bound to.
    pub fn session_id(&self) -> &str {
        self.metadata.session_id()
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
            capability: "endpoint".into(),
        },
        SdkTransportError::FrameRejected => GjcError::InvalidPeerReply {
            method: method.into(),
            reason: "frame rejected by transport bounds".into(),
        },
        SdkTransportError::EndpointMalformed => GjcError::InvalidPeerReply {
            method: method.into(),
            reason: "endpoint metadata malformed".into(),
        },
        SdkTransportError::EndpointUnavailable
        | SdkTransportError::ConnectionClosed
        | SdkTransportError::RetryExhausted => GjcError::TransportUnavailable,
    }
}

#[async_trait]
impl GjcTransport for SdkEndpointTransport {
    async fn round_trip(
        &self,
        request: GjcRequest,
    ) -> std::result::Result<GjcResponse, GjcError> {
        let sdk_request = SdkRequest::new(request.method.clone(), request.params.clone());
        let mut client = SdkClient::new(self.metadata.clone());
        let reply = client
            .request(&sdk_request)
            .await
            .map_err(|error| map_transport_error(&request.method, &error))?;

        // Defense in depth: the transport already enforces correlation;
        // re-check before trusting the payload.
        if reply.id.as_deref() != Some(request.correlation_id.as_str()) {
            return Err(GjcError::AmbiguousAck {
                method: request.method.clone(),
            });
        }
        if reply.ok == Some(false) {
            let reason = reply
                .error
                .and_then(|error| error.code)
                .unwrap_or_else(|| "unknown".into());
            return Err(GjcError::InvalidPeerReply {
                method: request.method.clone(),
                reason: format!("peer rejected request: {reason}"),
            });
        }
        Ok(GjcResponse {
            correlation_id: request.correlation_id,
            result: reply.payload,
        })
    }
}

/// Resolve the live endpoint for one lane/worktree without scanning any
/// unrelated root. Outcomes map directly onto control-plane failures so the
/// daemon never guesses.
pub fn discover_endpoint(worktree: &Path) -> GjcResult<SdkEndpointTransport> {
    let discovery =
        gjc_sdk::discover(&StateRoot::for_worktree(worktree)).map_err(|error| {
            map_transport_error("endpoint.discover", &error)
        })?;
    match discovery {
        Discovery::Live(metadata) => Ok(SdkEndpointTransport::new(metadata)),
        Discovery::Stale { .. } => Err(GjcError::StaleEndpoint {
            capability: "endpoint".into(),
        }),
        Discovery::Malformed | Discovery::NoMetadata => Err(GjcError::TransportUnavailable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_metadata(worktree: &Path, url: &str, token: &str) -> std::path::PathBuf {
        let sdk_dir = worktree.join(".gjc").join("state").join("sdk");
        std::fs::create_dir_all(&sdk_dir).unwrap();
        let path = sdk_dir.join("01a02ccd-cb43-7570-8ce2-98b3b67ed2ef.json");
        std::fs::write(
            &path,
            json!({
                "sessionId": "sess-lane-1",
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

        // Malformed layout (file instead of directory) -> transport_unavailable.
        std::fs::create_dir_all(temp.path().join(".gjc")).unwrap();
        std::fs::write(temp.path().join(".gjc").join("state"), "not-a-dir").unwrap();
        let error = discover_endpoint(temp.path()).unwrap_err();
        assert_eq!(error.error_code(), "transport_unavailable");

        // Live metadata binds to its recorded session.
        std::fs::remove_file(temp.path().join(".gjc").join("state")).unwrap();
        std::fs::create_dir_all(temp.path().join(".gjc").join("state")).unwrap();
        write_metadata(temp.path(), "ws://127.0.0.1:1/", "tok-1");
        let transport = discover_endpoint(temp.path()).unwrap();
        assert_eq!(transport.session_id(), "sess-lane-1");

        // Dead pid recorded -> stale endpoint.
        let sdk_dir = temp.path().join(".gjc").join("state").join("sdk");
        let stale_path = sdk_dir.join("ffffffff-ffff-ffff-ffff-ffffffffffff.json");
        std::fs::write(
            &stale_path,
            json!({
                "sessionId": "sess-dead",
                "url": "ws://127.0.0.1:1/",
                "token": "tok-1",
                "pid": 0xFFFF_FFF1u32,
            })
            .to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stale_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let newer = sdk_dir.join("01a02ccd-cb43-7570-8ce2-98b3b67ed2ef.json");
        let _ = std::fs::remove_file(&newer);
        std::fs::write(
            &newer,
            json!({
                "sessionId": "sess-dead",
                "url": "ws://127.0.0.1:1/",
                "token": "tok-1",
                "pid": 0xFFFF_FFF1u32,
            })
            .to_string(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&newer, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let error = discover_endpoint(temp.path()).unwrap_err();
        assert_eq!(error.error_code(), "stale_endpoint");
    }

    #[tokio::test]
    async fn unreachable_endpoint_maps_to_transport_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".gjc").join("state").join("sdk")).unwrap();
        write_metadata(temp.path(), "ws://127.0.0.1:1/", "tok-1");
        let transport = discover_endpoint(temp.path()).unwrap();
        let error = transport
            .round_trip(GjcRequest::new("corr-1", "session.get", json!({})))
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "transport_unavailable");
    }
}
