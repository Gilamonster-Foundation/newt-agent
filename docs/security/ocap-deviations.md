# OCAP deviation register & ratchet

**Status:** Mechanism + living register (2026-06-16). The disciplined way to take
practical caveats for function while guaranteeing a guided, *enforced* path back to the
full object-capability vision. Seeded from the captured-shell threat model
([`../design/captured-shell-ocap.md`](../design/captured-shell-ocap.md) §4) and the
authority plane of [`../design/centaur-swarm-architecture.md`](../design/centaur-swarm-architecture.md).

## 1. The mechanism

We will ship before OCAP is fully enforced. The rule that lets us do so *without lying
about security* — and it reuses the `meet`-only Caveat algebra the threat model found
sound:

> **Effective authority = `meet`( the human's grant , what the currently-verified
> invariants can actually enforce ).**

- Every **dangerous capability** declares the OCAP **invariants** it requires.
- At runtime, newt **verifies** which invariants hold (a checker returns
  `verified | absent` + evidence).
- A capability is available **iff all** its invariants verify; otherwise it is
  **fail-closed OFF**, with an honest banner (`#73` pattern) — never silently insecure.
- A **deviation** is an invariant currently *absent*. While it is open, the system
  **caveats its own authority down** (refuses the dependent capabilities) to match what it
  can actually enforce. *"The harness only stamps what it can enforce."*
- **Closing** a deviation (building + verifying the invariant) removes a self-caveat and
  **unlocks** its capabilities — raising the ceiling **toward, never above** the human's
  grant.

This is a **ratchet**: invariants close, never silently re-open; the effective ceiling
rises monotonically; **zero deviations = full OCAP = full function.** A deviation is, quite
literally, an OCAP *caveat* the system applies to itself — so the compromise is bounded **by
construction** (the dangerous path is structurally unreachable), not by discipline.

## 2. Enforcement (the ratchet gate)

A deviation is only real if the system *enforces* the bound. Two enforcement points:

- **Runtime:** at the point a dangerous capability would be exercised, check its invariants;
  if any is absent, **deny** + banner. (e.g. `seed_live_credential()` refuses unless
  `verify_b1()` passes.)
- **CI — `just ocap-check`** (the analog of `cov-ci`): **fails** if
  (a) any dangerous capability is reachable without its required invariants verified;
  (b) a registered deviation lacks a compensating control or a closure criterion;
  (c) a previously-verified invariant regressed to absent (the ratchet — no silent
  backslide); or
  (d) a capability is enabled whose deviation is still open.
  This register is the source of truth; CI makes convergence enforceable, not aspirational.

## 3. Entry format

```
### <id>
- Invariant (ideal):        <the OCAP invariant being violated>
- Practical caveat (now):   <what we actually do>
- Residual:                 <honest severity, from the threat model>
- Disabled while open:      <the dangerous capabilities fail-closed OFF — the BOUND>
- Compensating controls:    <what else bounds the residual>
- Closure criterion:        <the concrete, runtime-verifiable condition that retires it>
- Ratchet guard:            <the test/assertion that it can't widen + stays gated>
- Status / owner / review-by
```

## 4. The register (seeded — open deviations)

| id | invariant | residual | disabled while open |
|---|---|---|---|
| `b1-os-isolation` | OS isolation + egress proxy | 🔴 critical | live credentials, untrusted-remote voices |
| `disclosure-gate-live-path` | output filtered before it reaches the model | 🔴 critical | seeding any secret-bearing file readable by the worker |
| `exec-behavior-bound` | exec bound to resolved-path behavior tier | 🟠 high | (bounded by `b1`) |
| `fs-canonical-containment` | canonicalize-then-contain (`openat2`) | 🟠 high | cross-voice shared-fs seeding |
| `sod-proposer-not-worker` | cryptographic proposer ≠ worker | 🟠 high | auto-apply of any proposed policy |
| `mcp-under-leash` | MCP calls under the Caveats leash | 🟠 high | MCP tools holding/forwarding secrets |

### b1-os-isolation
- **Invariant (ideal):** uid-namespace + Landlock fs + seccomp + default-deny netns + an
  egress proxy that is the *only* egress (DNS included).
