//! Newt-Agent MCP server — stdio JSON-RPC.
//!
//! v0 tool surface (vi-minimal):
//! - `code_read` — read a file
//! - `code_edit` — apply a unified diff patch
//! - `code_search` — regex search across a directory tree
//! - `goal_run` — tier-routed inference (wired through Router +
//!   BackendRegistry; tries to discover a local Ollama on startup)

use std::sync::Arc;

use newt_core::Router;
use newt_inference::BackendRegistry;

pub mod caveats;
pub mod handlers;
pub mod server;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

/// Default model name handed to the discovered Ollama backend.
///
/// A follow-up will read this from `Config` (see `newt_core::Config`)
/// so an operator can override without recompiling. For now it matches
/// the value `newt-acp-worker::run_with_io` uses, so both surfaces
/// drive the same model by default.
const DEFAULT_OLLAMA_MODEL: &str = "llama3.1:8b";

pub async fn run_stdio() -> anyhow::Result<()> {
    run_with_io(tokio::io::stdin(), tokio::io::stdout()).await
}

/// Run the MCP server against an explicit reader/writer pair.
///
/// Used by the CLI binary's `Mcp` dispatch arm to feed a private
/// "real stdout" file handle (obtained from
/// [`newt_cli::stdio_guard::redirect_stdout_to_stderr`]) into the
/// server *after* fd 1 has been redirected to stderr. That sequence
/// is what protects the JSON-RPC wire from rogue `println!` calls in
/// dependencies.
pub async fn run_with_io<R, W>(reader: R, mut writer: W) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let registry = build_default_registry().await;
    let router = Arc::new(Router::new());
    let mut server = server::McpServer::new();
    handlers::register_handlers(&mut server, registry, router);
    server.run(reader, &mut writer).await
}

/// Build the default backend registry for the MCP server.
///
/// Tries to discover a local Ollama endpoint with the default model.
/// If discovery fails the registry stays empty — `goal_run` will then
/// surface `NoBackendForTier` as a clean JSON-RPC error rather than
/// crashing the whole server (the other tools still work fine).
async fn build_default_registry() -> Arc<BackendRegistry> {
    let mut registry = BackendRegistry::new();
    match newt_inference::local::LocalOllamaBackend::discover(DEFAULT_OLLAMA_MODEL).await {
        Ok(backend) => {
            tracing::info!(
                model = DEFAULT_OLLAMA_MODEL,
                endpoint = backend.endpoint(),
                "MCP server: discovered local Ollama backend"
            );
            registry.register(Arc::new(backend));
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "MCP server: no local Ollama discovered; goal_run will fail with NoBackendForTier"
            );
        }
    }
    Arc::new(registry)
}
