//! [`NewtMeshService`] — the responder side of newt-mesh inference.
//!
//! Binding a service:
//!
//! 1. Opens an [`agent_mesh_bus::Bus`] on the supplied port (with
//!    `0` for "OS-picked").
//! 2. Registers an inference handler on the `newt/inference/v1` topic
//!    under the user's namespace.
//! 3. Returns a guard that keeps the bus alive until [`close`] is
//!    called (or the guard is dropped).
//!
//! mDNS announce happens *inside* the bus (it reads role/host/
//! capabilities off the agent's cert chain), so callers must put
//! `"newt-inference"` into `AgentMetadata::capabilities` at issue
//! time if they want peers to be able to filter on it.

use std::sync::Arc;

use agent_mesh_bus::{Bus, Topic};
use agent_mesh_core::{AgentKey, Fingerprint, UserKey};
use newt_inference::backend::{ChatRequest, InferenceBackend, Message};

use crate::protocol::{InferenceReply, InferenceRequest, INFERENCE_TOPIC};

/// A bound responder serving inference over the agent-mesh.
///
/// The bus and its background tasks live for the lifetime of the
/// service. Drop the service or call [`close`](Self::close) to release
/// the QUIC endpoint and unregister the mDNS announce.
pub struct NewtMeshService {
    bus: Bus,
    backend_name: String,
    backend_model: String,
}

impl NewtMeshService {
    /// Bind a responder service.
    ///
    /// * `user` — trust root; every peer that shares this user
    ///   fingerprint auto-teams with us.
    /// * `agent` — the per-process agent key. Its
    ///   `AgentMetadata::capabilities` should include
    ///   [`crate::CAPABILITY_TAG`] so browsing peers can filter on it.
    /// * `backend` — the inference backend that services incoming
    ///   requests. Wrapping in `Arc` lets multiple in-flight
    ///   handlers share it.
    /// * `port` — UDP port to bind to. `0` picks an ephemeral port.
    pub async fn bind(
        user: &UserKey,
        agent: AgentKey,
        backend: Arc<dyn InferenceBackend>,
        port: u16,
    ) -> anyhow::Result<Self> {
        let backend_name = backend.name().to_string();
        let backend_model = backend.model_id().to_string();
        let user_fp = user.fingerprint();
        let bus = Bus::bind(user, agent, port).await?;

        let topic = Topic::new(user_fp, INFERENCE_TOPIC);
        let handler_backend = backend.clone();
        bus.handle_requests(topic, move |body| {
            let backend = handler_backend.clone();
            async move { Ok(handle_inference(backend, body).await) }
        });

        tracing::info!(
            agent = %bus.agent_fingerprint().short(),
            user = %bus.user_fingerprint().short(),
            port = bus.local_port(),
            backend = %backend_name,
            model = %backend_model,
            "newt-mesh service bound"
        );

        Ok(Self {
            bus,
            backend_name,
            backend_model,
        })
    }

    /// Agent fingerprint this responder runs as.
    #[must_use]
    pub fn agent_fingerprint(&self) -> Fingerprint {
        self.bus.agent_fingerprint()
    }

    /// User fingerprint this responder belongs to.
    #[must_use]
    pub fn user_fingerprint(&self) -> Fingerprint {
        self.bus.user_fingerprint()
    }

    /// Local UDP port the bus is bound on.
    #[must_use]
    pub fn local_port(&self) -> u16 {
        self.bus.local_port()
    }

    /// Name of the backend wired into this responder (e.g. `"ollama"`).
    #[must_use]
    pub fn backend_name(&self) -> &str {
        &self.backend_name
    }

    /// Model id served by this responder's backend (e.g. `"llama3.1:8b"`).
    #[must_use]
    pub fn backend_model(&self) -> &str {
        &self.backend_model
    }

    /// Graceful shutdown — closes the bus and releases its endpoint.
    pub async fn close(self) -> anyhow::Result<()> {
        self.bus.close().await?;
        Ok(())
    }
}

