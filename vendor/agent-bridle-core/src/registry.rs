//! The [`Registry`] — explicit-builder tool catalog + leashed dispatch.
//!
//! Explicit registration is the **default** (DESIGN §5): newt's release profile
//! is `strip=true` + `lto="thin"`, the verified real-world trigger for linker
//! DCE silently dropping an `inventory`-self-registered tool from `tools/list`.
//! A `Registry::builder().tool(...).build()` is immune because every tool is
//! referenced by an explicit anchor symbol. We deliberately do **not** use
//! `inventory` in P0.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::{
    AxisEnforcement, CallRequest, Caveats, CountBound, DischargeProvider, DischargeVerifier,
    EnforcementFloor, Gate, Invocation, SessionId, StepUpPolicy, Tool, ToolContext, ToolError,
    ToolResult,
};

/// The **shared, unforgeable mutable call budget** carried by a [`Grant`]
/// (AB-001, #264; issuer-binding hardening for v0.8).
///
/// Every clone of a grant shares *this* cell via [`Arc`], and the budget lives
/// **inside the grant**, not in per-`Registry` state keyed by a forgeable id.
/// So a grant's `max_calls` **cannot regenerate**: not by cloning the grant, not
/// by dispatching it through a *different* [`Registry`], and not by recreating a
/// session — every dispatch decrements the same cell. (The earlier design kept
/// the budget in a per-Registry `HashMap<GrantId, _>` with create-on-first-use,
/// so the *same* grant crossing to a second Registry — whose map lacked that id
/// — silently minted a fresh budget. That regeneration is now impossible.)
///
/// `None` ⇒ unlimited; `Some(n)` ⇒ `n` calls remaining. The `Mutex` makes the
/// check-and-decrement atomic, so concurrent dispatches on one grant cannot
/// overspend. Its constructor is private, so a caller cannot forge a budget.
#[derive(Debug)]
struct GrantBudget {
    remaining: Mutex<Option<u64>>,
}

impl GrantBudget {
    fn new(max_calls: CountBound) -> Self {
        Self {
            remaining: Mutex::new(match max_calls {
                CountBound::AtMost(n) => Some(n),
                CountBound::Unlimited => None,
            }),
        }
    }

    /// Charge one call. `Unlimited` never exhausts; `AtMost(0)` (or a spent
    /// budget) is a fail-closed [`ToolError::Budget`]. Held under the lock so
    /// concurrent charges cannot overspend.
    fn charge(&self) -> ToolResult<()> {
        let mut remaining = self.remaining.lock().expect("grant budget mutex poisoned");
        match &mut *remaining {
            None => Ok(()),
            Some(0) => Err(ToolError::Budget),
            Some(n) => {
                *n -= 1;
                Ok(())
            }
        }
    }

    /// Return a charge that did not result in an admitted call (a pre-run denial).
    fn refund(&self) {
        let mut remaining = self.remaining.lock().expect("grant budget mutex poisoned");
        if let Some(n) = remaining.as_mut() {
            *n += 1;
        }
    }
}

/// A minted grant: authority ([`Caveats`]) bound to a shared, unforgeable
/// [`GrantBudget`]. Pass it to [`Registry::dispatch`] and **reuse it across
/// calls** so the `max_calls` budget persists — mint it once per session
/// (`Registry::mint_grant`), not per call (a fresh grant per call gives per-call
/// budget semantics). Cloning a grant shares the *same* budget, and the budget
/// travels with the grant, so it cannot regenerate across Registries or sessions.
#[derive(Clone, Debug)]
pub struct Grant {
    caveats: Caveats,
    budget: Arc<GrantBudget>,
}

impl Grant {
    /// The authority this grant carries.
    #[must_use]
    pub fn caveats(&self) -> &Caveats {
        &self.caveats
    }
}

/// Optional step-up enforcement wired into [`Registry::dispatch`] (ADR 0018 R2 /
/// ADR 0007). When present, dispatch runs the gate's step-up ceremony
/// (`evaluate → obtain → authorize_with_discharge`) instead of a plain
/// `authorize`, so a host-designated HIGH-consequence call demands a human
/// gesture on the **default** path — and even while *unbridled* (the human gate
/// is orthogonal to the capability axis, ADR 0018 D8). A refused/failed gesture
/// is a fail-closed denial; nothing is minted or charged. Absent ⇒ today's
/// behavior (no gestures).
struct StepUp {
    policy: StepUpPolicy,
    provider: Arc<dyn DischargeProvider + Send + Sync>,
    verifier: Arc<dyn DischargeVerifier + Send + Sync>,
    /// The lifetime-unique session identity every dispatched gate binds its
    /// step-up challenges to (see [`SessionId`]). Supplied by the host when it
    /// wires step-up, so a discharge obtained under this registry's session
    /// cannot be replayed into a different registry/session or a recreated one.
    session: SessionId,
}

