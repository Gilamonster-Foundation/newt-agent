# #1528 B3 — proactive local-overflow compaction

Status: **implemented** (proactive guard + shared helper + Lean lifecycle proof; builds on B1 budget policy + B2 provenance
bridge now present in `feat/agent-psyche-production`).

## Problem

The Responses loop currently compacts only **reactively** — after the provider
returns a context-window 400 (B2, PR #1531/#1537). When Newt can already tell
*locally* that the next dispatch would exceed the actionable input budget (the B1
`ResponsesBudgetState::actionable_input_budget`), it should compact **before**
sending, not pay a round-trip to learn what it already knew. This is the Responses
analogue of the Chat path's `ensure_request_fits`.

## Scope

`ensure_responses_request_fits` (working name): a pre-dispatch guard that, when the
estimated request exceeds the actionable budget, runs the **same** compaction
bridge B2 already owns (`responses_compaction` + `compress`) and re-validates
before dispatch. No new compactor, no second provenance path — reuse B2's typed
bridge and B1's estimator.

## Correctness requirements (acceptance contract)

1. **Proactive compaction before a locally-known oversized dispatch** — when the
   B1 estimate exceeds `actionable_input_budget`, compact first; never dispatch a
   request already known to overflow.
2. **Same-logical-round retry** — proactive compaction retries the SAME round in
   place (the B2 P1 lesson: a recovery/compaction `continue` must not advance the
   round counter). Assert request COUNT + tools-on-the-recovered-request.
3. **Bounded no-progress handling** — if compaction cannot reduce the request below
   budget (no progress), fail closed with a bounded, non-looping refusal — never an
   unbounded compact→still-too-big→compact spin.
4. **No dispatch before provenance validation** — the rebuilt request must pass the
   B2 provenance classification/rebuild before it can be sent (fail-closed on a
   `system` item / unknown role).
5. **No dispatch above the post-bridge actionable budget** — reuse the B2 typed
   `check_post_bridge_budget` guard: after fencing, re-estimate; a fence-expanded
   request over budget performs zero dispatch and consumes no round.
6. **Final-summary proactive compaction** — the tools-disabled final summary
   request is also proactively compacted when it would overflow.
7. **Tools-disabled budget recalculation** — when tools are dropped (final summary
   or a tools-disabled retry), recompute the budget/estimate without the tool
   schemas rather than reusing the tools-enabled figure.

## Formal obligation

8. **Lifecycle proof** for the full proactive path:
   `estimate → compact → rebuild → validate → (readyToDispatch | abort)`. Modelled as
   an abstract state machine (`formal/CompactionLifecycle/Basic.lean`) proving:
   - **REACHABLE no-over-budget dispatch** — `reachable_ready_to_dispatch_within_budget`
     (via an `Inv` invariant preserved across a `Reachable` relation from an
     `Initial` estimate state); a naive one-step lemma is insufficient because
     `readyToDispatch` self-loops, so reachability is the honest claim.
   - **Actual finite termination** — `eventually_terminal` / `eventually_ready_or_abort`
     by well-founded recursion on a strictly-decreasing `measure` (not just the
     one-step `progress` lemma), with `fuel_never_increases` +
     `exhausted_validation_aborts` bounding the retry loop.
   - **Round ≠ safe-to-send** — `no_lifecycle_step_advances_round` (+ per-phase
     corollaries): NO lifecycle step advances the logical round; reaching
     `readyToDispatch` is NOT a completed round. Round advancement on a COMPLETED
     dispatch is the wider agent-turn model's job, deliberately out of this kernel
     (Problem C, Preferred shape).
   - **Non-vacuity demos** — `example`s that pin each load-bearing guard (dropping the
     estimate-budget guard, letting exhausted validation retry, or advancing the round
     each turns a proof into a build failure).

   **Landed as a Lean lib** (registered in `formal/lakefile.toml`), because that is
   what `.github/workflows/formal.yml` machine-checks on THIS branch — `lake build`,
   sorry-free, bare toolchain (no Mathlib). The `validate` phase's provenance check
   (untrusted-derived material never gains operator/model authority) is the B2
   `CompactionProvenance` kernel; this lib models the budget + round + termination
   half. **Lean proves the ABSTRACT lifecycle, not Rust refinement** — there is no
   generated oracle connecting the two (that is a B6 obligation); the Rust
   `proactive_decision` property test mirrors the same estimate-level invariants.

   **TLA+ temporal model deferred to B6.** A `spec/tla/` TLC/Apalache spec belongs on
   the integration/main branch where `spec/behavior-map.toml` + a TLA CI job live; on
   this base there is no TLA gate, and the workspace law "never commit an unchecked
   spec" forbids landing it here unchecked (so this PR adds NO `.tla` file). The Lean
   kernel proves the pure lifecycle in isolation; TLA+ (B6) composes it with dispatch,
   retries, cancellation, and the wider agent-turn protocol.

### B6 TLA+ obligation (precise)

The temporal model to land once this branch is rebased onto the
behavioral-constitution mainline:

```
Estimate
  → Compact
  → Rebuild
  → ValidateProvenance
  → ValidateBudget
  → ReadyToDispatch | Abort
  → Dispatch
  → Completed | Failed
```

Required TLA+ properties:

```
NoDispatchBeforeProvenanceValidation
NoDispatchWhenPostBridgeOverBudget
CompactionDoesNotConsumeLogicalRound
FailedDispatchDoesNotConsumeLogicalRound
OnlyCompletedRoundAdvances
CompactionAttemptsAreBounded
InstructionsAppearExactlyOnce
ToolsDisabledMeansNoSchemaOverhead
EventuallyReadyOrAbort
```

Lean owns the pure lifecycle kernel in B3 (`estimate → … → readyToDispatch | abort`,
with `reachable_ready_to_dispatch_within_budget`, `eventually_terminal`, and the
round-preservation theorems). TLA+ (B6) adds the `Dispatch → Completed | Failed`
tail — where `OnlyCompletedRoundAdvances` / `FailedDispatchDoesNotConsumeLogicalRound`
are proved — that this B3 kernel deliberately leaves out (Problem C).

## Reuse discipline

- Estimator: B1 `estimate_responses_request_real_tokens` /
  `actionable_input_budget` (no second estimator).
- Bridge: B2 `responses_compaction` (role-gated, strict-parse, canonical rebuild) +
  `check_post_bridge_budget` (no second provenance path, no second budget guard).
- Compactor: the ONE `compress::compress` (no fork).

## Out of scope

- B4 successful-round usage learning (`on_round_usage`, Accepted).
- B5 strict post-compaction wire validation (mock tier) — B3 relies on B2's
  fail-closed rebuild; B5 hardens the wire shape separately.
- B6 registry rows + executable Lean/Rust oracle.
