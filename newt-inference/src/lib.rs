//! Newt-Agent inference layer.
//!
//! - `backend::InferenceBackend` is the trait every backend implements.
//! - `local::LocalOllamaBackend` and `local::LocalVllmBackend` are the only
//!   backends compiled into the default Newt binary.
//! - `provider_plugin::ProviderPluginBackend` spawns a subprocess that
//!   speaks the Newt-Provider JSON-RPC protocol — this is how OpenAI,
//!   Anthropic, etc. join via opt-in plugin installs.

pub mod anthropic;
pub mod backend;
/// In-process **embedder** (#720) — opt-in behind the `embedded` feature.
#[cfg(feature = "embedded")]
pub mod embed;
/// In-process inference backend (#639) — opt-in behind the `embedded` feature.
#[cfg(feature = "embedded")]
pub mod embedded;
pub mod local;
/// Curated palette of mini models for in-process inference (always available).
pub mod palette;
pub mod provider_plugin;
pub mod registry;
pub mod responses;
pub mod stream;

// Step 9.7: the shared retry/backoff module moved to `newt-core` so the
// relocated agentic loop (`newt_core::agentic`) can use it without a
// `newt-inference` ⇄ `newt-core` cycle. Re-exported here so every existing
// `newt_inference::retry::*` path keeps working unchanged.
pub use newt_core::retry;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

pub use anthropic::AnthropicBackend;
pub use backend::{ChatReply, ChatRequest, InferenceBackend};
pub use newt_core::retry::{with_backoff_notify, RetryPolicy};
pub use registry::BackendRegistry;
pub use responses::{openai_inference_backend, ResponsesBackend};
pub use stream::{ChatChunk, ChatStream};
