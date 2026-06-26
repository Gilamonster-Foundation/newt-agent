# Design: adversarial verification gates for the Crew / workflow system

**Status:** Design / proposed — **implementation HELD for Shawn's review** (2026-06-22)
**Related:** `docs/design/crew-swarm-overseer.md` (the overseer stack + *honest
gates*), `docs/design/workflow-swarm-harness.md` (the scheduler + the `Plan` DAG),
`docs/research/crew-vs-dynamic-workflows.md` (floor/ceiling + runner-agnostic),
`docs/decisions/mode_dispatch_topology.md` (`/mode` topology axis),
`docs/design/centaur-swarm-architecture.md` + `docs/design/captured-shell-ocap.md`
+ `docs/decisions/agentic_object_capability_security.md` (the **authority
boundary this rides on top of**), `newt-core/src/plan.rs` (`Plan`/`Subtask`/
`verify`/`CaveatPolicy`), `newt-scheduler/src/crew.rs` (harness-owned verify +
bounded retry loop), `newt-scheduler/src/panel.rs` (#468 decorrelated-voices gate).

---

## TL;DR

Adversarial verification — a decoupled, skeptical critic that tries to **refute**
the builder's output before the workflow advances — is **largely already built**
in newt. This design **generalizes the existing `Subtask.verify` field into
first-class adversarial gate kinds** and bakes the canonical adversarial-design
rules (context isolation, deterministic grounds, structured verdicts, gated
execution, circuit-breaker) in as **engine invariants** — *not* as conventions a
plan author has to remember.

It is a generalization of code that exists, not a new engine.

## What already exists (≈80%)

- **Harness-owned verify, never the model's self-report** — `workspace.run_test()`
  (`crew.rs`); *"Verification is the harness's, never the model's self-report"*
  (`crew-swarm-overseer.md`). This is *deterministic grounds* + *gated execution*.
- **The panel** (`panel.rs`, #468): the same task → N **decorrelated** voices,
  **verify-gate each, accept by agreement** — anti-groupthink adversarial review,
  already shipped.
- **Per-subtask verify** (`team.rs`, #477) and a `verify` field on `Subtask` in
  the declarative `Plan` DAG (`plan.rs`, #338).
- **A bounded self-correcting loop** — `for attempt in 1..=max_attempts` with
  triage-feedback re-attempts (`crew.rs`), exiting to `NeedsHumanReview` (the
  honest-failure path — never a silent pass). That is the *circuit breaker*.
- **Default-deny caveats** on any model-proposed stage (`plan.rs` `CaveatPolicy`
  — an omitted policy denies every axis), and the runner-agnostic `CrewRunner`
  seam + `/mode`.

## The honest correction — a gate is verifiability, not authority

An adversarial gate is a **verifiability / trust mechanism, not an authority
boundary.** Be precise, because the codebase's own doctrine refutes the stronger
claim:

- What bounds what the builder *can do* is the **per-hop caveat lattice (`meet`,
  never widen) + the OS sandbox + the harness-owned verify gate** — and those are
  **identical whether the plan is declarative TOML or imperative code**. Parsing
  the plan no more bounds the agent's *actions* than a command name bounds an
  interpreter's (`captured-shell-ocap.md`).
- What the gate *does* buy: it decides whether the builder's **output is
  trusted**, decoupled from generation.

So: **the gate is the "is this output real?" check; the cage is the caveat lattice
+ sandbox.** Two separate boundaries — the gate rides on top of the cage. No doc
or PR may present an adversarial gate as containment.

## Design — `Verify` as a sum type + engine invariants

Today `Subtask.verify` is effectively a test command. Generalize it to a sum type:

- **`Command { cmd }`** — today's harness-owned `run_test()` (a deterministic
  ground).
- **`Refute { critic_role, strategy, deterministic_ground }`** — dispatch a
  **fresh-context refuter agent** whose mission is to break the output, returning
  a **structured verdict** `{ valid: bool, exploit_vectors: [String], reason }`.
- **`Panel { n, lenses, accept }`** — expose the existing #468 panel as a gate
  kind (already built).

Gates **compose** (a stage may require a `Command` *and* a `Refute` — defense in
depth). The three canonical rules are enforced by the **scheduler**, not the plan
(the plan cannot opt out):

1. **Isolation (rule 1):** a refuter is *always* a fresh `CrewRunner` dispatch —
   new `AgentKey`, no builder history — and preferably a **decorrelated backend**
   (a different model/family, reusing the panel's strongest-per-distinct-family
   logic) so the critic doesn't inherit the builder's blind spots.
2. **Deterministic ground (rule 2):** where a real check exists (test / exploit /
   typecheck), the gate **must** run it; the refuter agent *augments*, never
   *replaces*, the deterministic check. No "the critic read it and guessed."
3. **Structured verdict (rule 3):** a typed payload read by the engine to advance
   or block — never free text the model re-interprets.
4. **Circuit breaker:** `max_retries` + a declared **token budget**; on exhaustion
   → `NeedsHumanReview` (generalize the existing `crew.rs` loop). Never a silent
   pass.

## Self-correcting loop

Builder → gate → on fail, feed the refuter's `exploit_vectors` / `reason` back
into the next attempt, bounded by `max_retries`. This is the existing `crew.rs`
retry loop with the feedback source generalized from "test failure" to "refuter
verdict" — a small change, not a new loop.

## OCAP discipline (the gate rides on top of the cage)

- The refuter dispatches with **attenuated, default-deny, typically read-only
  caveats** (`plan.rs CaveatPolicy`) — it inspects and runs sandboxed checks, it
  does not mutate the workspace; `meet` guarantees it cannot amplify.
- The **verdict is harness-owned**, gated by the engine — never the builder's
  self-report (`crew-swarm-overseer.md` *honest gates*).
- Crew **dispatch is an amplify** and already rides the `crew_attest` decision
  surface; a gate adds no new authority — it only *withholds trust* on failure.
- Authoring: a model-proposed plan may include a gate (default-deny keeps it
  safe), but the gate's caveats are **clamped to the ceiling by the parent**
  (separation of duties — a worker never writes its own ceiling).

## Runner-agnostic — floor vs ceiling

The gate compiles to `CrewRunner` dispatches. Default to the **floor** (ephemeral,
decorrelated local critics, `/mode crew`); a **resident red-team** crew member is
the **ceiling** (`/mode mesh|remote`) for projects that want a standing adversary.
The `Plan` stays pure (no residency fields); `/mode` picks where the critic runs
(`crew-vs-dynamic-workflows.md`).

## The declarative surface

```toml
[[subtask]]
id = "patch-auth"
instruction = "Fix the auth bypass in controllers/auth.js"

  [subtask.verify]
  kind = "refute"                          # command | refute | panel  (composable)
  critic_role = "pentester"
  deterministic_ground = "npm run test:security"   # rule 2: a real check, not vibes
  max_retries = 3
  # engine enforces: fresh decorrelated dispatch (rule 1), structured verdict
  # (rule 3), read-only caveats on the critic, NeedsHumanReview on exhaustion.
```

Declarative (parseable, pre-verifiable, runner-agnostic) — consistent with newt's
decided **Plan-DAG-as-data** choice (`plan.rs`), *not* model-generated code.

## Phasing (each PR-sized; builds on existing code)

| Phase | What | Builds on |
|---|---|---|
| **P1** | `Verify` enum + `Refute` gate kind (fresh dispatch + structured verdict) | `plan.rs` `verify` field, panel dispatch, the StructuredOutput schema mechanism |
| **P2** | Engine invariants: isolation/decorrelation, deterministic-ground-required, verdict-gating, `max_retries` + budget → `NeedsHumanReview` | `crew.rs` loop, `run_test()` |
| **P3** | Self-correcting loop (refuter verdict → re-attempt) | generalize `crew.rs` triage-feedback |
| **P4** | `Panel` gate kind + decorrelated-backend selection for critics | `panel.rs` #468 |
| **P5** | Resident red-team over Mesh/Remote (the ceiling) | `CrewRunner` + `/mode` |

P1–P3 give single-critic gated self-correction on the floor — the 80%-value slice
— entirely as a generalization of code that already exists.

## Consequences / non-goals

- **Implementation is HELD pending Shawn's review of this design.** No P1 branch
  lands until the approach is signed off.
- A gate is **not** sold as containment (see *the honest correction*); reviewers
  reject any PR that frames it as an authority boundary.
- **No model-generated imperative orchestration.** Gates are declarative `verify`
  data on the `Plan`, consumed by the trusted scheduler — never a model-emitted
  script (newt deliberately chose Plan-DAG-as-data over harness-as-code).
- The deterministic ground is **preferred over** the refuter agent wherever one
  exists; the agent is for the cases a fixed check can't cover, not a replacement.
