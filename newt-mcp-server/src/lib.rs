//! Newt-Agent MCP server — stdio JSON-RPC.
//!
//! v0 tool surface (vi-minimal):
//! - `code_read` — read a file
//! - `code_edit` — apply a unified diff patch
//! - `code_search` — regex search across a directory tree
//! - `goal_run` — tier-routed inference (placeholder)

pub mod handlers;
pub mod server;

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
    let mut server = server::McpServer::new();
    handlers::register_handlers(&mut server);
    server.run(reader, &mut writer).await
}
