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

/// WHY a call is authorized to dispatch — the structural grant *provenance*,
/// NOT the server's own tool name or metadata hints. Authority is bound to how
/// the OPERATOR granted THIS operation; a server-chosen tool name can never mint
/// one. This is the `mcp-under-leash` closure of the name-classification vector:
/// a hostile admitted server that names a destructive tool with a read verb
/// (`get_…`) earns NOTHING here, because a read verb is not a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpGrant {
    /// The active persona's tool allow-list explicitly names this operation.
    PersonaAllowList,
    /// A present human `PermissionGate` approved THIS specific call.
    HumanApproved,
}

/// Witness that a single MCP tool call passed the call-time leash. The private
/// `_seal` field makes it unconstructable outside this module, so the only way
/// to obtain one is a successful [`leash_mcp_call`]; [`McpTools::call`] requires
/// a `&LeasedMcpCall`, so an un-leashed dispatch does not compile. It carries the
/// structural [`McpGrant`] that authorized it, so the authority is bound to the
/// grant, never to the server-controlled tool name.
#[derive(Debug)]
pub struct LeasedMcpCall<'a> {
    tool: &'a str,
    args: &'a Value,
    grant: McpGrant,
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
    /// How this call was authorized — the structural provenance, never the
    /// server's say-so.
    #[must_use]
    pub fn grant(&self) -> McpGrant {
        self.grant
    }
    /// The admitted server this operation routes to: the namespace prefix newt
    /// assigns at admission (`server__tool`), NOT a value the server supplies in
    /// its tool metadata. Empty if the tool is un-namespaced.
    #[must_use]
    pub fn server(&self) -> &str {
        self.tool.rsplit_once("__").map_or("", |(server, _)| server)
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

/// Mint a [`LeasedMcpCall`] iff a structural [`McpGrant`] authorizes it — the
/// SOLE minter of the witness [`McpTools::call`] requires.
///
/// `grant` is the provenance the caller resolved: the active persona's tool
/// allow-list ([`McpGrant::PersonaAllowList`]) or a present human's decision
/// ([`McpGrant::HumanApproved`]). `None` fails closed. Authority NEVER comes from
/// the tool NAME — a read-verb name is not a grant (the name-classification
/// vector a hostile admitted server could otherwise exploit).
pub fn leash_mcp_call<'a>(
    tool: &'a str,
    args: &'a Value,
    grant: Option<McpGrant>,
) -> Result<LeasedMcpCall<'a>, LeashDenied> {
    match grant {
        Some(grant) => Ok(LeasedMcpCall {
            tool,
            args,
            grant,
            _seal: (),
        }),
        None => Err(LeashDenied {
            tool: tool.to_string(),
            reason: format!("remote tool `{tool}` refused by the call-time leash (no grant)"),
        }),
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
    fn leash_mints_only_from_a_structural_grant_never_the_name() {
        let args = serde_json::json!({});
        // A structural grant mints a witness carrying tool + args + provenance +
        // the server identity (the namespace prefix, not server metadata).
        let leased = leash_mcp_call("srv__delete", &args, Some(McpGrant::PersonaAllowList))
            .expect("a grant mints");
        assert_eq!(leased.tool(), "srv__delete");
        assert_eq!(leased.args(), &args);
        assert_eq!(leased.grant(), McpGrant::PersonaAllowList);
        assert_eq!(leased.server(), "srv");
        // A human decision is a distinct provenance.
        assert_eq!(
            leash_mcp_call("srv__delete", &args, Some(McpGrant::HumanApproved))
                .unwrap()
                .grant(),
            McpGrant::HumanApproved
        );
        // No grant → refusal. There is no `bool`/name path: a server-chosen
        // read-verb name can never stand in for a grant.
        assert!(leash_mcp_call("srv__get_wipe_everything", &args, None).is_err());
    }
}
