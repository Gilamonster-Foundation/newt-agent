//! MCP bridge seam for the agentic loop.
//!
//! The loop advertises and dispatches remote MCP tools (namespaced
//! `server__tool`) alongside the built-ins. The concrete connection pool
//! (`Mcp` in `newt-tui`) wraps `newt-mcp-client`, which itself depends on
//! `newt-core` — so the loop cannot name that type without a dependency
//! cycle. This minimal trait is the seam: `newt-tui` implements it for its
//! `Mcp` pool with three one-line forwarders.
//!
//! NOTE: this is deliberately NOT an inference-backend abstraction.
//! `ChatCtx` stays a concrete type per the Step 9.7 spec (no
//! `InferenceBackend` trait until a second implementor exists).

/// Bridge to connected MCP servers used by the agentic loop.
#[async_trait::async_trait]
pub trait McpTools: Send {
    /// Whether this bridge routes `name` (an MCP-namespaced `server__tool`).
    fn handles(&self, name: &str) -> bool;
    /// Tool definitions advertised to the model in addition to the built-ins.
    fn tool_defs(&self) -> Vec<serde_json::Value>;
    /// Invoke the remote tool and return the result text fed back to the model.
    async fn call(&mut self, name: &str, args: &serde_json::Value) -> String;
}

/// The no-servers bridge: handles nothing, advertises nothing. Used by
/// headless callers without MCP config and by the loop's unit tests
/// (mirrors the old `Mcp::empty()` behavior exactly).
pub struct NoMcp;

#[async_trait::async_trait]
impl McpTools for NoMcp {
    fn handles(&self, _name: &str) -> bool {
        false
    }
    fn tool_defs(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }
    async fn call(&mut self, _name: &str, _args: &serde_json::Value) -> String {
        // Unreachable through the loop (it only calls after `handles()`),
        // but fail soft for any direct caller.
        "error: no MCP servers connected".to_string()
    }
}
