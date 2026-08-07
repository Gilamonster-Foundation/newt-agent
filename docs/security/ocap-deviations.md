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
| `disclosure-gate-live-path` | tool-derived text value-filtered before it reaches the model, at every funnel | 🟢 closed | (a NEW model-ingress path added without routing through a funnel — guarded by the convergence audit) |
| `exec-behavior-bound` | exec bound to resolved-path behavior tier | 🟠 high | (bounded by `b1`) |
| `fs-canonical-containment` | object-bound fs (`openat2 RESOLVE_BENEATH`) | 🟢 closed (Linux) | (non-Linux lexical fallback) |
| `sod-proposer-not-worker` | cryptographic proposer ≠ worker | 🟠 high | auto-apply of any proposed policy |
| `mcp-under-leash` | every MCP call mediated at call time (witness-typed leash; no-persona ≠ unrestricted) | 🟠→🟡 | untrusted server holding/forwarding a live secret (bounded by admission + `b1` + disclosure) |
| `mcp-config-admission` | untrusted/disabled MCP config cannot spawn or dial | 🟢 closed (fail-closed) | admitting an untrusted server without out-of-repo approval |
| `acp-worker-fs-scope` | worker fs attenuated to the session workspace (caveat fence + object-bound) | 🟢 closed | (fence active; non-Linux keeps the lexical-prefix fallback) |
| `acp-worker-debug-authority` | no production worker dispatches under `Caveats::top()` without a signed operator key | 🟢 closed (compile-gated) | a production build reaching the unbounded-authority fallback |
| `config-plane-provenance` | an untrusted project `.newt/config.toml` cannot grant exec / endpoint (control-plane) authority | 🟢 closed (fail-closed) | (overlay stripped; ambient `./newt.toml` base control-plane strip = tracked follow-up) |
| `noninteractive-launch-policy` | `--non-interactive` changes interaction only; OCAP-off host exec is an explicit opt-in | 🟠→🟡 | libraries still read `NEWT_DISABLE_OCAP`/`NEWT_FULL_ACCESS` from ambient env (typed `LaunchAuthority` follow-on) |
| `p4-constrained-executor` | all attacker-influenced subprocess creation routes through one confined executor | 🔴 critical | 25 agent-exec spawn sites not yet migrated (now inventory-gated) |
| `posture-report-honesty` | every security-posture surface is DERIVED from the same `verify_*` invariants the gates enforce, never independent prose | 🟢 closed | (a posture surface that asserts a claim the verifiers don't back — a report/enforcement drift) |
| `platform-capability-ceiling` | an unsupported/unverified platform reports each guarantee as `unsupported` and REFUSES operations needing it — never a Linux-equivalent OCAP claim | 🟢 closed | (a non-Linux build silently claiming a kernel-backed guarantee it cannot provide) |

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
  unchanged). **step-6.5 LANDED session-start registration + the summary path:** both live ChatCtx
  builders — the headless driver (`agentic/driver.rs`) and the interactive TUI (`newt-tui/chat.rs`) —
  now build a session filter via `ocap::session_disclosure_filter(api_key)` (registers the live
  provider bearer value; inert when absent) and pass `Some(&filter)`, so the tool-result chokepoint
  is LIVE; and the three backend loops' `final_summary_*` outputs are value-filtered through
  `redact_model_facing` before they leave the loop. Remaining gaps: (i) the next-turn
  **observation/compaction** memory still redacts shape-only (`redact_secrets`, 7 regexes), not by
  value; (ii) **streaming/chunked** deltas printed live and non-`api_key` secrets (MCP credential
  handles, brokered/temporary tokens) are not yet registered; (iii) `ChatCtx.disclosure` is still an
  `Option` — a *future* builder could pass `None` (the "no alternate path" guarantee wants the field
  made required).
