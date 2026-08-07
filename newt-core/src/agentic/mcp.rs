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
    ///
    /// Requires a [`LeasedMcpCall`] witness, so a call that did not pass the
    /// call-time leash ([`leash_mcp_call`]) does not type-check — the tool name
    /// and args travel *inside* the witness.
    async fn call(&mut self, leased: &LeasedMcpCall<'_>) -> String;
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
    async fn call(&mut self, _leased: &LeasedMcpCall<'_>) -> String {
        // Unreachable through the loop (it only calls after `handles()`),
        // but fail soft for any direct caller.
        "error: no MCP servers connected".to_string()
    }
}

// ── MCP call-time leash (`mcp-under-leash`) ─────────────────────────────────
//
// Admission (`newt_core::mcp::admit` → `AdmittedServer`) decides WHICH servers
// may connect. This is the CALL-time counterpart: every individual tool call is
// mediated before it reaches the wire. The [`LeasedMcpCall`] witness (private
// field, minted only by [`leash_mcp_call`]) is REQUIRED by [`McpTools::call`],
// so an un-leashed dispatch does not type-check — the same structural guarantee
// the admission witness gives, one layer down. The pre-leash world dispatched a
// remote tool UNLEASHED whenever there was no active persona ("no persona" was
// read as "unrestricted"); the leash closes that.

use serde_json::Value;

/// Effect class of an MCP tool operation, decided from the tool NAME by a
/// droppable convention — NEVER the server's own `readOnlyHint` /
/// `destructiveHint` (untrusted under the hostile-server threat model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpEffect {
    /// A read-only lookup — low-risk; permitted even without an explicit grant.
    Read,
    /// A state-changing or unknown operation — fail-closed: it needs a grant.
    Mutating,
}

/// Read-class verb prefixes: a tool whose bare (post-`server__`) name begins
/// with one of these is [`McpEffect::Read`]; everything else is
/// [`McpEffect::Mutating`]. Data, not logic — extend the slice (the three-Cs
/// convention). An unknown verb ⇒ MUTATING (fail-closed).
pub const MCP_READ_VERB_PREFIXES: &[&str] = &[
    "get", "list", "read", "search", "find", "fetch", "describe", "query", "show", "lookup",
    "view", "count", "stat", "head", "peek", "inspect",
];

/// Classify a (possibly `server__tool`-namespaced) MCP tool name by effect.
#[must_use]
pub fn classify_mcp_effect(tool: &str) -> McpEffect {
    let bare = tool.rsplit("__").next().unwrap_or(tool);
    let lower = bare.to_ascii_lowercase();
    if MCP_READ_VERB_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        McpEffect::Read
    } else {
        McpEffect::Mutating
    }
}

/// Witness that a single MCP tool call passed the call-time leash. The private
/// `_seal` field makes it unconstructable outside this module, so the only way
/// to obtain one is a successful [`leash_mcp_call`]; [`McpTools::call`] requires
/// a `&LeasedMcpCall`, so an un-leashed dispatch does not compile.
#[derive(Debug)]
pub struct LeasedMcpCall<'a> {
    tool: &'a str,
    args: &'a Value,
    _seal: (),
}

impl<'a> LeasedMcpCall<'a> {
    /// The namespaced tool name this lease authorizes.
    #[must_use]
    pub fn tool(&self) -> &str {
        self.tool
    }
    /// The arguments this lease authorizes.
    #[must_use]
    pub fn args(&self) -> &Value {
        self.args
    }
}

/// Refusal of an MCP call by the call-time leash.
#[derive(Debug, Clone)]
pub struct LeashDenied {
    /// The tool that was refused.
    pub tool: String,
    /// Human-facing reason (fed back to the model as the tool result).
    pub reason: String,
}

/// Mint a [`LeasedMcpCall`] iff the call-time leash admits it — the SOLE minter
/// of the witness [`McpTools::call`] requires.
///
/// `granted` folds in every EXPLICIT authorization the caller has already
/// resolved: the active persona's tool allow-list, or a human `PermissionGate`
/// decision. The caller is responsible for computing `granted` truthfully; the
/// leash then makes the dispatch structurally impossible without it — a `false`
/// `granted` fails closed, no matter the effect class. (Read-class tolerance for
/// the no-persona case is applied by the caller when it computes `granted`, so a
/// persona's explicit *deny* is never overridden here.)
pub fn leash_mcp_call<'a>(
    tool: &'a str,
    args: &'a Value,
    granted: bool,
) -> Result<LeasedMcpCall<'a>, LeashDenied> {
    if granted {
        Ok(LeasedMcpCall {
            tool,
            args,
            _seal: (),
        })
    } else {
        Err(LeashDenied {
            tool: tool.to_string(),
            reason: format!("remote tool `{tool}` refused by the call-time leash (no grant)"),
        })
    }
}

#[cfg(test)]
mod leash_tests {
    use super::*;

    #[test]
    fn classify_reads_by_verb_prefix_stripping_namespace() {
        for read in [
            "get_x",
            "srv__list_things",
            "search",
            "readFile",
            "srv__describe_pod",
        ] {
            assert_eq!(classify_mcp_effect(read), McpEffect::Read, "{read}");
        }
        // Unknown / state-changing verbs are MUTATING (fail-closed).
        for mut_ in [
            "delete_all",
            "srv__create_incident",
            "exec",
            "srv__rm",
            "frobnicate",
        ] {
            assert_eq!(classify_mcp_effect(mut_), McpEffect::Mutating, "{mut_}");
        }
    }

    #[test]
    fn leash_mints_only_when_granted() {
        let args = serde_json::json!({});
        // Granted → witness carrying the tool + args.
        let leased = leash_mcp_call("srv__delete", &args, true).expect("granted mints");
        assert_eq!(leased.tool(), "srv__delete");
        assert_eq!(leased.args(), &args);
        // Not granted → refusal (the caller folds read-tolerance into `granted`).
        assert!(leash_mcp_call("srv__delete", &args, false).is_err());
    }
}
