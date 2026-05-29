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
mod server;

pub use diff::{capture_diff, is_empty_diff};
pub use server::{AcpServer, Session, TaskReply};

/// Spawn the default ACP worker over stdio.
///
/// Discovers a local Ollama endpoint (per `LocalOllamaBackend::discover`)
/// using the default model `llama3.1:8b` and runs the server until stdin
/// closes.
pub async fn run_stdio() -> anyhow::Result<()> {
    run_with_io(tokio::io::stdin(), tokio::io::stdout()).await
}

/// Spawn the default ACP worker against an explicit reader/writer pair.
///
/// Used by the CLI binary's `Worker` dispatch arm to feed a private
/// "real stdout" file handle (obtained from
/// [`newt_cli::stdio_guard::redirect_stdout_to_stderr`]) into the
/// server *after* fd 1 has been redirected to stderr. That sequence
/// is what protects the JSON-RPC wire from rogue `println!` calls in
/// dependencies — see the module-level doc on `stdio_guard` for the
/// full rationale.
///
/// Picks the initial Ollama model from `NEWT_DEFAULT_MODEL` env, falling
/// back to `llama3.1:8b`. Lets the bake-off harness iterate models by
/// spawning fresh worker subprocesses with different model envs while
/// session-level model swap isn't wired through `ChatRequest` yet.
pub async fn run_with_io<R, W>(reader: R, writer: W) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let default_model =
        std::env::var("NEWT_DEFAULT_MODEL").unwrap_or_else(|_| "llama3.1:8b".to_string());
    let backend = newt_inference::local::LocalOllamaBackend::discover(&default_model).await?;
    let server = AcpServer::new(std::sync::Arc::new(backend));
    server.run(reader, writer).await
}
