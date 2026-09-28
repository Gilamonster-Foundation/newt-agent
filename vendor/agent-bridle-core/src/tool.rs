//! The [`Tool`] trait: a leashable capability.

use async_trait::async_trait;

use crate::{Caveats, ToolContext, ToolResult};

/// The typed outcome of a [`Tool`] invocation that PASSED the leash — used by
/// [`crate::Registry`] to account the per-grant call budget (AB-001, #264)
/// **without scraping the result JSON for security semantics**.
///
/// The distinction the Registry cannot recover from the returned value, but the
/// tool knows structurally:
///
/// * [`Invocation::Ran`] — the capability actually executed. It is charged one
///   call whether it succeeded, exited non-zero, timed out, or hit a
///   *mid-run* per-operation denial (e.g. an egress-proxy host refusal recorded
///   in the envelope's `denials`). A call happened, so the budget is spent.
/// * [`Invocation::Denied`] — the tool refused **before running** on authority
///   grounds and represents that refusal as an in-band `Ok` envelope (rather
///   than [`ToolError::Denied`](crate::ToolError)). Nothing executed, so it
///   costs **zero** calls — exactly like a gate-level denial.
///
/// Both variants carry the same `serde_json::Value` the public
/// [`Registry::dispatch`](crate::Registry::dispatch) surfaces to the caller;
/// the enum only tells the Registry how to charge. This is why a naive
/// `"denied": true` scrape would be wrong: a *ran-with-mid-run-net-denial*
/// envelope also carries `denied: true` yet must be charged, while a *pre-run*
/// refusal envelope must not — only the tool can tell them apart.
#[derive(Debug, Clone)]
pub enum Invocation {
    /// The capability executed; charge one call.
    Ran(serde_json::Value),
    /// The capability refused on authority grounds before running; charge zero.
    Denied(serde_json::Value),
}

impl Invocation {
    /// The result value to surface to the caller, discarding the accounting tag.
    #[must_use]
    pub fn into_value(self) -> serde_json::Value {
        match self {
            Self::Ran(v) | Self::Denied(v) => v,
        }
    }

    /// Whether this outcome is a pre-run authority denial (charge zero calls).
    #[must_use]
    pub fn is_denied(&self) -> bool {
        matches!(self, Self::Denied(_))
    }
}

impl From<serde_json::Value> for Invocation {
    /// A bare value is a [`Invocation::Ran`] — the safe default: a tool that
    /// does not represent pre-run denials in-band charges for what it returns.
    fn from(value: serde_json::Value) -> Self {
        Self::Ran(value)
    }
}

/// A capability the agent can invoke, governed by the leash.
///
/// A tool declares the authority it needs via [`Tool::required`]; the
/// [`crate::Gate`] confines the grant to `granted.meet(required)` and hands the
/// tool a [`ToolContext`] minted from that meet. The tool can only act through
/// the context's `check_*` methods, so it can never exceed what it declared or
/// what the session was granted.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The dispatch name (the key in `tools/list` and in
    /// [`crate::Registry::dispatch`]).
    fn name(&self) -> &str;

    /// The MCP `inputSchema` (JSON Schema) for this tool's arguments.
    fn schema(&self) -> serde_json::Value;

    /// The authority ceiling this tool promises to stay under.
    ///
    /// Defaults to [`Caveats::top`] — i.e. "I declare nothing special; confine
    /// me entirely by the session grant." Because the gate hands the tool the
    /// *meet* of granted-and-required, a `top` default means the tool runs under
    /// exactly the granted caveats, while a narrower declaration tightens the
    /// effective authority (and any future Landlock ruleset) even further. It is
    /// a *ceiling*, not a demand: declaring authority the grant lacks is not an
    /// error — the meet simply intersects it away, and per-operation
    /// [`ToolContext`](crate::ToolContext) `check_*` calls deny at use.
    fn required(&self) -> Caveats {
        Caveats::top()
    }

    /// Run the tool. The `cx` proves the leash was passed; the tool enforces
    /// per-operation policy by calling `cx.check_exec`, `cx.check_path_*`, etc.
    async fn invoke(
        &self,
        args: serde_json::Value,
        cx: &ToolContext,
    ) -> ToolResult<serde_json::Value>;

    /// Run the tool and report a typed [`Invocation`] outcome so
    /// [`crate::Registry`] can account the per-grant call budget correctly
    /// (AB-001, #264) — without inspecting the result JSON.
    ///
    /// The default runs [`Tool::invoke`] and reports [`Invocation::Ran`], which
    /// is correct for every tool that signals authority denials as
    /// [`ToolError::Denied`](crate::ToolError). A tool whose public contract
    /// represents a **pre-execution** leash refusal as an in-band `Ok` envelope
    /// (the shell family's `deny`/`refused_envelope`) overrides this to return
    /// [`Invocation::Denied`] for exactly those pre-run refusals, keeping the
    /// ran/denied distinction with the tool that owns the envelope. The Registry
    /// dispatches through this method and unwraps the value for callers, so the
    /// public [`Registry::dispatch`](crate::Registry::dispatch) return type is
    /// unchanged.
    async fn invoke_accounted(
        &self,
        args: serde_json::Value,
        cx: &ToolContext,
    ) -> ToolResult<Invocation> {
        self.invoke(args, cx).await.map(Invocation::Ran)
    }
}