- **Practical caveat (now):** only an unconfined host `bash -c`; `sandbox_kind = none`.
- **Residual:** 🔴 critical — the in-process monitor is the only barrier; any monitor bypass
  escalates to direct token→internet exfil.
- **Disabled while open:** seeding a **live scoped credential** into the box; running a
  **genuinely-untrusted / foreign remote voice** that holds anything sensitive.
- **Compensating controls:** trusted-code-only tasks on trusted hosts; the credential stays
  *out of the box* (a broker presents it to outbound requests — the model never sees the
  value).
- **Closure criterion:** `verify_b1()` confirms the full stack present + fail-closed at
  session seed (kernel floor: Landlock-net 6.7; else a real egress proxy / netns).
- **Ratchet guard:** `seed_live_credential()` / `admit_untrusted_remote()` refuse unless
  `verify_b1()` passes; `ocap-check` asserts no caller bypasses; the verifier is re-run per
  session (no COW-cloned-pod skip).
- **Status:** OPEN · owner: — · review-by: at credential-seeding work (#84)

### disclosure-gate-live-path
- **Invariant (ideal):** *every* tool result passes a single disclosure filter before it is
  pushed into `messages` (one chokepoint at `mod.rs:1312`/`2025`).
- **Practical caveat (now):** redaction runs only on the next-turn observation/summary; the
  live tool result reaches the model raw; the filter is shape-only (7 regexes).
- **Residual:** 🔴 critical — a seeded token (or any secret a tool emits) reaches the model
  verbatim in-turn; re-encoding defeats shape-matching.
- **Disabled while open:** seeding **any secret-bearing file the worker can `read_file`/
  `cat`**.
- **Compensating controls:** keep secrets out of the box (above); for any unavoidable seeded
  secret, redact by **known value** (B3 knows the exact path), not by shape.
- **Closure criterion:** all three disclosure paths share one chokepoint; a canary value
  seeded at session start never appears in the model-facing message stream.
- **Ratchet guard:** a canary test — a known sentinel value placed at a seeded path must be
  absent from every `{"role":"tool"}` message and every summary.
- **Status:** OPEN · review-by: with B1 (they pair)

### sod-proposer-not-worker
- **Invariant (ideal):** the policy-proposing surface is cryptographically a *different,
  more-trusted* identity than the confined worker; observe-then-propose is taint-aware.
- **Practical caveat (now):** `role` is a descriptive string; observe-then-propose would
  ingest the worker's own transcript as ground truth → the worker authors its own ceiling.
- **Residual:** 🟠 high — privilege escalation by self-proposal.
- **Disabled while open:** **auto-apply** of any proposed policy (every promotion requires a
  human approval bound to the lowered-Caveats hash) and observe-then-propose over a
  *credentialed* transcript.
- **Compensating controls:** human-in-the-loop approval (no auto-apply); meet-to-ceiling caps
  absolute authority regardless of proposal.
- **Closure criterion:** a distinct proposer key (`proposer_fp ≠ worker_fp`, verified) +
  taint-aware proposal that down-weights worker-controlled transcript.
- **Ratchet guard:** the promote path refuses if `proposer_fp == worker_fp`; `ocap-check`
  asserts no auto-apply path exists.
- **Status:** OPEN

> `exec-behavior-bound`, `fs-canonical-containment`, `mcp-under-leash` — full entries to be
> filled as those land; each is **disabled-while-open bounded by `b1`** (the OS sandbox is
> the backstop for name-granularity exec, prefix-fs-fence, and unleashed MCP until they are
> closed).

## 5. How to use this (for the practical-caveat moments)

When you must cut a corner to get function:
1. **Name it here** as a deviation (don't let it be silent).
2. State **what it disables** (the dangerous capability that goes fail-closed) — that *is*
   the bound; the function you keep is bounded-safe.
3. Wire the **ratchet guard** so the bound is enforced by the system, not by memory.
4. Write the **closure criterion** as a runtime check.
5. `ocap-check` then holds the line; closing the deviation later is a single ratchet click
   that unlocks the capability — convergence back to the proper OCAP vision, by construction.