- **Residual:** 🟢 closed. **step-6.6** routed the last funnels through the registered-secret value
  filter: the observation / compaction / spill memory path (`redact_secrets` now also applies the
  by-value filter) and, via a `scoped_session_disclosure` TLS backstop installed per driven turn, the
  tool-result chokepoint and the summary path too — so even a caller that forgot the explicit
  `&DisclosureFilter` param cannot place tool-derived text into model context unfiltered. Streaming /
  error deltas are covered transitively: with every INGRESS funnel filtered the model never RECEIVES
  a registered secret, so it cannot echo one into its streamed output (the summary path filters the
  final answer regardless). The one remaining risk — a FUTURE model-ingress path added without
  routing through a funnel — is a fresh-audit obligation, not a known open hole; it is the standing
  job of the convergence audit + would be caught by the guards below going stale.
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
  step-6.2). **step-6.5:** `session_filter_registers_a_real_provider_key` +
  `session_filter_ignores_trivial_or_absent_key` (`ocap.rs`) prove the live registration; the summary
  redaction is `redact_model_facing` at the three `final_summary_*` returns (`agentic/mod.rs`).
- **Status:** CLOSED — step-6.6. `verify_disclosure_gate()` now returns `Verified`: the session
  secret is registered at start (`session_disclosure_filter`, both live builders) and value-filtered
  at every model-ingress funnel (tool-result chokepoint, summary, memory/observation/compaction/spill)
  via the explicit param + the TLS backstop, proven by
  `no_model_ingress_funnel_leaks_a_registered_session_secret`,
  `redact_secrets_value_filters_a_registered_session_secret`,
  `session_tls_redacts_installed_secret_and_restores`, and the step-6.1a/6.2 chokepoint + matrix
  guards. Note: flipping this does NOT enable `seed_live_credential` — that still requires
  `verify_b1` (Absent). · owner: — · review-by: the convergence audit re-verifies no new bypass path.

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

### config-plane-provenance
- **Invariant (ideal):** repository-controlled configuration cannot grant executable or
  control-plane authority. A walked-up project `.newt/config.toml` (a cloned repo can ship one, so
  it is attacker-reachable, exactly like a `.mcp.json`) must not be able to run a command
  (`[[providers]]`, `[lifecycle]`), select the exec/shell backend (`[shell]`), or redirect the
  agent's inference/data endpoints (`[[backends]]`, `default_backend`, `[dgx]`, `[discovery]`) — via
  config alone. It may still pin benign, non-control-plane preferences.
- **Practical caveat (now):** none for the walked-up project-overlay vector. This generalizes the
  #1301 MCP-trust model (which already stamps a project overlay's `[[mcp_servers]]` `Untrusted`) to
  the rest of the control plane.
- **Mechanism:** control-plane authority is a **data table** — `CONTROL_PLANE_KEYS`
  (`newt-core/src/config.rs`) — and the raw `merge_toml` of the project overlay is replaced by
  `merge_project_overlay`, which `strip_control_plane`s the overlay at the `toml::Value` layer
  *before* `try_into::<Config>()`. A stripped key therefore fails closed to the trusted base's value
  (or the built-in default), never the attacker's. `mcp_servers` is deliberately left to its finer
  literal-only untrusted gate (`mark_project_mcp_untrusted`), not blanket-stripped.
