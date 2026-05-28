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
    let mut server = server::McpServer::new();
    handlers::register_handlers(&mut server);
    server.run_stdio().await
}
