//! Newt-Agent MCP server — stdio JSON-RPC.
//!
//! v0 tool surface (vi-minimal):
//! - `code.read(path)`
//! - `code.edit(path, patch)`
//! - `code.search(query, path)`
//! - `goal.run(prompt, tier?)`
//! - `flight.dispatch(prompt, models[])`

pub async fn run_stdio() -> anyhow::Result<()> {
    anyhow::bail!("newt-mcp-server::run_stdio not yet implemented")
}