/// A catalog of tools that dispatches through the leash.
///
/// Each [`Registry::dispatch`] looks up the named tool, has a fresh [`Gate`]
/// authorize it against the supplied grant (the single mint site), then runs
/// it. A registry has no ambient authority of its own — all authority flows in
/// per-dispatch as the `granted` caveats.
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    /// The causal generation dispatched gates embody. Defaults to 0; set via
    /// [`RegistryBuilder::generation`]. A *counter*, never a clock.
    generation: u64,
    /// Optional step-up enforcement on the dispatch path (`None` ⇒ today's plain
    /// authorize). Set via [`RegistryBuilder::step_up`].
    step_up: Option<StepUp>,
    /// Monotonic single-use nonce counter for the step-up ceremony. Core is
    /// rng-less; a per-registry counter is single-use *across* dispatches, which
    /// is what anti-replay needs *within* a session — the gate binds
    /// `challenge(session, action, generation, nonce)`, so a fresh nonce makes a
    /// captured discharge invalid on any later call. The `session` component (see
    /// [`StepUp`]) additionally invalidates it across *different* sessions and
    /// recreated registries. (A host wanting unpredictable nonces runs its own
    /// ceremony.)
    step_up_nonce: AtomicU64,
}

impl Registry {
    /// Start building a registry with explicit tool registration.
    #[must_use]
    pub fn builder() -> RegistryBuilder {
        RegistryBuilder::default()
    }

    /// The MCP `tools/list` payload: one object per tool with `name`,
    /// `description`-free `inputSchema`. (Descriptions are a frontend concern.)
    #[must_use]
    pub fn tool_definitions(&self) -> Vec<serde_json::Value> {
        self.tools
            .values()
            .map(|t| {
                serde_json::json!({
                    "name": t.name(),
                    "inputSchema": t.schema(),
                })
            })
            .collect()
    }

    /// The set of registered tool names (sorted). Used by the CI presence test.
    #[must_use]
    pub fn tool_names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// Whether a tool is registered under `name`.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Mint a [`Grant`] from `caveats`. **Reuse the returned grant across calls**
    /// so the `max_calls` budget persists — mint once per session, not per call
    /// (AB-001, #264). Two grants minted from equal caveats carry **independent**
    /// budgets (each mint allocates its own [`GrantBudget`] cell); a *clone* of one
    /// grant shares that grant's budget.
    #[must_use]
    pub fn mint_grant(&self, caveats: Caveats) -> Grant {
        let budget = Arc::new(GrantBudget::new(caveats.max_calls));
        Grant { caveats, budget }
    }

    /// Dispatch `name` with `args`, enforced by the leash, charging `grant`'s
    /// persistent call budget.
    ///
    /// The `grant` is authorized (minting the [`crate::ToolContext`] the tool
    /// needs); if authorization is denied, the tool never runs and nothing is
    /// charged. Reuse the same grant across calls for a cross-dispatch budget.
    pub async fn dispatch(
        &self,
        name: &str,
        args: serde_json::Value,
        grant: &Grant,
    ) -> ToolResult<serde_json::Value> {
        self.dispatch_axis(name, args, grant, EnforcementFloor::DEFAULT)
            .await
    }

    /// Dispatch `name` with an explicit minimum confinement strength (the
    /// **scalar** form: filesystem always Kernel, exec/net take `strength_floor`,
    /// via [`EnforcementFloor::from_scalar`]). A confined executor that wants the
    /// exec axis accepted at the interceptor tier should call
    /// [`Self::dispatch_with_enforcement_floor`] with
    /// [`EnforcementFloor::CONFINED`] instead of a blanket scalar `Kernel`.
    pub async fn dispatch_with_strength_floor(
        &self,
        name: &str,
        args: serde_json::Value,
        grant: &Grant,
        strength_floor: AxisEnforcement,
    ) -> ToolResult<serde_json::Value> {
        self.dispatch_axis(
            name,
            args,
            grant,
            EnforcementFloor::from_scalar(strength_floor),
        )
        .await
    }

    /// Dispatch `name` with an explicit **per-axis** confinement floor.
    ///
    /// This is the strong-principal form of [`Self::dispatch`]. The selected
    /// floor is stamped into the unforgeable [`crate::ToolContext`] at the
    /// gate's mint site and follows delegated trusted-worker requests. A
    /// subprocess boundary then refuses to launch if any restricted axis would
    /// fall below its per-axis floor — with no fallback to a weaker backend for a
    /// restricted axis. This closes the gap between a host's prospective
    /// enforcement check and the backend actually governing execution.
    pub async fn dispatch_with_enforcement_floor(
        &self,
        name: &str,
        args: serde_json::Value,
        grant: &Grant,
        floor: EnforcementFloor,
    ) -> ToolResult<serde_json::Value> {
        self.dispatch_axis(name, args, grant, floor).await
    }

