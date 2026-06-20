//! Mesh integration for newt-agent.
//!
//! - [`NewtMeshService`] — bind a mesh-listening newt that answers
//!   [`InferenceRequest`]s on the agent-mesh bus.
//! - [`MeshAsker`] — client to send [`InferenceRequest`]s to a peer
//!   newt and await its [`InferenceReply`].
//!
//! Wire types: see [`protocol`].
//!
//! # Worked example
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::time::Duration;
//! use agent_mesh_core::{AgentKey, AgentMetadata, Caveats, UserKey};
//! use newt_mesh::{NewtMeshService, MeshAsker, InferenceRequest};
//!
//! # async fn demo() -> anyhow::Result<()> {
//! let user = UserKey::generate();
//! let responder_agent = AgentKey::issue(&user, AgentMetadata {
//!     role: "newt-worker".into(),
//!     host: "geforcenuc".into(),
//!     capabilities: vec!["newt-inference".into()],
//!     issued_at: "2026-05-29T12:00:00Z".into(),
//!     expires_at: None,
//!     caveats: Caveats::top(),
//! });
//! let responder_fp = responder_agent.fingerprint();
//!
//! // Caller provides any InferenceBackend implementation.
//! let backend: Arc<dyn newt_inference::backend::InferenceBackend> = unimplemented!();
//! let service = NewtMeshService::bind(&user, responder_agent, backend, 0).await?;
//!
//! let asker_agent = AgentKey::issue(&user, AgentMetadata {
//!     role: "newt-asker".into(),
//!     host: "geforcenuc".into(),
//!     capabilities: vec!["newt-asker".into()],
//!     issued_at: "2026-05-29T12:00:00Z".into(),
//!     expires_at: None,
//!     caveats: Caveats::top(),
//! });
//! let asker = MeshAsker::bind(&user, asker_agent).await?;
//! let reply = asker.ask(
//!     responder_fp,
//!     InferenceRequest { prompt: "hi".into(), tier: None, model: None, max_tokens: None },
//!     Duration::from_secs(10),
//! ).await?;
//! println!("{}: {}", reply.model_id, reply.content);
//! # Ok(())
//! # }
//! ```

#![doc(html_root_url = "https://docs.rs/newt-mesh")]

pub mod ask;
pub mod caveats;
pub mod error;
pub mod plugin_envelope;
pub mod protocol;
pub mod service;

pub use ask::MeshAsker;
pub use caveats::{caveats_for_peer, caveats_for_peer_at, CaveatsError};
pub use error::MeshIntegrationError;
pub use plugin_envelope::{caveats_from_envelope, serialize_for_plugin, EnvelopeError};
pub use protocol::{InferenceReply, InferenceRequest, TokenUsage, CAPABILITY_TAG, INFERENCE_TOPIC};
pub use service::NewtMeshService;
