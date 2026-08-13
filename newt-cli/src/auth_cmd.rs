//! `newt auth [<server>]` — MCP OAuth authentication.
//!
//! Without arguments: prints a table of every discovered HTTP MCP server and
//! whether its token is valid, expired, missing, or unregistered.
//!
//! With a server name: runs the MCP OAuth 2.1 PKCE browser flow for that server
//! and writes a Newt-owned private credential generation. Newt may read and
//! adopt a complete, strictly resource/issuer-bound legacy Hermes credential as
//! input, but Newt auth and refresh never overwrite Hermes' flat token files.
//!
//! Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 17:11 EDT | Date: 2026-08-13

pub fn run(server_name: Option<String>) -> anyhow::Result<()> {
    newt_tui::run_auth(server_name.as_deref())
}