    async fn dispatch_axis(
        &self,
        name: &str,
        args: serde_json::Value,
        grant: &Grant,
        floor: EnforcementFloor,
    ) -> ToolResult<serde_json::Value> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::not_found(name))?
            .clone();

        // AB-001 (#264): charge the PERSISTENT per-grant budget first. Under the
        // ledger lock, so concurrent dispatches on one grant cannot overspend.
        grant.budget.charge()?;

        // Authorize (generation / step-up / strength-floor). A pre-invoke error
        // means the tool never ran, so refund the charge unconditionally.
        let cx = match self.authorize_grant(tool.as_ref(), grant.caveats(), name, floor) {
            Ok(cx) => cx,
            Err(e) => {
                grant.budget.refund();
                return Err(e);
            }
        };

        // Budget is spent only on an *admitted* call that actually RAN. A policy
        // denial performed no effect, so it costs nothing, in either of its two
        // shapes — and the Registry learns which from the TYPED outcome, never by
        // scraping the result JSON (AB-001 review):
        //   * `Err(ToolError::Denied)` — a hard leash refusal; or
        //   * `Ok(Invocation::Denied(_))` — a tool (the shell family) whose
        //     public contract represents a *pre-run* refusal as an in-band `Ok`
        //     envelope. A *mid-run* denial (e.g. an egress-proxy host refusal on
        //     a child that DID spawn) is `Invocation::Ran` and stays charged —
        //     which a `"denied": true` scrape could never distinguish.
        // A tool that ran and failed (`ToolError::Exec`/`Other`) or timed out
        // stays charged.
        match tool.invoke_accounted(args, &cx).await {
            Ok(outcome) => {
                if outcome.is_denied() {
                    grant.budget.refund();
                }
                Ok(outcome.into_value())
            }
            Err(e) => {
                if matches!(e, ToolError::Denied { .. }) {
                    grant.budget.refund();
                }
                Err(e)
            }
        }
    }

    /// Dispatch `name` with `args` as a **stateless one-shot**: enforce
    /// `caveats.max_calls` for this single call *without* creating a persistent
    /// ledger entry (AB-001 review, #264). A one-shot has no cross-call budget to
    /// track. The grant's budget lives in the grant (dropped with the ephemeral
    /// grant), so there is no per-Registry state to leak, race, or resurrect.
    ///
    /// `AtMost(0)` denies (fail-closed); `AtMost(n≥1)` or `Unlimited` admits the
    /// one call. Accounting for the single call still honors the typed
    /// [`Invocation`] outcome, but there is no persistent balance to charge.
    pub async fn dispatch_oneshot(
        &self,
        name: &str,
        args: serde_json::Value,
        caveats: &Caveats,
    ) -> ToolResult<serde_json::Value> {
        self.dispatch_oneshot_with_enforcement_floor(name, args, caveats, EnforcementFloor::DEFAULT)
            .await
    }

    /// [`Self::dispatch_oneshot`] with an explicit **scalar** minimum floor
    /// (filesystem always Kernel, exec/net take `strength_floor`).
    pub async fn dispatch_oneshot_with_strength_floor(
        &self,
        name: &str,
        args: serde_json::Value,
        caveats: &Caveats,
        strength_floor: AxisEnforcement,
    ) -> ToolResult<serde_json::Value> {
        self.dispatch_oneshot_with_enforcement_floor(
            name,
            args,
            caveats,
            EnforcementFloor::from_scalar(strength_floor),
        )
        .await
    }

    /// [`Self::dispatch_oneshot`] with an explicit **per-axis** minimum floor.
    pub async fn dispatch_oneshot_with_enforcement_floor(
        &self,
        name: &str,
        args: serde_json::Value,
        caveats: &Caveats,
        floor: EnforcementFloor,
    ) -> ToolResult<serde_json::Value> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| ToolError::not_found(name))?
            .clone();

        // The single call's budget, checked inline (no ledger row). One call
        // never fits in `AtMost(0)`; every other bound admits exactly one.
        if matches!(caveats.max_calls, CountBound::AtMost(0)) {
            return Err(ToolError::Budget);
        }

        let cx = self.authorize_grant(tool.as_ref(), caveats, name, floor)?;
        tool.invoke_accounted(args, &cx)
            .await
            .map(Invocation::into_value)
    }

    /// Authorize `granted` for `tool` through a fresh gate — plain or via the
    /// step-up ceremony. The gate carries NO call budget: the persistent budget
    /// lives in the registry ledger ([`Self::charge_grant`]), so it is enforced
    /// across dispatches rather than reset on each per-dispatch gate.
    fn authorize_grant(
        &self,
        tool: &dyn Tool,
        granted: &Caveats,
        name: &str,
        strength_floor: EnforcementFloor,
    ) -> ToolResult<ToolContext> {
        let gate = Gate::with_budget(self.generation, CountBound::Unlimited)
            .with_enforcement_floor(strength_floor);
        match &self.step_up {
            // Step-up wired in (ADR 0018 R2): a policy-demanded gesture is
            // obtained + verified before minting; a refusal is a fail-closed
            // denial. The gate stays the single mint site.
            Some(su) => {
                // Bind this registry's session so the gate domain-separates its
                // step-up challenges — a discharge from another session (or a
                // recreated registry) cannot answer them (v0.8 replay defense).
                let gate = gate.with_session(su.session);
                let request = CallRequest::unspecified(name);
                // Fresh single-use nonce per ceremony (monotonic counter → the
                // gate's bound challenge differs each call, defeating replay).
                let mut nonce = [0u8; 32];
                let n = self.step_up_nonce.fetch_add(1, Ordering::Relaxed);
                nonce[..8].copy_from_slice(&n.to_le_bytes());
                let (cx, _attestation) = gate.authorize_step_up(
                    tool,
                    granted,
                    &request,
                    &su.policy,
                    su.provider.as_ref(),
                    su.verifier.as_ref(),
                    nonce,
                )?;
                Ok(cx)
            }
            None => gate.authorize(tool, granted),
        }
    }
}