/// Decode the body, dispatch to the backend, encode the reply.
///
/// Always returns a serialised [`InferenceReply`]; backend failures
/// are encoded into the reply's `error` field so the asker sees a
/// "responder is up, request failed" signal instead of a timeout.
async fn handle_inference(backend: Arc<dyn InferenceBackend>, body: Vec<u8>) -> Vec<u8> {
    let req: InferenceRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "newt-mesh: decode InferenceRequest failed");
            return reply_to_bytes(InferenceReply {
                content: String::new(),
                model_id: backend.model_id().to_string(),
                usage: None,
                error: Some(format!("malformed InferenceRequest: {e}")),
            });
        }
    };

    if let Some(pin) = req.model.as_deref() {
        if pin != backend.model_id() {
            tracing::info!(
                requested = %pin,
                served = %backend.model_id(),
                "newt-mesh: model pin mismatch"
            );
            return reply_to_bytes(InferenceReply {
                content: String::new(),
                model_id: backend.model_id().to_string(),
                usage: None,
                error: Some(format!(
                    "model pin {pin} not available (responder serves {})",
                    backend.model_id()
                )),
            });
        }
    }

    let chat = ChatRequest {
        messages: vec![Message {
            role: "user".into(),
            content: req.prompt,
        }],
        max_tokens: req.max_tokens,
    };

    match backend.complete(chat).await {
        Ok(chat_reply) => reply_to_bytes(InferenceReply {
            content: chat_reply.content,
            model_id: chat_reply.model_id,
            usage: None,
            error: None,
        }),
        Err(e) => {
            tracing::warn!(error = %e, "newt-mesh: backend complete failed");
            reply_to_bytes(InferenceReply {
                content: String::new(),
                model_id: backend.model_id().to_string(),
                usage: None,
                error: Some(format!("backend error: {e}")),
            })
        }
    }
}

/// Encode an [`InferenceReply`] for the bus. Falls back to a hand-
/// rolled error JSON if serialisation somehow fails (shouldn't happen
/// for our types, but the wire path must not panic).
fn reply_to_bytes(reply: InferenceReply) -> Vec<u8> {
    match serde_json::to_vec(&reply) {
        Ok(bytes) => bytes,
        Err(e) => format!(r#"{{"content":"","model_id":"","error":"reply encode failed: {e}"}}"#)
            .into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::router::Tier;
    use tests_common::MockBackend;

    #[tokio::test]
    async fn handle_inference_returns_reply_for_known_backend() {
        let backend: Arc<dyn InferenceBackend> =
            Arc::new(MockBackend::all_tiers("svc-test", "diff goes here"));
        let req = InferenceRequest {
            prompt: "hi".into(),
            tier: Some(Tier::Standard),
            model: None,
            max_tokens: None,
        };
        let body = serde_json::to_vec(&req).unwrap();
        let bytes = handle_inference(backend, body).await;
        let reply: InferenceReply = serde_json::from_slice(&bytes).unwrap();
        assert!(!reply.is_error());
        assert_eq!(reply.content, "diff goes here");
        assert_eq!(reply.model_id, "svc-test-model");
    }

    #[tokio::test]
    async fn handle_inference_reports_malformed_request() {
        let backend: Arc<dyn InferenceBackend> = Arc::new(MockBackend::all_tiers("svc-test", "x"));
        // Not a valid InferenceRequest JSON.
        let body = b"{nope".to_vec();
        let bytes = handle_inference(backend, body).await;
        let reply: InferenceReply = serde_json::from_slice(&bytes).unwrap();
        assert!(reply.is_error());
        assert!(reply.error.unwrap().contains("malformed"));
    }

    #[tokio::test]
    async fn handle_inference_rejects_model_pin_mismatch() {
        let backend: Arc<dyn InferenceBackend> = Arc::new(MockBackend::all_tiers("svc-test", "x"));
        let req = InferenceRequest {
            prompt: "hi".into(),
            tier: None,
            model: Some("some-other-model".into()),
            max_tokens: None,
        };
        let body = serde_json::to_vec(&req).unwrap();
        let bytes = handle_inference(backend, body).await;
        let reply: InferenceReply = serde_json::from_slice(&bytes).unwrap();
        assert!(reply.is_error());
        let msg = reply.error.unwrap();
        assert!(msg.contains("not available"), "got: {msg}");
        assert!(msg.contains("some-other-model"), "got: {msg}");
    }

    #[tokio::test]
    async fn handle_inference_accepts_matching_model_pin() {
        let backend: Arc<dyn InferenceBackend> = Arc::new(MockBackend::all_tiers("svc-test", "ok"));
        let req = InferenceRequest {
            prompt: "hi".into(),
            tier: None,
            model: Some("svc-test-model".into()),
            max_tokens: None,
        };
        let body = serde_json::to_vec(&req).unwrap();
        let bytes = handle_inference(backend, body).await;
        let reply: InferenceReply = serde_json::from_slice(&bytes).unwrap();
        assert!(!reply.is_error());
        assert_eq!(reply.content, "ok");
    }
}
