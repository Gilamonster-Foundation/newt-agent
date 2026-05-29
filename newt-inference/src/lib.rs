//! Newt-Agent inference layer.
//!
//! - `backend::InferenceBackend` is the trait every backend implements.
//! - `local::LocalOllamaBackend` and `local::LocalVllmBackend` are the only
//!   backends compiled into the default Newt binary.
//! - `provider_plugin::ProviderPluginBackend` spawns a subprocess that
//!   speaks the Newt-Provider JSON-RPC protocol — this is how OpenAI,
//!   Anthropic, etc. join via opt-in plugin installs.

pub mod backend;
pub mod local;
pub mod provider_plugin;
pub mod registry;
pub mod stream;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

pub use backend::{ChatReply, ChatRequest, InferenceBackend};
pub use registry::BackendRegistry;
pub use stream::{ChatChunk, ChatStream};
