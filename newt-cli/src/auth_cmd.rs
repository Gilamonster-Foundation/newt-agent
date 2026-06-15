//! `newt auth [<server>]` — MCP OAuth authentication.
//!
//! Without arguments: prints a table of every discovered HTTP MCP server and
//! whether its token is valid, expired, missing, or unregistered.
//!
//! With a server name: runs the MCP OAuth 2.1 PKCE browser flow for that
//! server, writes the resulting token to `~/.hermes/mcp-tokens/<name>.json`,
//! and exits. Both newt and hermes-agent share the same token store, so
//! authenticating via either tool gives both access.

pub fn run(server_name: Option<String>) -> anyhow::Result<()> {
    newt_tui::run_auth(server_name.as_deref())
}
