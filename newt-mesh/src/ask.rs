//! [`MeshAsker`] — the client side of newt-mesh inference.
//!
//! Binds a (typically ephemeral-port) bus, then exposes a single
//! request/reply method: send a peer an [`InferenceRequest`], get back
//! either an [`InferenceReply`] or a [`crate::MeshIntegrationError`].
//!
//! The asker does NOT need a backend — it dispatches to a peer that
//! has one.

use std::time::Duration;

use agent_mesh_bus::{Bus, Topic};
use agent_mesh_core::{AgentKey, Fingerprint, UserKey};

use crate::error::{MeshIntegrationError, Result};
use crate::protocol::{InferenceReply, InferenceRequest, INFERENCE_TOPIC};

/// Client that asks peer newts for inference over the mesh.
pub struct MeshAsker {
    bus: Bus,
}

impl MeshAsker {
    /// Bind a client bus.
    ///
    /// The asker uses an ephemeral port (`0`) — it doesn't need a
    /// stable port because nobody dials it back. mDNS still announces
    /// the asker (the bus owns the announcer), so a peer browser can
    /// see "asker came up" if it wants to.
    pub async fn bind(user: &UserKey, agent: AgentKey) -> Result<Self> {
        let bus = Bus::bind(user, agent, 0).await?;
        Ok(Self { bus })
    }

    /// Agent fingerprint this asker runs as.
    #[must_use]
    pub fn agent_fingerprint(&self) -> Fingerprint {
        self.bus.agent_fingerprint()
    }

    /// User fingerprint this asker belongs to.
    #[must_use]
    pub fn user_fingerprint(&self) -> Fingerprint {
        self.bus.user_fingerprint()
    }

    /// Send `request` to `peer_fp` and wait up to `timeout` for the
    /// reply.
    ///
    /// Returns the parsed [`InferenceReply`] on the wire. Note that
    /// a successful return DOES NOT imply the backend succeeded — the
    /// reply may carry `error: Some(_)`; check
    /// [`InferenceReply::is_error`].
    pub async fn ask(
        &self,
        peer_fp: Fingerprint,
        request: InferenceRequest,
        timeout: Duration,
    ) -> Result<InferenceReply> {
        let user_fp = self.bus.user_fingerprint();
        let topic = Topic::new(user_fp, INFERENCE_TOPIC);
        let body = serde_json::to_vec(&request)?;
        let reply_bytes = self.bus.request(peer_fp, &topic, body, timeout).await?;
        let reply: InferenceReply = serde_json::from_slice(&reply_bytes)
            .map_err(|e| MeshIntegrationError::MalformedReply(e.to_string()))?;
        Ok(reply)
    }

    /// Graceful shutdown — closes the bus.
    pub async fn close(self) -> Result<()> {
        self.bus.close().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_mesh_core::{AgentMetadata, Caveats, UserKey};

    fn agent(user: &UserKey, role: &str) -> AgentKey {
        AgentKey::issue(
            user,
            AgentMetadata {
                role: role.into(),
                host: "test".into(),
                capabilities: vec!["test".into()],
                issued_at: "2026-05-29T00:00:00Z".into(),
                expires_at: None,
                caveats: Caveats::top(),
            },
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bind_exposes_fingerprints() {
        let user = UserKey::generate();
        let a = agent(&user, "asker");
        let a_fp = a.fingerprint();
        let asker = MeshAsker::bind(&user, a).await.unwrap();
        assert_eq!(asker.user_fingerprint(), user.fingerprint());
        assert_eq!(asker.agent_fingerprint(), a_fp);
        asker.close().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ask_unknown_peer_returns_bus_error() {
        let user = UserKey::generate();
        let a = agent(&user, "asker");
        let asker = MeshAsker::bind(&user, a).await.unwrap();
        let phantom = Fingerprint([0xeeu8; 32]);
        let req = InferenceRequest {
            prompt: "ping".into(),
            tier: None,
            model: None,
            max_tokens: None,
        };
        let res = asker.ask(phantom, req, Duration::from_millis(200)).await;
        match res {
            Err(MeshIntegrationError::Bus(_)) => {}
            other => panic!("expected Bus error, got {other:?}"),
        }
        asker.close().await.unwrap();
    }
}
