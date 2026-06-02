//! Newt-Agent ACP worker.
//!
//! Speaks the Agent Client Protocol (agentclientprotocol.com) over stdio so
//! `drake-foreman` can dispatch coding goals to Newt instances.
//!
//! Contract (per memory `feedback_drake_patch_not_prose` and
//! `feedback_empty_diff_is_a_crash`):
//! - Worker ONLY edits files; never `git add` / `git commit` / `git push`.
//! - Empty `git diff` post-turn is a deterministic crash — foreman counts it
//!   against the model's scorecard.
//! - `TaskReply.model_id` is mandatory.

mod diff;
mod identity;
pub mod prom;
mod server;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

pub use diff::{capture_diff, is_empty_diff};
pub use identity::{
    worker_session_caveats, IdentityError, WorkerIdentity, WORKER_TURN_CALL_BUDGET,
};
pub use prom::NewtMetrics;
pub use server::{AcpServer, Session, TaskReply};

use std::sync::Arc;

/// Spawn the default ACP worker over stdio.
///
/// Discovers a local Ollama endpoint (per `LocalOllamaBackend::discover`)
/// using the default model `llama3.1:8b` and runs the server until stdin
/// closes.
///
/// Identity is resolved from the default operator key path
/// (`~/.newt/identity.pem`, generated on first run). To opt out, use
/// [`run_with_io_metrics_and_identity`] directly with
/// [`WorkerIdentity::AllowNoKey`].
pub async fn run_stdio() -> anyhow::Result<()> {
    run_with_io(tokio::io::stdin(), tokio::io::stdout()).await
}

/// Like [`run_stdio`] but with an explicit reader/writer pair, and an optional
/// Prometheus metrics registry that will receive per-turn observations.
///
/// Used by the CLI binary's `Worker` dispatch arm to feed a private
/// "real stdout" file handle (obtained from
/// [`newt_cli::stdio_guard::redirect_stdout_to_stderr`]) into the
/// server *after* fd 1 has been redirected to stderr.
///
/// When `metrics` is `Some`, the server records timing, token counts, and
/// cost into the Prometheus counters after every `prompt` turn.
pub async fn run_with_io<R, W>(reader: R, writer: W) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    run_with_io_and_metrics(reader, writer, None).await
}

/// Convenience entry-point: resolves the operator key from the default
/// path (no override), refuses on failure. For finer control — explicit
/// key path, env override, or the debug `--allow-no-key` escape hatch —
/// call [`run_with_io_metrics_and_identity`] directly.
pub async fn run_with_io_and_metrics<R, W>(
    reader: R,
    writer: W,
    metrics: Option<Arc<NewtMetrics>>,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let identity = WorkerIdentity::resolve(None, false)
        .map_err(|e| anyhow::anyhow!("operator identity required: {e}"))?;
    run_with_io_metrics_and_identity(reader, writer, metrics, identity).await
}

/// Full entry-point: accepts explicit I/O streams, an optional metrics
/// registry, and the worker [`WorkerIdentity`] that the ACP server
/// attenuates per dispatch.
///
/// Issue #94: every `prompt` turn the ACP server dispatches under
/// `Coder::run` now derives its [`newt_core::Caveats`] from this
/// identity — never from `Caveats::top()`. The headless worker rooted
/// in a real operator key (the default) therefore enforces the same
/// attenuation-only ocap discipline the TUI already does; the
/// `--allow-no-key` debug fallback preserves pre-#94 behavior for
/// developer iteration but is never the default.
pub async fn run_with_io_metrics_and_identity<R, W>(
    reader: R,
    writer: W,
    metrics: Option<Arc<NewtMetrics>>,
    identity: WorkerIdentity,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let default_model =
        std::env::var("NEWT_DEFAULT_MODEL").unwrap_or_else(|_| "llama3.1:8b".to_string());
    let backend = newt_inference::local::LocalOllamaBackend::discover(&default_model).await?;
    let server = AcpServer::new(std::sync::Arc::new(backend))
        .with_metrics(metrics)
        .with_identity(identity);
    server.run(reader, writer).await
}