/// Explicit builder for a [`Registry`]. The supported, DCE-proof registration
/// path.
#[derive(Default)]
pub struct RegistryBuilder {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    generation: u64,
    step_up: Option<StepUp>,
}

impl RegistryBuilder {
    /// Register a tool. A later registration with the same name replaces an
    /// earlier one.
    #[must_use]
    pub fn tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.insert(tool.name().to_string(), tool);
        self
    }

    /// Set the causal generation dispatched gates will embody (default 0).
    #[must_use]
    pub fn generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    /// Enforce **step-up** on the dispatch path (ADR 0018 R2 / ADR 0007): a
    /// policy-demanded human gesture is obtained via `provider`, verified by
    /// `verifier`, and required before the tool runs — on the default path and
    /// even while unbridled. Omit to keep today's gesture-free dispatch.
    ///
    /// `session` is a **lifetime-unique** [`SessionId`] the host supplies (core
    /// is rng-less); it domain-separates this registry's step-up challenges so a
    /// discharge obtained here cannot be replayed into a different registry or a
    /// recreated one. Mint it fresh per registry lifetime — see [`SessionId`].
    #[must_use]
    pub fn step_up(
        mut self,
        session: SessionId,
        policy: StepUpPolicy,
        provider: Arc<dyn DischargeProvider + Send + Sync>,
        verifier: Arc<dyn DischargeVerifier + Send + Sync>,
    ) -> Self {
        self.step_up = Some(StepUp {
            policy,
            provider,
            verifier,
            session,
        });
        self
    }

    /// Finish building.
    #[must_use]
    pub fn build(self) -> Registry {
        Registry {
            tools: self.tools,
            generation: self.generation,
            step_up: self.step_up,
            step_up_nonce: AtomicU64::new(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CountBound, Scope, ToolContext};

    /// A tool that records that it ran and echoes its `program` arg back, but
    /// only after the leash lets it exec that program.
    struct ProbeTool;
    #[async_trait::async_trait]
    impl Tool for ProbeTool {
        fn name(&self) -> &str {
            "probe"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn invoke(
            &self,
            args: serde_json::Value,
            cx: &ToolContext,
        ) -> ToolResult<serde_json::Value> {
            let program = args["program"].as_str().unwrap_or("");
            cx.check_exec(program)?;
            Ok(serde_json::json!({ "ran": program }))
        }
    }

    /// A tool whose only action is to cross a subprocess boundary. The fake
    /// path must never reach the OS when the requested strength exceeds the
    /// governing backend.
    struct SpawnProbeTool;
    #[async_trait::async_trait]
    impl Tool for SpawnProbeTool {
        fn name(&self) -> &str {
            "spawn_probe"
        }

        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }

        async fn invoke(
            &self,
            _args: serde_json::Value,
            cx: &ToolContext,
        ) -> ToolResult<serde_json::Value> {
            crate::ConfinedCommand::new("agent-bridle-strength-floor-must-refuse").spawn(cx)?;
            panic!("a backend downgrade must be refused before process creation")
        }
    }

    /// A tool that drives each typed-outcome shape from its `action` arg so the
    /// call-budget accounting can be exercised without a real subprocess:
    ///
    /// * `ran_ok` → `Ok(Invocation::Ran)` — executed, charge one call.
    /// * `ran_fail` → `Err(ToolError::Exec)` — executed and failed, charge.
    /// * `inband_deny` → `Ok(Invocation::Denied)` — a pre-run in-band refusal
    ///   (the shell family's `deny` shape); charge ZERO.
    /// * `hard_deny` → `Err(ToolError::Denied)` — hard leash refusal; charge ZERO.
    struct BudgetProbe;
    #[async_trait::async_trait]
    impl Tool for BudgetProbe {
        fn name(&self) -> &str {
            "budget_probe"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        async fn invoke(
            &self,
            args: serde_json::Value,
            cx: &ToolContext,
        ) -> ToolResult<serde_json::Value> {
            self.invoke_accounted(args, cx)
                .await
                .map(Invocation::into_value)
        }
        async fn invoke_accounted(
            &self,
            args: serde_json::Value,
            _cx: &ToolContext,
        ) -> ToolResult<Invocation> {
            match args["action"].as_str() {
                Some("ran_fail") => Err(ToolError::Exec(std::io::Error::other("boom"))),
                Some("hard_deny") => Err(ToolError::denied("hard leash refusal")),
                // In-band pre-run refusal: `Ok`, but structurally a denial.
                Some("inband_deny") => {
                    Ok(Invocation::Denied(serde_json::json!({ "denied": true })))
                }
                _ => Ok(Invocation::Ran(serde_json::json!({ "ran": true }))),
            }
        }
    }

    fn reg() -> Registry {
        Registry::builder().tool(Arc::new(ProbeTool)).build()
    }

    fn budget_reg() -> Registry {
        Registry::builder().tool(Arc::new(BudgetProbe)).build()
    }

    fn budget_call(r: &Registry, grant: &Grant, action: &str) -> ToolResult<serde_json::Value> {
        block_on(r.dispatch(
            "budget_probe",
            serde_json::json!({ "action": action }),
            grant,
        ))
    }

    /// Minimal no-dependency `block_on`. `agent-bridle-core` deliberately does
    /// NOT depend on tokio (the dep budget is the leanness win, DESIGN §3), so
    /// these async-dispatch tests drive the future with a tiny std-only
    /// executor. The futures here complete synchronously (no real I/O), so a
    /// noop-waker poll loop is sufficient.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        use std::task::{Context, Poll, Waker};
        // The crate forbids `unsafe`, so we use the safe `Waker::noop()`
        // (stable since 1.85) rather than hand-rolling a RawWaker vtable.
        let mut cx = Context::from_waker(Waker::noop());
        let mut fut = std::pin::pin!(fut);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(out) => return out,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn dispatch_unknown_tool_is_not_found() {
        let r = reg();
        let grant = r.mint_grant(Caveats::top());
        let err = block_on(r.dispatch("nope", serde_json::json!({}), &grant)).unwrap_err();
        assert!(matches!(err, ToolError::NotFound { .. }));
    }

    #[test]
    fn dispatch_runs_in_scope_and_denies_out_of_scope() {
        let r = reg();
        let granted = Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        };
        let grant = r.mint_grant(granted);
        let ok = block_on(r.dispatch("probe", serde_json::json!({ "program": "echo" }), &grant))
            .unwrap();
        assert_eq!(ok["ran"], "echo");

        let denied = block_on(r.dispatch("probe", serde_json::json!({ "program": "rm" }), &grant))
            .unwrap_err();
        assert!(matches!(denied, ToolError::Denied { .. }));
    }

    #[test]
    fn explicit_dispatch_strength_floor_refuses_backend_downgrade() {
        let registry = Registry::builder().tool(Arc::new(SpawnProbeTool)).build();
        // A non-empty hostname allow-list cannot be BOUNDED by any current host
        // backend (Landlock/Seatbelt express no hostname net rules), so the
        // conservative projection resolves the net axis to `Unknown`/`Unbounded`
        // and admission refuses at the SCOPE bound (L3) — reached before, and
        // strictly stronger than, the strength-floor downgrade. Either way a Kernel
        // net floor over an un-bound-able net grant must refuse before the
        // deliberately nonexistent program reaches the OS.
        let granted = Caveats {
            net: Scope::only(["example.invalid".to_string()]),
            ..Caveats::top()
        };
        let grant = registry.mint_grant(granted);
        let error = block_on(registry.dispatch_with_strength_floor(
            "spawn_probe",
            serde_json::json!({}),
            &grant,
            AxisEnforcement::Kernel,
        ))
        .unwrap_err();
        let ToolError::Denied { reason } = error else {
            panic!("backend downgrade returned the wrong error: {error:?}");
        };
        // The typed refusal names the unenforceable net axis (whether via the L3
        // scope bound — the net axis cannot be bounded — or the L4 strength floor).
        assert!(
            reason.contains("Net"),
            "denial must identify the unenforceable net axis: {reason}"
        );
    }

    /// The per-axis dispatch path threads [`EnforcementFloor::CONFINED`] to the
    /// spawn confinement check: a restricted **net** axis with no kernel net
    /// backend refuses (net floor = Kernel), and the refused admission does not
    /// consume the grant's call budget (#309 accounting preserved on this path).
    #[test]
    fn dispatch_with_axis_confined_floor_refuses_net_and_refunds() {
        let registry = Registry::builder().tool(Arc::new(SpawnProbeTool)).build();
        let grant = registry.mint_grant(Caveats {
            net: Scope::only(["example.invalid".to_string()]),
            max_calls: CountBound::AtMost(1),
            ..Caveats::top()
        });
        let call = || {
            block_on(registry.dispatch_with_enforcement_floor(
                "spawn_probe",
                serde_json::json!({}),
                &grant,
                EnforcementFloor::CONFINED,
            ))
        };
        let err = call().unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }));
        // Budget intact (the refusal was pre-spawn): the same refusal reproduces,
        // proving the AtMost(1) was not spent.
        assert!(matches!(call().unwrap_err(), ToolError::Denied { .. }));
    }

    /// AB-001 regression: a `max_calls` bound is enforced *across* dispatches on
    /// one grant. Before the GrantId ledger, each `dispatch` seeded a fresh gate
    /// from the grant, so `AtMost(2)` was silently unbounded — the third call
    /// went through. The registry-owned ledger, keyed by the unforgeable
    /// `GrantId`, now decrements a single balance per grant.
    #[test]
    fn dispatch_budget_persists_across_dispatches() {
        let r = reg();
        let grant = r.mint_grant(Caveats {
            max_calls: CountBound::AtMost(2),
            ..Caveats::top()
        });
        let call =
            || block_on(r.dispatch("probe", serde_json::json!({ "program": "echo" }), &grant));
        assert!(call().is_ok(), "first call within budget");
        assert!(call().is_ok(), "second call exhausts budget");
        assert!(
            matches!(call().unwrap_err(), ToolError::Budget),
            "third call must be denied: the AtMost(2) bound is spent"
        );
    }

    /// Two grants minted from *equal* caveats hold *independent* budgets: the
    /// ledger is keyed by grant identity, not by caveat value. Exhausting one
    /// grant must not touch the other.
    #[test]
    fn two_grants_equal_caveats_have_independent_budgets() {
        let r = reg();
        let caveats = Caveats {
            max_calls: CountBound::AtMost(1),
            ..Caveats::top()
        };
        let a = r.mint_grant(caveats.clone());
        let b = r.mint_grant(caveats);
        let echo =
            |g: &Grant| block_on(r.dispatch("probe", serde_json::json!({ "program": "echo" }), g));
        assert!(echo(&a).is_ok(), "grant A's single call");
        assert!(
            matches!(echo(&a).unwrap_err(), ToolError::Budget),
            "grant A now spent"
        );
        // Grant B is untouched by A's exhaustion.
        assert!(echo(&b).is_ok(), "grant B keeps its own independent budget");
    }

    /// A dispatch denied at authorization (out-of-scope program) must not spend
    /// budget: the charge is refunded before the error returns. A later valid
    /// call still succeeds within the original bound.
    #[test]
    fn denied_dispatch_does_not_charge_budget() {
        let r = reg();
        let grant = r.mint_grant(Caveats {
            exec: Scope::only(["echo".to_string()]),
            max_calls: CountBound::AtMost(1),
            ..Caveats::top()
        });
        let denied = block_on(r.dispatch("probe", serde_json::json!({ "program": "rm" }), &grant))
            .unwrap_err();
        assert!(
            matches!(denied, ToolError::Denied { .. }),
            "out-of-scope program denied"
        );
        // The single-call budget was refunded, so the in-scope call still runs.
        assert!(
            block_on(r.dispatch("probe", serde_json::json!({ "program": "echo" }), &grant)).is_ok(),
            "denied admission must not have consumed the AtMost(1) budget"
        );
    }

    /// AB-001 concurrency invariant: many threads racing `dispatch` on ONE grant
    /// never overspend its `max_calls`. The ledger charge is a check-and-decrement
    /// held under a single mutex, so *exactly* the budget is admitted — no more
    /// (overspend), no fewer (a lost decrement) — and the losers fail closed with
    /// `Budget`. This is the property the per-dispatch gate could not provide.
    #[test]
    fn concurrent_dispatches_on_one_grant_never_overspend() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        const THREADS: usize = 64;
        const BUDGET: u64 = 20;
        let r = reg();
        let grant = r.mint_grant(Caveats {
            max_calls: CountBound::AtMost(BUDGET),
            ..Caveats::top()
        });
        let admitted = AtomicUsize::new(0);
        let denied = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..THREADS {
                s.spawn(|| {
                    match block_on(r.dispatch(
                        "probe",
                        serde_json::json!({ "program": "echo" }),
                        &grant,
                    )) {
                        Ok(_) => {
                            admitted.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(ToolError::Budget) => {
                            denied.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => panic!("unexpected error under concurrency: {e:?}"),
                    }
                });
            }
        });
        assert_eq!(
            admitted.load(Ordering::Relaxed),
            BUDGET as usize,
            "exactly the budget is admitted — no overspend, no lost decrement"
        );
        assert_eq!(
            denied.load(Ordering::Relaxed),
            THREADS - BUDGET as usize,
            "every over-budget racer fails closed with Budget"
        );
    }

    /// AB-001 #309-A regression — the headline bug. A tool that signals an
    /// authority denial **in-band** (`Ok(Invocation::Denied)`, the shell
    /// family's `deny`/`refused_envelope` shape) must cost ZERO calls, exactly
    /// like an `Err(ToolError::Denied)`. Before the typed `Invocation` outcome
    /// the registry only refunded the `Err` shape, so an in-band denial silently
    /// consumed budget. This test fails on that old code path.
    #[test]
    fn inband_denial_does_not_charge_budget() {
        let r = budget_reg();
        let grant = r.mint_grant(Caveats {
            max_calls: CountBound::AtMost(1),
            ..Caveats::top()
        });
        let denied = budget_call(&r, &grant, "inband_deny").unwrap();
        assert_eq!(
            denied["denied"], true,
            "the in-band denial envelope surfaces"
        );
        // The AtMost(1) budget must be intact: a real call still runs.
        assert!(
            budget_call(&r, &grant, "ran_ok").is_ok(),
            "an in-band `Ok(Denied)` must not have consumed the single-call budget"
        );
    }

    /// A hard `Err(ToolError::Denied)` from inside the tool likewise costs zero.
    #[test]
    fn hard_tool_denial_does_not_charge_budget() {
        let r = budget_reg();
        let grant = r.mint_grant(Caveats {
            max_calls: CountBound::AtMost(1),
            ..Caveats::top()
        });
        assert!(matches!(
            budget_call(&r, &grant, "hard_deny").unwrap_err(),
            ToolError::Denied { .. }
        ));
        assert!(
            budget_call(&r, &grant, "ran_ok").is_ok(),
            "a hard tool denial must not have consumed the single-call budget"
        );
    }

    /// A gate-level refusal (here: a strength floor the backend cannot meet)
    /// costs zero calls — the charge is refunded before the error returns, so a
    /// later admissible call still runs within the original bound.
    #[test]
    fn gate_level_denial_does_not_charge_budget() {
        let r = Registry::builder().tool(Arc::new(SpawnProbeTool)).build();
        let grant = r.mint_grant(Caveats {
            net: Scope::only(["example.invalid".to_string()]),
            max_calls: CountBound::AtMost(1),
            ..Caveats::top()
        });
        // Kernel floor is unmet for a directly-spawned process → refused at
        // authorization, before invoke.
        let err = block_on(r.dispatch_with_strength_floor(
            "spawn_probe",
            serde_json::json!({}),
            &grant,
            AxisEnforcement::Kernel,
        ))
        .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }));
        // Budget intact: the same refusal is reproducible, proving the AtMost(1)
        // was not spent by the first (denied) admission.
        assert!(matches!(
            block_on(r.dispatch_with_strength_floor(
                "spawn_probe",
                serde_json::json!({}),
                &grant,
                AxisEnforcement::Kernel,
            ))
            .unwrap_err(),
            ToolError::Denied { .. }
        ));
    }

    /// A call that actually RAN and then failed (`Err(ToolError::Exec)`) DOES
    /// consume a call — only pre-run denials are free.
    #[test]
    fn ran_but_failed_consumes_budget() {
        let r = budget_reg();
        let grant = r.mint_grant(Caveats {
            max_calls: CountBound::AtMost(1),
            ..Caveats::top()
        });
        assert!(matches!(
            budget_call(&r, &grant, "ran_fail").unwrap_err(),
            ToolError::Exec(_)
        ));
        assert!(
            matches!(
                budget_call(&r, &grant, "ran_ok").unwrap_err(),
                ToolError::Budget
            ),
            "a run that executed and failed still spent the single-call budget"
        );
    }

    /// A successful run consumes a call (the ordinary charged path).
    #[test]
    fn successful_run_consumes_budget() {
        let r = budget_reg();
        let grant = r.mint_grant(Caveats {
            max_calls: CountBound::AtMost(1),
            ..Caveats::top()
        });
        assert!(budget_call(&r, &grant, "ran_ok").is_ok());
        assert!(
            matches!(
                budget_call(&r, &grant, "ran_ok").unwrap_err(),
                ToolError::Budget
            ),
            "the single admitted call spent the budget"
        );
    }

    /// AB-001 #309-B: a stateless one-shot dispatch keeps **no per-call state**.
    /// The Registry no longer holds any per-grant ledger (the budget lives in the
    /// grant), so a leak is structurally impossible — 1000 one-shots just run.
    #[test]
    fn oneshot_dispatch_keeps_no_per_call_state() {
        let r = budget_reg();
        for _ in 0..1000 {
            let _ = block_on(r.dispatch_oneshot(
                "budget_probe",
                serde_json::json!({ "action": "ran_ok" }),
                &Caveats::top(),
            ))
            .unwrap();
        }
        // Nothing to assert about the Registry — it carries no per-grant state.
    }

    /// A one-shot still enforces `max_calls` for its single call: `AtMost(0)`
    /// denies fail-closed; `AtMost(1)`/`Unlimited` admit the one call.
    #[test]
    fn oneshot_respects_max_calls() {
        let r = budget_reg();
        let zero = block_on(r.dispatch_oneshot(
            "budget_probe",
            serde_json::json!({ "action": "ran_ok" }),
            &Caveats {
                max_calls: CountBound::AtMost(0),
                ..Caveats::top()
            },
        ))
        .unwrap_err();
        assert!(
            matches!(zero, ToolError::Budget),
            "AtMost(0) denies one-shot"
        );
        assert!(
            block_on(r.dispatch_oneshot(
                "budget_probe",
                serde_json::json!({ "action": "ran_ok" }),
                &Caveats {
                    max_calls: CountBound::AtMost(1),
                    ..Caveats::top()
                },
            ))
            .is_ok(),
            "AtMost(1) admits the single one-shot call"
        );
    }

    /// An in-band one-shot denial surfaces its envelope.
    #[test]
    fn oneshot_inband_denial_surfaces_envelope() {
        let r = budget_reg();
        let out = block_on(r.dispatch_oneshot(
            "budget_probe",
            serde_json::json!({ "action": "inband_deny" }),
            &Caveats::top(),
        ))
        .unwrap();
        assert_eq!(out["denied"], true);
    }

    // ── Grant issuer/budget binding (v0.8): a grant's budget cannot regenerate ──

    /// A grant CLONE shares the same budget: exhausting the clone exhausts the
    /// original (the budget is one shared `Arc<GrantBudget>`, not per-value).
    #[test]
    fn cloned_grant_shares_one_budget() {
        let r = budget_reg();
        let grant = r.mint_grant(Caveats {
            max_calls: CountBound::AtMost(1),
            ..Caveats::top()
        });
        let clone = grant.clone();
        // Spend the single call via the CLONE.
        assert!(budget_call(&r, &clone, "ran_ok").is_ok());
        // The ORIGINAL is now spent too — the clone did not get its own budget.
        assert!(
            matches!(
                budget_call(&r, &grant, "ran_ok").unwrap_err(),
                ToolError::Budget
            ),
            "a cloned grant must not carry an independent budget"
        );
    }

    /// THE issuer-binding fix: a grant minted by Registry A, once spent, **cannot
    /// regenerate its budget** by being dispatched through a *different* Registry
    /// B. The budget lives in the grant, so B decrements the SAME cell. Before the
    /// fix, B's per-Registry ledger (keyed by a per-Registry id counter) had no
    /// entry for the grant and minted a fresh `max_calls` budget.
    #[test]
    fn spent_grant_cannot_regenerate_budget_in_a_second_registry() {
        let reg_a = budget_reg();
        let reg_b = budget_reg(); // a distinct Registry — its own id counter, no shared state
        let grant = reg_a.mint_grant(Caveats {
            max_calls: CountBound::AtMost(1),
            ..Caveats::top()
        });
        // Spend the single call in Registry A.
        assert!(budget_call(&reg_a, &grant, "ran_ok").is_ok());
        assert!(matches!(
            budget_call(&reg_a, &grant, "ran_ok").unwrap_err(),
            ToolError::Budget
        ));
        // Cross to Registry B with the SAME (spent) grant — must STAY spent.
        assert!(
            matches!(
                budget_call(&reg_b, &grant, "ran_ok").unwrap_err(),
                ToolError::Budget
            ),
            "a spent grant must not regenerate its budget by crossing to another Registry"
        );
    }

    /// Recreating the session/Registry does not resurrect a spent grant: the
    /// spent grant, dispatched through a freshly built Registry, is still spent.
    #[test]
    fn recreated_registry_does_not_resurrect_a_spent_grant() {
        let reg1 = budget_reg();
        let grant = reg1.mint_grant(Caveats {
            max_calls: CountBound::AtMost(1),
            ..Caveats::top()
        });
        assert!(budget_call(&reg1, &grant, "ran_ok").is_ok());
        drop(reg1); // session ends
        let reg2 = budget_reg(); // "restart" — a brand-new Registry
        assert!(
            matches!(
                budget_call(&reg2, &grant, "ran_ok").unwrap_err(),
                ToolError::Budget
            ),
            "a spent grant carried across a session restart stays spent"
        );
    }

    /// A provider whose ceremony always fails (no authenticator / human declined)
    /// — enough to prove the gesture is *demanded* and a refusal is fail-closed,
    /// without any crypto. The verifier is never reached (obtain fails first).
    struct FailingProvider;
    impl crate::DischargeProvider for FailingProvider {
        fn obtain(
            &self,
            _session: &SessionId,
            _request: &crate::CallRequest,
            _required: &crate::AttestRequirement,
            _generation: u64,
            _nonce: &[u8; 32],
        ) -> Result<crate::Discharge, String> {
            Err("test: no authenticator present".into())
        }
    }
    struct StubVerifier;
    impl crate::DischargeVerifier for StubVerifier {
        fn verify(
            &self,
            _discharge: &crate::Discharge,
            _required: &crate::AttestRequirement,
            _expected: &crate::Challenge,
        ) -> Result<(), String> {
            Ok(()) // never called in this test — the provider refuses first
        }
    }

    /// R2 (ADR 0018): a step-up policy demanding a gesture is enforced on the
    /// **default dispatch path** — a refused gesture is a fail-closed denial and
    /// the tool never runs (nothing minted/charged). Without the seam, dispatch is
    /// unchanged (covered by `dispatch_runs_in_scope_and_denies_out_of_scope`).
    #[test]
    fn step_up_policy_demands_a_gesture_on_the_default_path() {
        let policy = crate::StepUpPolicy::new(
            vec![crate::Rule {
                selector: "probe".to_string(),
                requirement: crate::AttestRequirement::passkey_recorded(),
            }],
            crate::AttestRequirement::NONE,
        );
        let r = Registry::builder()
            .tool(Arc::new(ProbeTool))
            .step_up(
                SessionId::new([7u8; 32]),
                policy,
                Arc::new(FailingProvider),
                Arc::new(StubVerifier),
            )
            .build();
        let granted = Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        };
        // The policy demands a passkey for `probe`; the provider refuses → denied.
        let grant = r.mint_grant(granted);
        let err = block_on(r.dispatch("probe", serde_json::json!({ "program": "echo" }), &grant))
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "a demanded-but-refused gesture must fail closed: {err:?}"
        );
    }

    /// R3 (ADR 0018 D8): the step-up (human) axis is **independent of authority**.
    /// Even a `top()` grant — the maximally-permissive extreme an unbridled
    /// principal carries — does not lower the step-up floor: a demanded gesture is
    /// still required. Caveats decide *whether the authority exists*; step-up
    /// decides *what gesture admits its use*. Nothing launders one into the other.
    #[test]
    fn step_up_holds_at_maximal_authority() {
        let policy = crate::StepUpPolicy::new(
            vec![crate::Rule {
                selector: "probe".to_string(),
                requirement: crate::AttestRequirement::passkey_recorded(),
            }],
            crate::AttestRequirement::NONE,
        );
        let r = Registry::builder()
            .tool(Arc::new(ProbeTool))
            .step_up(
                SessionId::new([7u8; 32]),
                policy,
                Arc::new(FailingProvider),
                Arc::new(StubVerifier),
            )
            .build();
        // top() authority does NOT bypass the demanded gesture.
        let grant = r.mint_grant(Caveats::top());
        let err = block_on(r.dispatch("probe", serde_json::json!({ "program": "echo" }), &grant))
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "maximal (top) authority must still owe the step-up gesture: {err:?}"
        );
    }

    #[test]
    fn tool_definitions_have_name_and_schema() {
        let r = reg();
        let defs = r.tool_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0]["name"], "probe");
        assert!(defs[0]["inputSchema"].is_object());
        assert!(r.contains("probe"));
        assert_eq!(r.tool_names(), vec!["probe"]);
    }
}
