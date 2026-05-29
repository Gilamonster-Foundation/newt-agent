//! Wire types for the newt-mesh inference protocol.
//!
//! The protocol is intentionally narrow:
//!
//! - [`InferenceRequest`] from caller to responder
//! - [`InferenceReply`] back from responder to caller
//!
//! Both flow as JSON-encoded bodies inside a `Request`/`Reply`
//! [`agent_mesh_bus::BusMessage`], so the signature, replay defense,
//! and correlation handling are all inherited from the bus.

use newt_core::router::Tier;
use serde::{Deserialize, Serialize};

/// Request sent from one newt to another, asking for inference.
///
/// The responder is free to choose which backend services the request
/// unless [`Self::model`] is set, in which case the responder must
/// either use that exact model or fail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InferenceRequest {
    /// User prompt — single-turn for v1; multi-turn chats are
    /// represented by encoding history into the prompt string.
    pub prompt: String,
    /// Optional tier hint. If `None`, the responder uses its own
    /// router/policy.
    pub tier: Option<Tier>,
    /// Optional model id pin. If `Some`, the responder must use this
    /// model or return an error.
    pub model: Option<String>,
    /// Max output tokens. Forwarded to the backend; backends that
    /// don't respect this just ignore it.
    pub max_tokens: Option<u32>,
}

/// Reply sent back from the responder.
///
/// `model_id` is mandatory on success replies: the drake patch-not-prose
/// contract requires every reply to be attributable to the exact model
/// that produced it. On error replies it carries the model the responder
/// tried (or an empty string if it never reached a backend).
///
/// We carry backend failures *inside* the reply rather than as a
/// `BusError::Handler` because the bus's `register_handler` signature
/// returns `Result<Vec<u8>, BusError>` — an `Err` there means "no reply
/// gets shipped, asker times out", which is the wrong shape for a peer
/// telling us "I tried and the model said no". Inline error fields let
/// the asker distinguish "peer unreachable" from "peer reachable but
/// backend declined" without overloading `BusError`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InferenceReply {
    /// Model output. Empty on error.
    pub content: String,
    /// The model that produced this reply.
    pub model_id: String,
    /// Approximate token usage if the backend reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    /// `Some(message)` if the responder failed to produce content
    /// (model unavailable, backend error, etc). `None` on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl InferenceReply {
    /// True if the responder reported an error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

/// Optional token-usage breakdown.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Topic name (under a user's namespace) for inference requests.
///
/// The actual wire topic is `"<user_fp_hex>:newt/inference/v1"` —
/// `agent_mesh_bus::Topic::new(user_fp, INFERENCE_TOPIC)` handles the
/// namespacing.
pub const INFERENCE_TOPIC: &str = "newt/inference/v1";

/// Capability tag announced in mDNS TXT so peers can pre-filter on
/// "this peer can serve newt-mesh inference" before dialing it.
pub const CAPABILITY_TAG: &str = "newt-inference";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips_through_json() {
        let req = InferenceRequest {
            prompt: "rename foo to bar".into(),
            tier: Some(Tier::Fast),
            model: Some("llama3.1:8b".into()),
            max_tokens: Some(512),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: InferenceRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn request_with_minimal_fields_roundtrips() {
        let req = InferenceRequest {
            prompt: "hi".into(),
            tier: None,
            model: None,
            max_tokens: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: InferenceRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn reply_roundtrips_through_json() {
        let reply = InferenceReply {
            content: "the cat sat on the mat".into(),
            model_id: "llama3.1:8b".into(),
            usage: Some(TokenUsage {
                input_tokens: 12,
                output_tokens: 34,
            }),
            error: None,
        };
        let json = serde_json::to_string(&reply).unwrap();
        let back: InferenceReply = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reply);
        assert!(!back.is_error());
    }

    #[test]
    fn reply_without_usage_roundtrips() {
        let reply = InferenceReply {
            content: "ok".into(),
            model_id: "test-model".into(),
            usage: None,
            error: None,
        };
        let json = serde_json::to_string(&reply).unwrap();
        let back: InferenceReply = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reply);
        assert!(back.usage.is_none());
    }

    #[test]
    fn reply_with_error_is_error() {
        let reply = InferenceReply {
            content: String::new(),
            model_id: "llama3.1:8b".into(),
            usage: None,
            error: Some("model not available".into()),
        };
        let json = serde_json::to_string(&reply).unwrap();
        let back: InferenceReply = serde_json::from_str(&json).unwrap();
        assert!(back.is_error());
        assert_eq!(back.error.as_deref(), Some("model not available"));
    }

    #[test]
    fn reply_omits_optional_fields_when_none() {
        let reply = InferenceReply {
            content: "x".into(),
            model_id: "m".into(),
            usage: None,
            error: None,
        };
        let json = serde_json::to_string(&reply).unwrap();
        // The skip_serializing_if attrs keep the wire shape tight.
        assert!(!json.contains("usage"));
        assert!(!json.contains("error"));
    }

    #[test]
    fn token_usage_is_copy() {
        let u = TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
        };
        let u2 = u;
        // Both still usable — proves Copy semantics.
        assert_eq!(u.input_tokens, u2.input_tokens);
    }

    #[test]
    fn capability_tag_is_stable_string() {
        assert_eq!(CAPABILITY_TAG, "newt-inference");
    }

    #[test]
    fn inference_topic_is_stable_string() {
        // Bump this with a protocol version change, not on a whim.
        assert_eq!(INFERENCE_TOPIC, "newt/inference/v1");
    }
}
