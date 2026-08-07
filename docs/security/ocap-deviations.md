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
| `fs-canonical-containment` | object-bound fs (`openat2 RESOLVE_BENEATH`) | 🟢 closed (Linux) | (non-Linux lexical fallback) |
| `sod-proposer-not-worker` | cryptographic proposer ≠ worker | 🟠 high | auto-apply of any proposed policy |
| `mcp-under-leash` | MCP calls under the Caveats leash | 🟠 high | MCP tools holding/forwarding secrets |
| `mcp-config-admission` | untrusted/disabled MCP config cannot spawn or dial | 🟢 closed (fail-closed) | admitting an untrusted server without out-of-repo approval |
| `acp-worker-fs-scope` | worker fs attenuated to the session workspace (caveat fence + object-bound) | 🟢 closed | (fence active; non-Linux keeps the lexical-prefix fallback) |
| `acp-worker-debug-authority` | no production worker dispatches under `Caveats::top()` without a signed operator key | 🟢 closed (compile-gated) | a production build reaching the unbounded-authority fallback |

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
- **Practical caveat (now):** **step-6.1a** wired the by-VALUE `ocap::DisclosureFilter` into the
  SINGLE live tool-result chokepoint — `maybe_offload_tool_result` (`agentic/mod.rs`), which all four
  backend loops call and nothing else to make a `{"role":"tool"}` content string, including the
  early-return tools (`run_command`/`lifecycle`/`prompt_read`/`artifact_read`) the offload/spill
  redaction never touched. Threaded via a new `ChatCtx.disclosure` (`None` = inert, bit-for-bit
  unchanged). Two gaps remain: (i) the caller does not yet register the session secret into
  `ChatCtx.disclosure` at session start; (ii) the next-turn observation + summary paths still redact
  shape-only (`redact_secrets`, 7 regexes), not by value.
- **Residual:** 🔴 critical → 🟠 high — the *mechanism* now exists and is proven (canary redacted in
  every encoding at the live chokepoint), but it is inert until session-start registration lands; the
  other two paths remain shape-only.
- **Disabled while open:** seeding **any secret-bearing file the worker can `read_file`/
  `cat`** (until registration + convergence land).
