//! Error type for the newt-mesh integration.
//!
//! Wraps the underlying agent-mesh bus error so callers don't have to
//! pull in transport types just to match on a failure shape.

use std::time::Duration;

use thiserror::Error;

/// Errors surfaced by the newt-mesh integration crate.
#[derive(Debug, Error)]
pub enum MeshIntegrationError {
    /// JSON encode/decode failure on the wire types.
    #[error("encode/decode: {0}")]
    Codec(#[from] serde_json::Error),

    /// Underlying bus failure — peer unreachable, timeout, transport
    /// error, etc.
    #[error("bus: {0}")]
    Bus(#[from] agent_mesh_bus::BusError),

    /// The backend the responder dispatched to returned an error. The
    /// string is the backend's own anyhow chain rendered for logs.
    #[error("backend failure: {0}")]
    Backend(String),

    /// The peer is reachable but its reply could not be parsed as a
    /// well-formed [`crate::InferenceReply`].
    #[error("malformed reply from peer: {0}")]
    MalformedReply(String),

    /// A timeout configured at the integration layer fired before the
    /// underlying bus call returned. (The bus has its own timeout that
    /// surfaces as [`Self::Bus`]; this one wraps caller-supplied
    /// integration-level deadlines.)
    #[error("integration timed out after {0:?}")]
    Timeout(Duration),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, MeshIntegrationError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_error_renders() {
        let bad: std::result::Result<serde_json::Value, _> = serde_json::from_str("not-json");
        let e: MeshIntegrationError = bad.unwrap_err().into();
        assert!(format!("{e}").contains("encode/decode"));
    }

    #[test]
    fn backend_error_carries_message() {
        let e = MeshIntegrationError::Backend("ollama 500".into());
        assert!(format!("{e}").contains("ollama 500"));
    }

    #[test]
    fn malformed_reply_carries_message() {
        let e = MeshIntegrationError::MalformedReply("missing model_id".into());
        assert!(format!("{e}").contains("missing model_id"));
    }

    #[test]
    fn timeout_renders_duration() {
        let e = MeshIntegrationError::Timeout(Duration::from_secs(7));
        let msg = format!("{e}");
        assert!(msg.contains('7'));
        assert!(msg.contains("timed out"));
    }
}
