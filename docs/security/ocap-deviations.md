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
| `mcp-config-admission` | untrusted/disabled MCP config cannot spawn or dial | 🟢 closed (fail-closed) | admitting an untrusted server without out-of-repo approval |
| `acp-worker-fs-scope` | model write target contained to the workspace (object-bound) | 🟠→🟡 | untrusted worker with write access on an unsandboxed host |

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

### fs-canonical-containment
- **Invariant (ideal):** the fs gate canonicalizes the target (resolving symlinks,
  e.g. via `openat2(RESOLVE_BENEATH)`) and contains it under the workspace root, so no
  path — symlink, `..`, or otherwise — escapes the fence.
- **Practical caveat (now):** `tui_permits_path` (`newt-core/src/agentic/tools.rs`)
  **lexically** normalizes the target and each root (collapsing `.`/`..`) and contains with
  a component-aware `Path::starts_with`. This closes `..` traversal (`/ws/../etc/passwd` →
  `/etc/passwd`, denied) and sibling-prefix escapes (`/ws-evil` denied against `/ws`) — both
  were reproduced on the host before the #502 review. It does NOT resolve symlinks: a symlink
  *inside* the workspace pointing out would still be read.
- **Residual:** 🟠 high — a symlink under the workspace that targets an outside path escapes
  the read/write fence (lexical normalization can't see through it). Planting one needs a
  write/exec, both separately gated.
- **Disabled while open:** seeding a shared filesystem across mutually-distrusting voices
  where one can plant a symlink the other follows; relying on the fence alone (no OS sandbox)
  for a genuinely-untrusted worker.
- **Compensating controls:** lexical containment (the #502 fix) blocks the `..`/sibling
  vectors; the crew/worker run in throwaway git worktrees; `b1`'s OS sandbox (Landlock fs) is
  the backstop that bounds the symlink residual once present.
- **Closure criterion:** the gate canonicalizes (or `openat2`-resolves) the target before
  containment, and `tui_permits_path_symlink_escape_is_the_known_residual` (`tools.rs`) is
  flipped from "permitted" to "denied" — that test already drives a real symlink under the
  workspace through the gate, so closing the deviation is exactly: implement canonicalization,
  flip the assertion.
- **Ratchet guard:** two tests in `tools.rs` — `tui_permits_path_prefix_semantics` drives
  `..`, repeated `..`, and a sibling-prefix path through the real gate and asserts denial (the
  lexical part #502 fixed; can't regress to raw `str::starts_with` without failing); and
  `tui_permits_path_symlink_escape_is_the_known_residual` pins the still-open symlink gap so it
  can't silently widen and so closing it is a visible, test-driven flip.
- **Status:** OPEN (lexical containment landed; symlink resolution pending — issue #522) ·
  review-by: with `b1` OS-sandbox work (#84)

### acp-worker-fs-scope
- **Invariant (ideal):** no production ACP/coder worker holds `fs_write = Scope::All`; every
  model-supplied write target is object-bound to the session workspace before any `join`, so a
  path with an absolute or `..` component cannot resolve outside the fence.
- **Practical caveat (now):** the default operator worker's `fs_write` is `Scope::All`
  (`newt-acp-worker/src/identity.rs`), so `permits_fs_write` admits any target; containment is
  enforced at the two shared write primitives — `apply_whole_files` (the `Emission::WholeFiles`
  landing) and `apply_patch` (the unified-diff path) in `newt-tools/src/patch.rs` — which now
  reject any target whose relative path has a non-`Normal` component (absolute / `..`) *before*
  the `join`, at their one shared owner.
- **Residual:** 🟠 high → 🟡 medium — the primitive containment closes the lexical absolute/`..`
  escape on the default write paths; a symlink *under* the workspace still resolves out (that
  residual is `fs-canonical-containment`/#522, closed by the object-bound `WorkspaceDir` resolver
  in step-52.x). The `Scope::All` caveat itself is not yet attenuated.
- **Disabled while open:** a genuinely-untrusted model/worker with write access on an unsandboxed
  host (bounded by `b1`'s OS sandbox as backstop).
- **Compensating controls:** the primitive containment (step-4.1, this deviation); the crew/plan
  write path was already contained (`is_safe_worktree_path`, `newt-cli/src/crew.rs`); throwaway
  git worktrees; the `b1` OS sandbox.
- **Closure criterion:** the default worker's `fs_write` is workspace-scoped (not `Scope::All`,
  step-4.2) AND every write target passes an object-bound `WorkspaceDir` resolve-beneath before
  the open (step-52.x), so both the lexical and the symlink escape are structurally unreachable.
- **Ratchet guard:** `apply_whole_files_rejects_path_escape`, `apply_patch_rejects_path_escape`,
  and `is_workspace_contained_rejects_escapes` (`newt-tools/src/patch.rs`) drive real absolute/`..`
  targets through the primitives and assert denial + no outside write — a regression to a raw
  `join` fails them.
- **Status:** OPEN — partial (primitive containment landed — step-4.1; full closure still needs
  both the `Scope::All` caveat attenuation (step-4.2) and the object-bound `WorkspaceDir` resolve
  (#522/step-52.x), so the invariant is not yet enforced end-to-end) · owner: — · review-by: with
  `fs-canonical-containment` (#522) + the ACP caveat fence (step-4.2)

### mcp-config-admission
- **Invariant (ideal):** repository-controlled configuration cannot cause a process spawn or a
  network dial without an approval decision made *outside* the repository. An MCP server entry
  discovered from an untrusted origin (a cloned repo's `.mcp.json`, `~/.claude.json`, or a
  walked-up project `.newt/config.toml`, all stamped `McpTrust::Untrusted` by
  `newt_core::mcp::discover`), or any entry with `enabled = false`, is refused **before** any
  transport is constructed — never spawned, never dialled.
- **Practical caveat (now):** there is no interactive "approve this untrusted server" path yet, so
  an untrusted MCP entry is *always* refused (fail-closed) rather than promotable to admitted. The
  only servers that connect are `McpTrust::Trusted` ones — a newt-owned `~/.newt/config.toml` /
  `~/.newt/mcp.toml` the operator controls outside any cloned repo. This is the conservative end of
  the invariant (invariant #9: unsupported enforcement fails closed for untrusted origins), not a
  gap.
- **Mechanism:** one gate, `newt_core::mcp::admit(&McpServerEntry) -> Result<AdmittedServer<'_>,
  AdmissionDenied>`, decides `enabled && Trusted` at a single site and returns a witness
  (`AdmittedServer`, private field — unconstructable except by a successful `admit`). The four
  public transport entry points — `newt_mcp_client::{connect_stdio, connect_http}` and both
  planners (`McpToolset::connect` headless, `newt_tui::mcp::Mcp::connect` interactive) — take
  `&AdmittedServer`, so a `connect_*` on an un-admitted entry **does not compile**: the bug is
  unrepresentable, not merely unhit. Previously the headless planner connected *every* discovered
  entry (the interactive one already checked `enabled` but not trust), so a cloned repo shipping a
  `.mcp.json` could spawn an arbitrary subprocess on first agent turn — the closed vector.
- **Residual:** 🟢 closed for the spawn/dial vector. Remaining scope is *feature*, not exposure: no
  path yet promotes an untrusted server to admitted via an out-of-repo approval (would need a
  signed operator decision, per `sod-proposer-not-worker`'s spirit). Post-admission call-time
  leashing of the connected server is the separate, still-open `mcp-under-leash`.
- **Disabled while open:** admitting an untrusted server without an out-of-repo approval (there is
  no such path — untrusted stays refused).
- **Closure criterion:** met — the gate decides at one site, the witness type makes an un-admitted
  `connect_*` uncompilable, and both the decision and the wired planner behaviour are proven by
  executable tests.
- **Ratchet guard:** `admit_denies_untrusted_and_disabled_admits_trusted`
  (`newt-core/src/mcp.rs`, mocked unit tier) proves the gate *decides* deny for untrusted + disabled
  and admit for trusted; `headless_planner_never_spawns_an_untrusted_server`
  (`newt-mcp-client/tests/headless_admission_gate.rs`, real-resource tier, grounds the mocked gate)
  drives `McpToolset::connect` over an untrusted stdio entry whose command would `touch` a marker
  and asserts the marker never appears — proving the wired planner *acts* on the deny by never
  launching the process. Neutering the gate re-creates the marker (verified red→green).
- **Status:** CLOSED (fail-closed) — step-1.1 · owner: — · review-by: revisit if/when an
  out-of-repo untrusted-server approval path is designed.

> `exec-behavior-bound`, `mcp-under-leash` — full entries to be filled as those land; each is
> **disabled-while-open bounded by `b1`** (the OS sandbox is the backstop for
> name-granularity exec and unleashed MCP until they are closed).

## 5. How to use this (for the practical-caveat moments)

When you must cut a corner to get function:
1. **Name it here** as a deviation (don't let it be silent).
2. State **what it disables** (the dangerous capability that goes fail-closed) — that *is*
   the bound; the function you keep is bounded-safe.
3. Wire the **ratchet guard** so the bound is enforced by the system, not by memory.
4. Write the **closure criterion** as a runtime check.
5. `ocap-check` then holds the line; closing the deviation later is a single ratchet click
   that unlocks the capability — convergence back to the proper OCAP vision, by construction.