- **Residual:** 🟢 closed for the walked-up `.newt/config.toml` overlay. Remaining scope is *feature*
  (a `newt config adopt` path so an operator can opt a repo's control-plane keys back in) and one
  sibling vector: the ambient `./newt.toml` **base** (`cd repo && newt`) — already MCP-downgraded by
  #1301 but its control-plane keys are not yet stripped. Tracked as the follow-up; cross-referenced
  here so it is not mistaken for closed.
- **Disabled while open:** n/a (closed for the overlay vector).
- **Closure criterion:** met — the project overlay's control-plane keys are stripped before
  deserialize, proven on the real `Config::resolve()` path.
- **Ratchet guard:** `untrusted_project_overlay_cannot_contribute_control_plane_keys`
  (`config.rs`, pure unit — the strip at the merge seam) and
  `walked_up_project_config_cannot_grant_control_plane_authority`
  (`newt-core/tests/config_project_trust.rs`, real-resource `#[serial]` — plants a walked-up
  `.newt/config.toml` with an RCE provider + lifecycle command + host shell + exfil endpoint, runs
  real `Config::resolve()`, asserts every control-plane key is absent from the resolved config while
  a benign `[context]` preference survives). Neutering `strip_control_plane` re-admits them.
- **Status:** CLOSED (fail-closed) — step-7.1 · owner: — · review-by: when a `newt config adopt`
  path or the ambient-base control-plane strip is designed.

### noninteractive-launch-policy
- **Invariant (ideal):** launch authority is resolved once, explicitly, and cannot be widened by an
  ambient signal after the fact. `--non-interactive` changes INTERACTION only; OCAP-off ambient host
  execution is an unmistakable explicit opt-in; authority may attenuate but never widen because an
  environment variable later appears; child processes never inherit the authority switches.
- **Practical caveat (now):** the sharpest vector is closed. `newt solve` previously defaulted to the
  OCAP-**off** full-access Yolo lane purely because `--non-interactive` defaults to true
  (`resolve_lane(false, None, /*non_interactive*/ true) == Yolo`, which set `NEWT_FULL_ACCESS=1` +
  `NEWT_DISABLE_OCAP=1`). **step-3.1** decoupled them: the lane no longer consults `--non-interactive`
  at all; OCAP-off requires the explicit `--unsafe-host-exec` flag (or the `NEWT_UNSAFE_HOST_EXEC` env
  twin), `--confined` still wins, and the **default lane is now `Confined`** (OCAP on, workspace-
  fenced). A plain `newt solve --non-interactive` is confined.
- **Residual:** 🟠 high → 🟡 medium. The remaining architectural work: (i) libraries still READ
  `NEWT_DISABLE_OCAP`/`NEWT_FULL_ACCESS` from the ambient process env (`ocap_disabled` /
  `full_access_requested` in `newt-core/agentic/tools.rs`), so a later-appearing env var could still
  widen authority mid-process — the fix is a typed, immutable `LaunchAuthority`/`ExecutionPolicy`
  resolved once at startup and threaded by value instead of consulted from libraries; (ii) stripping
  the authority switches from every child process is folded into `p4-constrained-executor` (the
  cleared-env boundary). Until then the Yolo lane still *sets* those env vars for its own
  (now-explicit) use.
- **Disabled while open:** n/a for the closed vector; the typed-authority + child-env-strip work is
  tracked here + under `p4-constrained-executor`.
- **Closure criterion (the `--non-interactive` invariant):** met — `--non-interactive` cannot select
  the OCAP-off lane; only an independent explicit `--unsafe-host-exec` can. Full closure of the row
  needs the typed immutable `LaunchAuthority` retiring the ambient env reads.
- **Ratchet guard:** `non_interactive_never_relaxes_authority` (`newt-cli/src/solve.rs`) — a plain
  headless run (`resolve_lane(false, None, false)`) resolves to `Confined`, never `Yolo`; only an
  explicit opt-in (`resolve_lane(false, None, true)`) reaches `Yolo`. Reverting the decouple (letting
  `--non-interactive` pick Yolo) turns this red.
- **Status:** OPEN (narrowed) — the `--non-interactive`-disables-OCAP vector CLOSED (step-3.1, with
  the regression guard); the typed `LaunchAuthority` + ambient-env-read retirement remain · owner: —
  · review-by: with the `LaunchAuthority` + `p4-constrained-executor` work.

### p4-constrained-executor
- **Invariant (ideal):** every attacker-influenced subprocess (model shell, build checks, tests,
  formatters, lifecycle hooks, crew ops, MCP stdio, provider/plugin processes, and any git/helper
  that can run repository-authored code) is created through ONE `ConstrainedExecutor` that receives
  explicit origin/trust class, executable + argv, cwd, fs / net / env grants, credentials, timeout,
  and process budget — clearing inherited env, denying net by default, fencing fs to the workspace,
  killing the process tree on cancel, and FAILING CLOSED when the required confinement cannot be
  established. No raw `Command`/`sh -c` bypass may remain for repo-controlled execution.
- **Practical caveat (now):** the confined-spawn seam is now **owned by newt** —
  `newt_core::confined_exec::ConstrainedExecutor` (**step-4.2**). It wraps the audited
  `agent_bridle::ConfinedCommand::spawn` (the same primitive `newt-mcp-client` uses for MCP stdio) and
  adds the fail-closed contract: an `ExecOrigin::AgentInfluenced` request is minted under a **`Kernel`
  strength floor**, so `spawn` REFUSES (`confinement_unenforceable`) whenever the fs fence cannot be
  kernel-enforced — on a kernel without Landlock, or any platform without an OS fs backend — instead
  of running the child unconfined. The child starts env-EMPTY (only explicit grants cross), and
  `workspace_confined_caveats` fences fs read/write to the workspace with an **empty `net`
  allow-list** (kernel deny-all). A real-resource Landlock adversarial test
  (`newt-core/tests/confined_exec_landlock.rs`) proves a hostile child under this executor cannot
  write outside the workspace, read outside it, inherit a parent credential env var, or open a network
  connection — by kernel denial where Landlock is present, by refusal where it is not.
  **step-4.1** already landed the automated no-bypass GATE (`scripts/spawn_inventory.py` +
  `docs/security/spawn-inventory.toml`, CI-wired). **step-4.3** migrated the FIRST attacker-influenced
  site — the repo-configured `build_check_cmd`: `run_build_check` no longer runs a raw `sh -c` on the
  host, it routes through `ConstrainedExecutor` with `build_tool_caveats` (open reads, workspace-only
  writes, `TMPDIR` in-workspace, net deny-all) and fails closed off the kernel fence. **21 sites
  remain classed `agent-exec-todo-p4`** — `crew.rs` (formatters/tests), `newt-tui/lib.rs` (roadmap
  verify + git/gh reads to reclassify), and the `agentic/tools.rs` run_command HOST-SHELL yolo lane
  (an explicit `--disable-ocap`/`--full-access` opt-out, env-scrubbed; its #8 authority-env hardening
  is a later slice, not a confinement migration).
- **Residual:** 🔴 critical → 🟠 high. The executor + its kernel-backed enforcement now EXIST and are
  proven (step-4.2); the residual is the *migration* — the 25 raw `agent-exec-todo-p4` sites are not
  yet routed onto it, so `build_check_shell` et al. still spawn raw `sh -c` until each is migrated.
  The seam being fail-closed means the migration lands real confinement (not advisory) the moment a
  site moves onto it.
- **Disabled while open:** running genuinely-untrusted repository code on an unsandboxed host
  (bounded by `b1`'s OS sandbox as the backstop, itself still open).
- **Closure criterion:** the `spawn-inventory` shows ZERO `agent-exec-todo-p4` sites (all routed
  through `ConstrainedExecutor`), and a hostile-child adversarial test proves a build/test/plugin
  cannot read/write outside the workspace, reach the network unauthorized, read parent credentials,
  or leave surviving descendants. The adversarial-test half is **met** (step-4.2); the routing half
  is the remaining migration.
- **Ratchet guard:** `scripts/spawn_inventory.py` (self-tested; CI-gated) — a new or moved
  `Command`/`process::Command` site fails the build until it is inventoried + classified; plus
  `newt-core/tests/confined_exec_landlock.rs` (real-resource) + the `confined_exec` unit tests, which
  fail if the executor stops confining or stops failing-closed.
- **Status:** OPEN — inventory gate (step-4.1) + fail-closed executor seam & kernel-backed
  adversarial proof (step-4.2) landed; the executor migration of the 25 agent-exec sites remains ·
  owner: — · review-by: as each migration slice lands.

### mcp-under-leash
- **Invariant (ideal):** every individual MCP tool call is mediated at CALL time before it reaches
  the wire — admission (`mcp-config-admission`) decides *which* servers may connect; this is the
  per-call counterpart. Authority is a leash, not a blanket: an operation the session did not
  authorize does not dispatch, and "no persona" is NOT "unrestricted".
- **Practical caveat (now):** the LEASH invariant is enforced. `McpTools::call` requires a
  `LeasedMcpCall` witness (`newt-core/src/agentic/mcp.rs`, private field, minted only by
  `leash_mcp_call`), so an un-leashed dispatch does not type-check — structurally, like
  `mcp-config-admission`'s `AdmittedServer`. At the sole dispatch choke
  (`agentic/tools.rs`, `execute_tool_inner`) the grant is computed and the witness minted: the
  persona allow-list path is unchanged (allow-listed dispatches; out-of-list is prompted, a deny
  hard-stops), and the previously-**unleashed** no-persona path is closed — a read-class tool (by
  `classify_mcp_effect`, a droppable name convention, NEVER the server's own `readOnlyHint`) passes,
  a mutating/unknown one is prompted (interactive) or **denied fail-closed** (headless).
- **Residual:** 🟠 high → 🟡 medium. The leash mediates *dispatch*; two residuals remain, both
  cross-referenced, NOT claimed closed here:
  1. **secret-forwarding** — an admitted server handed a value could still exfiltrate it. That is a
     disclosure/egress concern bounded by `disclosure-gate-live-path` + `b1-os-isolation`, not by
     this leash.
  2. **name-based effect classification is server-influenceable** — a hostile server could name a
     destructive tool with a read verb (`get_…`) to earn read-class tolerance. The real containment
     for a genuinely-hostile server is admission (only trusted servers connect) + `b1` (OS sandbox),
     with the name convention as defense-in-depth. Per-call budget + resource-scope + credential-
     handle attenuation are the follow-on that tightens this.
- **Disabled while open:** admitting a genuinely-untrusted server that holds a live secret (bounded
  by `mcp-config-admission` + `b1` + `disclosure-gate-live-path`).
- **Closure criterion (LEASH):** met — an un-leashed `McpTools::call` does not compile, and the
  no-persona mutating dispatch is prompted/denied on the real dispatch path. Full closure of the row
  additionally needs the secret-forwarding residual retired (via `disclosure-gate-live-path`).
- **Ratchet guard:** `no_persona_does_not_dispatch_a_mutating_mcp_tool_unleashed`,
  `no_persona_read_class_mcp_tool_still_dispatches`,
  `no_persona_mutating_mcp_tool_dispatches_when_human_grants`,
  `remote_tool_outside_allow_list_is_prompted_not_hard_vetoed` (`agentic/tools.rs`), and
  `classify_reads_by_verb_prefix_stripping_namespace` + `leash_mints_only_when_granted`
  (`agentic/mcp.rs`). Removing the witness requirement makes the un-leashed dispatch compile again.
- **Status:** OPEN (narrowed) — LEASH invariant CLOSED (step-6.4, witness-typed call-time leash);
  secret-forwarding + name-classification residuals cross-referenced to `disclosure-gate-live-path`
  / `b1-os-isolation` · owner: — · review-by: with per-call budget/scope attenuation, or when
  `disclosure-gate-live-path` closes.

### posture-report-honesty
- **Invariant (ideal):** every place newt reports its security posture — to the user, to logs, or
  to the model — is DERIVED from the same runtime `verify_*` invariants the fail-closed capability
  gates consult, so a reporting surface can never claim more (or less) than what is actually
  enforced. No hand-written per-lane prose asserts a guarantee the verifiers do not back.
- **Practical caveat (now):** the typed **achieved-security report** exists and is derived, not
  asserted. `newt_core::ocap::SecurityReport` builds one `Achieved` entry per `Guarantee` from
  `RuntimeEvidence::current()` — the very `verify_b1` / `verify_disclosure_gate` /
  `verify_fs_object_bound` / `verify_constrained_executor` / `verify_fail_closed_execution`
  invariants the gates use — with `meet` for compound guarantees (credential/process isolation need
  BOTH the executor and `b1`). There is deliberately **no constructor that takes a free-form claim**.
  `newt doctor` renders it via `security_posture_lines(&SecurityReport::current())` (an
  "Achieved OCAP posture (per guarantee)" block), generalizing the `#1256` "report the achieved
  `SandboxKind`, never the intent" precedent from `newt mcp probe` / the `/mcp` table.
- **Residual:** 🟢 closed for the report type + the `doctor` surface. Follow-up (tracked, not
  claimed here): feed the same report into the remaining hand-written surfaces (the per-turn
  `runtime_context_block` "# Filesystem confinement" claim, the session-start banners) so they too
  read from the report instead of restating it.
- **Disabled while open:** (closed) — a posture surface asserting a claim the verifiers don't back.
- **Compensating controls:** the `verify_*` invariants remain the single source of truth for the
  fail-closed gates; the report is a pure function of them (`SecurityReport::from_parts`).
- **Closure criterion:** met — the report derives every entry from the verifiers, and the `doctor`
  render is a pure function of the report (no independent claim); a guarantee reported `enforced`
  implies its verifier is `Verified`.
- **Ratchet guard:** `linux_report_matches_live_verifier_state`, `compound_guarantees_take_the_meet`,
  `summary_lines_cover_every_guarantee_honestly` (`newt-core/src/ocap.rs`) +
  `posture_lines_are_derived_from_the_report_not_prose` (`newt-cli/src/doctor.rs`). Adding a
  free-form-claim constructor or an over-claiming render breaks these.
- **Status:** CLOSED — step-8.1 (typed `SecurityReport` derived from the verifiers, rendered by
  `newt doctor`) · owner: — · review-by: when the per-turn/banner surfaces are migrated onto it.

### platform-capability-ceiling
- **Invariant (ideal):** newt never claims a security guarantee a platform cannot provide. On a
  platform whose kernel primitives are absent or unverified (no `openat2(RESOLVE_BENEATH)`, no
  Landlock, no proven Seatbelt/AppContainer floor), each affected guarantee is reported
  `unsupported` and any operation that REQUIRES it is refused (fail-closed) — never silently
  downgraded to a best-effort path that still reports "confined".
- **Practical caveat (now):** the report takes the **meet of a pure-data platform ceiling and the
  runtime evidence**, and the ceiling never rounds up. `PlatformCeiling` is one const table per
  platform (`LINUX_CEILING` supports every guarantee; `MACOS_CEILING` / `WINDOWS_CEILING` mark the
  kernel-backed guarantees `Unsupported` with an honest reason; `UNKNOWN_CEILING` — the default
  arm — marks EVERYTHING unsupported, the opposite of a `_ => true` fail-open). Even if every
  runtime verifier were `Verified`, a ceiling entry of "cannot provide" forces `Unsupported`.
  `require_achieved(&report, guarantee)` is the refusal primitive: it returns `Ok` only on
  `Enforced` and a `FailClosed { deviation: "platform-unsupported", … }` on `Unsupported`.
- **Residual:** 🟢 closed for the reporting + refusal contract. This does NOT build the non-Linux
  kernel floors (macOS Seatbelt / Windows AppContainer) — those stay honestly `Unsupported`, which
  is the point: "Linux is the normative fully-supported OCAP platform for this milestone" (no macOS
  runner is needed to represent unsupported truthfully and fail closed). The still-open *runtime*
  fail-open of the non-Linux lexical fs fallback in `tools.rs` is tracked under
  `fs-canonical-containment` (a Linux-closed row) — this row governs the *report/refusal* honesty.
- **Disabled while open:** (closed) — a non-Linux build silently claiming a kernel-backed guarantee.
- **Compensating controls:** unrecognized platforms default to the all-`Unsupported` ceiling;
  `Achieved` has no "best effort" variant (enforced-with-evidence, open-with-deviation, or
  unsupported — nothing else).
- **Closure criterion:** met — an unsupported-platform report never marks a kernel-backed guarantee
  `Enforced`, and `require_achieved` refuses it.
- **Ratchet guard:** `ceiling_never_rounds_up`, `unknown_platform_is_fully_unsupported`,
  `require_achieved_refuses_unverified_and_unsupported`, `current_report_reflects_build_platform`
  (`newt-core/src/ocap.rs`) + `unsupported_platform_never_renders_a_linux_equivalent_claim`
  (`newt-cli/src/doctor.rs`). A permissive default arm or an `Enforced`-on-unsupported path breaks
  these.
- **Status:** CLOSED — step-8.1 (platform ceiling `meet` + `require_achieved` refusal) · owner: — ·
  review-by: when a non-Linux kernel floor lands and its ceiling can flip a guarantee off
  `Unsupported`.

> `exec-behavior-bound` — full entry to be filled as it lands; **disabled-while-open bounded by
> `b1`** (the OS sandbox is the backstop for name-granularity exec until it closes).

## 5. How to use this (for the practical-caveat moments)

When you must cut a corner to get function:
1. **Name it here** as a deviation (don't let it be silent).
2. State **what it disables** (the dangerous capability that goes fail-closed) — that *is*
   the bound; the function you keep is bounded-safe.
3. Wire the **ratchet guard** so the bound is enforced by the system, not by memory.
4. Write the **closure criterion** as a runtime check.
5. `ocap-check` then holds the line; closing the deviation later is a single ratchet click
   that unlocks the capability — convergence back to the proper OCAP vision, by construction.