- **Compensating controls:** keep secrets out of the box; the value-filter chokepoint (step-6.1a) is
  ready to redact by known value the moment B3 registers it — no longer only shape-matching.
  **step-6.2** hardened the by-value primitive itself to the full re-encoding matrix — base64
  (standard + url-safe, padded + unpadded), hex (lower + upper), percent/URL-encoding, the `\xXX` /
  `\uXXXX` string escapes, and **chunk-split** obfuscation (whitespace normalisation) — with `redact`
  now fail-closed (a split form that can't be excised inline withholds the whole text). So the moment
  registration lands, the live chokepoint catches every common exfil transform, not just raw/base64/
  hex.
- **Closure criterion:** all three disclosure paths share one **value** chokepoint; the session
  secret is registered at start; a canary seeded at session start never appears in the model-facing
  message stream in any encoding.
- **Ratchet guard:** `disclosure_chokepoint_redacts_registered_canary_in_every_encoding` (`agentic/mod.rs`,
  step-6.1a) — a registered canary embedded raw + base64 + hex in a tool result is absent from the
  chokepoint output in every encoding (`DisclosureFilter::leaks == false`), and the `None` path is
  byte-identical. The primitive's own matrix is guarded by `catches_base64url_reencoding`,
  `catches_base64_nopad_reencoding`, `catches_uppercase_hex`, `catches_percent_encoding`,
  `catches_string_escapes`, `catches_chunk_split_raw`, `catches_chunk_split_base64`,
  `redact_withholds_chunk_split`, and `redact_post_condition_holds_for_every_form` (`ocap.rs`,
  step-6.2). Follow-up guards will assert absence from the assembled `{"role":"tool"}` messages
  and every summary once registration + the observation/summary convergence land.
- **Status:** OPEN — mechanism landed (step-6.1a: live-path value chokepoint + canary guard);
  session-start registration + observation/summary value-convergence pending · review-by: with B1

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
  the backstop that bounds the symlink residual once present. **The object-bound resolver now
  exists** — `newt_core::fs_cap::WorkspaceDir` (step-52.1) resolves every path *beneath* an
  `O_DIRECTORY` root fd with `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`, so the symlink /
  `..` / absolute escape is refused by the kernel at open time. It is proven and available; the
  residual persists only because the fs tool arms have not yet been *rewired* onto it.
- **Closure criterion:** the read arms, write arms, and write primitives resolve through
  `WorkspaceDir` (step-52.2/52.3) rather than `join` + a `&str→bool` predicate, so the escape is
  structurally unreachable. Note: the register's earlier "make `tui_permits_path` canonicalize,
  flip its assertion" plan is **superseded** — `tui_permits_path` is a lexical `&str→bool`
  predicate, and having it canonicalize would re-introduce the TOCTOU (check decoupled from
  open). The correct closure binds authority to the opened object; the proof is an object-level
  test (`fs_cap_object_bound.rs`, landed here) plus each arm's own contained-open test, not a
  predicate flip. `tui_permits_path_symlink_escape_is_the_known_residual` is retired when its
  arm moves to `WorkspaceDir`, not flipped in place.
- **Ratchet guard:** `newt-core/tests/fs_cap_object_bound.rs` (step-52.1, real-fs tier) drives
  real `..`, absolute, in-tree-relative-symlink-escape, and absolute-symlink-escape paths through
  `WorkspaceDir` and asserts denial, with an explicit contrast test proving the object resolver
  denies exactly what a lexical `starts_with` admits — neutering the resolve flags fails 5 of the
  8 (verified red→green). `read_file_symlink_under_workspace_escaping_is_denied` (`tools.rs`,
  step-52.2, real-fs tier) drives a confined `read_file` over a symlink-escape path and asserts the
  read is *denied* rather than exfiltrating the outside file (fails on the pre-rewire arm — verified
  red→green). `list_dir_symlink_under_workspace_escaping_is_denied` (`tools.rs`, step-52.3, real-fs
  tier) + `read_dir_lists_contained_entries` / `read_dir_denies_a_symlink_escape_directory`
  (`fs_cap_object_bound.rs`) drive a confined `list_dir` (and the underlying `WorkspaceDir::read_dir`)
  over a symlink-escape directory and assert it is *denied* rather than enumerated (verified
  red→green — neutering the resolve flags re-fails them). `write_file`'s escape residual is now
  proven closed by `physical_symlink_escape_write_is_denied_object_bound` (`tools.rs`, step-52.4) —
  the *flip* of the old `..._mutates_under_existing_policy...` test: a confined `write_file` through a
  symlink-escape path is denied and the outside file is left unchanged (verified red→green) — plus
  `create_dir_all_makes_nested_dirs_beneath` / `create_dir_all_denies_a_symlink_escape_component`
  (`fs_cap_object_bound.rs`) ground the object-bound `mkdir -p`. `edit_file_symlink_under_workspace_escaping_is_denied`
  (`tools.rs`, step-52.5) proves BOTH the read of `existing` (which could leak an outside file's head
  on a no-match) and the write are object-bound beneath the `fs_write` root — the escape is denied and
  the outside file is left unchanged (verified red→green). `delete_file_symlink_under_workspace_escaping_is_denied`
  (`tools.rs`, step-52.6) proves the removal is object-bound via `unlinkat` on the resolved parent — a
  symlink-escape delete is denied and the outside file survives (before the rewire `remove_file`
  followed the intermediate symlink and deleted outside); `unlink_removes_a_contained_file` /
  `unlink_denies_a_symlink_escape_parent` (`fs_cap_object_bound.rs`) ground the primitive. `find`'s
  recursive-read root is now object-bound to the workspace (replacing its canonicalize-`starts_with`
  TOCTOU; `find_refuses_root_outside_workspace` + `find_does_not_follow_symlinks_out_of_workspace`
  pin it). The `newt-tools` applier primitives (`apply_whole_files` / `fuzzy` / `diffy`) read AND
  write object-bound through one shared owner (`read_contained_opt` / `write_contained` +
  `WorkspaceDir::rename`), proven by `apply_whole_files_denies_symlink_escape_object_bound`
  (step-52.7). `tui_permits_path` is now documented as a lexical *pre-filter* (renamed
  `tui_permits_path_is_a_lexical_prefilter_not_the_fence`) — the object-bound arms are the fence.
- **Status:** CLOSED on Linux (step-52.7) — every read arm, write arm, and write primitive resolves
  through `WorkspaceDir` (`openat2 RESOLVE_BENEATH`), so a symlink / `..` / absolute escape is
  refused by the kernel at the open, not adjudicated by a normalized pathname; the lexical residual
  (#502→#522) is structurally unreachable. **Residual:** the non-Linux fallback keeps the lexical
  `std::fs` path (`openat2` is Linux-only) — bounded (CI + prod are Linux; the whole `fs_cap` module
  is `#[cfg(target_os = "linux")]`), documented, and a future hardening (fail-closed-for-untrusted
  on kernels without `openat2`, invariant #9). · review-by: with `b1` OS-sandbox work (#84)

### acp-worker-fs-scope
- **Invariant (ideal):** no production ACP/coder worker holds `fs_write = Scope::All`; every
  model-supplied write target is object-bound to the session workspace before any `join`, so a
  path with an absolute or `..` component cannot resolve outside the fence.
- **Practical caveat (now):** none for the workspace-escape vector. step-4.3 ACTIVATED the fence:
  `handle_prompt_coder` passes the session workspace to `caveats_for_dispatch`, so the dispatched
  authority is `fs_read`/`fs_write = Scope::only([workspace])`, not `Scope::All`. The coder's fs
  predicates gate by PREFIX containment (`newt_core::permits_path`) against that same workspace, so
  the fence permits the coder's own in-workspace writes and denies any `..`/absolute escape — and
  every write is additionally object-bound beneath the workspace fd at the two shared primitives
  (`apply_whole_files` / `apply_patch`, `newt-tools/src/patch.rs`).
- **Residual:** 🟢 closed — the `Scope::All` default is gone at the dispatch layer (attenuated to
  the workspace, step-4.3), the coarse caveat gate now denies escapes lexically (prefix), and the
  symlink-*under*-the-workspace escape is closed by the object-bound `WorkspaceDir`
  (`fs-canonical-containment`/#522 CLOSED on Linux, step-52.7). Two independent layers now contain a
  hostile model's write: the caveat fence AND the kernel `openat2 RESOLVE_BENEATH`.
- **Disabled while open:** n/a (closed). On non-Linux the object-bound layer degrades to the lexical
  prefix gate (`fs-canonical-containment`'s portability residual); the caveat fence itself is
  platform-independent.
- **Compensating controls:** the object-bound `WorkspaceDir` fence (#522, the kernel-enforced
  containment); the prefix caveat gate shared with the interactive tool sites
  (`newt_core::permits_path`); the crew/plan write path was already contained
  (`is_safe_worktree_path`); throwaway git worktrees; the `b1` OS sandbox backstop.
- **Closure criterion:** met — the dispatched worker's `fs_write`/`fs_read` are workspace-scoped
  (not `Scope::All`), the coder gate denies escapes by containment, and every write resolves
  object-bound beneath the workspace fd. Proven by executable tests at the unit, dispatch, and
  end-to-end tiers.
- **Ratchet guard:** `apply_under_workspace_fence_permits_inside_denies_escape`,
  `apply_whole_files_denies_atomically_on_partial_scope`, `apply_unified_diff_gated_on_workspace_fence`
  (`newt-coder`, coder gate); `caveats_for_dispatch_fences_fs_to_the_session_workspace`
  (`newt-acp-worker`, the fence is workspace-scoped); `coder_dispatch_under_fence_contains_workspace_escape`
  (`newt-acp-worker` integration — an operator dispatch emitting `../escape.rs` never creates the
  file and reports a dispatch error); plus the object-bound `apply_whole_files_denies_symlink_escape_object_bound`
  et al. (`newt-tools/src/patch.rs`, #522).
- **Status:** CLOSED — dispatch-site fence ACTIVATED (step-4.3) atop object-bound containment
  (#522/step-52.7); the coder fs predicates are prefix-aware and share `newt_core::permits_path`
  with the interactive gate · owner: — · review-by: if a non-workspace write grant (e.g. `--write`)
  is ever threaded into ACP dispatch.

### acp-worker-debug-authority
- **Invariant (ideal):** a production headless worker NEVER dispatches under `Caveats::top()`. The
  only two authorities it can hold are (a) an attenuated, signed operator identity, or (b) a
  fail-closed deny-all when no key resolves. The `--allow-no-key` debug escape hatch — which
  restores the pre-#94 `top()` dispatch via `WorkerIdentity::AllowNoKey` — must be **unreachable in
  a production build**, not merely discouraged by a scary flag name.
- **Practical caveat (now):** none in a production build — the fallback is compiled out. During
  local development the `allow-no-key` Cargo feature (off by default, in both `newt-acp-worker` and
  the `newt-agent` CLI) re-enables it so key-less iteration keeps working.
- **Mechanism:** the `top()` path is behind a compile-time feature, not a runtime flag. With the
  `allow-no-key` feature OFF (the default, hence every release build):
  `WorkerIdentity::resolve(path, allow_no_key)` **ignores** `allow_no_key` and propagates any
  key-load failure (the worker refuses to start), and `WorkerIdentity::AllowNoKey` — if constructed
  at all — yields `fail_closed_caveats()` (deny-all: `Scope::none()` on every axis,
  `CountBound::AtMost(0)`), never `top()`. Only when the feature is compiled in does `resolve` fall
  back to `AllowNoKey` and `caveats_for_dispatch` return `unbounded_debug_fallback()`. A runtime
  `--allow-no-key` on a production binary is therefore inert: it parses, but changes nothing.
- **Residual:** 🟢 closed for the production-authority vector. A developer who deliberately compiles
  `--features allow-no-key` still gets `top()` — that is the intended dev affordance, and such a
  build is not a production artifact.
- **Disabled while open:** n/a (closed). The dev feature must never be enabled in a released binary
  or a CI/bench image that runs foreign models.
- **Closure criterion:** met — `top()` is structurally unreachable without a compile-time opt-in,
  proven by executable tests in both feature configurations.
- **Ratchet guard:** `allow_no_key_authority_is_compile_gated` (`newt-acp-worker/src/identity.rs`,
  unit tier — asserts `AllowNoKey.caveats_for_dispatch(..)` is deny-all `fail_closed_caveats()` with
  `!permits_fs_read`/`!permits_fs_write` when the feature is off, and `top()` when on);
  `resolve_refuses_when_path_unresolved_without_allow_no_key` and (feature-off)
  `worker_allow_no_key_is_inert_in_production_build` (`newt-agent`
  `tests/worker_cli.rs`, real-subprocess tier — a production `newt worker --allow-no-key` with an
  unresolvable key exits with "refused to start" and never prints "unbounded debug authority").
  Flipping the feature re-enables the `top()` fallback (verified both directions).
- **Status:** CLOSED (compile-gated) — step-1.3 · owner: — · review-by: if a runtime approval path
  for key-less dispatch is ever designed (it should not be).

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
  unrepresentable, not merely unhit. **step-1.2** sealed the *lower-level* constructors too —
  `StdioTransport::spawn` and `HttpTransport::connect` now also require `&AdmittedServer` (not a bare
  `&McpServerEntry`), closing an adversarial-review finding that the witness was enforced only by
  convention at the two wrapper call sites; no in-crate or downstream caller can now reach a
  spawn/dial without the witness. Previously the headless planner connected *every* discovered
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
