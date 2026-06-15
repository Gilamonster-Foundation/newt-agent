//! Live MCP server connections for the chat session.
//!
//! [`Mcp`] holds the connections opened once at session start (see
//! [`crate::run_chat`]) and reused for every tool call. It bridges the discovery
//! ([`newt_core::mcp`]) and client ([`newt_mcp_client`]) layers into the TUI's
//! agent loop: it advertises the remote tools (namespaced `server__tool`) in the
//! tool list, and routes a namespaced call to the right server.
//!
//! It connects **stdio** and **streamable-HTTP** servers, and carries **no
//! Caveats leash** on the remote tools — they run with whatever authority their
//! own server has.

use newt_core::mcp::{McpServerEntry, TransportKind};
use newt_mcp_client::{connect_http, connect_stdio, namespaced, split_namespaced, ConnectedServer};
use serde_json::{json, Value};

/// The session's connected MCP servers.
pub(crate) struct Mcp {
    servers: Vec<ConnectedServer>,
}

impl Mcp {
    /// An empty set — connects to nothing. Used by tests (the live session
    /// always builds via [`Self::connect`]).
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            servers: Vec::new(),
        }
    }

    /// Discover (newt config + Claude Code config) and connect to every **stdio**
    /// MCP server. A server that fails to spawn/initialize is logged and skipped
    /// — one bad server never blocks the session or the others.
    pub(crate) async fn connect(workspace: &str, cfg_servers: &[McpServerEntry]) -> Self {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let entries = newt_core::mcp::discover(
            cfg_servers,
            home.as_deref(),
            std::path::Path::new(workspace),
        );
        let mut servers = Vec::new();
        for entry in &entries {
            // Dispatch on transport. The legacy SSE-only transport (a separate
            // GET event-stream + POST endpoint) is not implemented; modern
            // servers use streamable-HTTP (`type: "http"`).
            let result = match entry.transport {
                TransportKind::Stdio => connect_stdio(entry).await,
                TransportKind::Http => {
                    // If no Authorization header is configured, check
                    // ~/.hermes/mcp-tokens/ for a stored OAuth token and inject
                    // it so newt can share the same auth as hermes-agent.
                    let mut enriched = entry.clone();
                    if !enriched.headers.contains_key("Authorization")
                        && !enriched.headers.contains_key("authorization")
                    {
                        if let Some(token) = crate::mcp_token::load_bearer_token(&entry.name).await
                        {
                            enriched
                                .headers
                                .insert("Authorization".into(), format!("Bearer {token}"));
                        }
                    }
                    connect_http(&enriched).await
                }
                TransportKind::Sse => {
                    tracing::warn!(
                        "MCP server `{}`: legacy SSE transport is not supported \
                         (use streamable-HTTP, `type = \"http\"`) — skipped",
                        entry.name
                    );
                    continue;
                }
            };
            match result {
                Ok(connected) => servers.push(connected),
                Err(e) => tracing::warn!("MCP server `{}` skipped: {e}", entry.name),
            }
        }
        Self { servers }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// `(server_name, tool_count)` for each connected server — for the ready line.
    pub(crate) fn summary(&self) -> Vec<(String, usize)> {
        self.servers
            .iter()
            .map(|s| (s.name.clone(), s.tools.len()))
            .collect()
    }

    /// OpenAI-style function tool definitions for every remote tool, with names
    /// namespaced `server__tool` so two servers cannot collide.
    pub(crate) fn tool_defs(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for server in &self.servers {
            for tool in &server.tools {
                out.push(json!({
                    "type": "function",
                    "function": {
                        "name": namespaced(&server.name, &tool.name),
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                }));
            }
        }
        out
    }

    /// Whether `name` is a namespaced tool belonging to a connected server.
    pub(crate) fn handles(&self, name: &str) -> bool {
        match split_namespaced(name) {
            Some((server, _)) => self.servers.iter().any(|s| s.name == server),
            None => false,
        }
    }

    /// Route a `server__tool` call to its server and render the result as the
    /// string the agent loop feeds back as the tool message.
    pub(crate) async fn call(&mut self, name: &str, args: &Value) -> String {
        let Some((server_name, tool)) = split_namespaced(name) else {
            return format!("error: `{name}` is not a namespaced MCP tool");
        };
        let Some(server) = self.servers.iter_mut().find(|s| s.name == server_name) else {
            return format!("error: no connected MCP server `{server_name}`");
        };
        match server.conn.call_tool(tool, args.clone()).await {
            Ok(result) => format_result(&result),
            Err(e) => format!("error: {e}"),
        }
    }
}

/// Bridge into the agentic loop (Step 9.7): `newt_core::agentic` cannot name
/// this type without a `newt-core` ← `newt-mcp-client` dependency cycle, so
/// the loop takes the minimal [`McpTools`] seam and the TUI forwards to the
/// inherent methods above.
#[async_trait::async_trait]
impl newt_core::agentic::McpTools for Mcp {
    fn handles(&self, name: &str) -> bool {
        Self::handles(self, name)
    }
    fn tool_defs(&self) -> Vec<Value> {
        Self::tool_defs(self)
    }
    async fn call(&mut self, name: &str, args: &Value) -> String {
        Self::call(self, name, args).await
    }
}

/// Flatten an MCP `tools/call` result (`{ content: [{type,text}], isError? }`)
/// into agent-facing text. Falls back to raw JSON if there is no text content.
fn format_result(result: &Value) -> String {
    let mut text = String::new();
    if let Some(items) = result.get("content").and_then(Value::as_array) {
        for item in items {
            if let Some(t) = item.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
        }
    }
    if text.is_empty() {
        text = result.to_string();
    }
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        format!("tool error: {text}")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_handles_nothing_and_has_no_defs() {
        let mcp = Mcp::empty();
        assert!(mcp.is_empty());
        assert!(!mcp.handles("git__status"));
        assert!(mcp.tool_defs().is_empty());
    }

    #[test]
    fn format_result_joins_text_content() {
        let r =
            json!({ "content": [{"type":"text","text":"hello"},{"type":"text","text":"world"}] });
        assert_eq!(format_result(&r), "hello\nworld");
    }

    #[test]
    fn format_result_flags_errors_and_falls_back_to_json() {
        let err = json!({ "content": [{"type":"text","text":"boom"}], "isError": true });
        assert_eq!(format_result(&err), "tool error: boom");
        // No text content → raw JSON fallback (still informative).
        let weird = json!({ "structured": 1 });
        assert!(format_result(&weird).contains("structured"));
    }
}
