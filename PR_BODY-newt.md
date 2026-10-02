## Summary

Backports agent-bridle#406 (the zero Mach-lookup floor + named `mach:` grants)
and the subsequent #405 promotion (the completed native channel audit, ADR
0015 amendment E6, and the resulting `MACH_DEPUTY_AUDIT = Complete` flip) into
`vendor/agent-bridle-core`, closing newt-agent#2673 on macOS.

- **Root cause of #2673, confirmed on the Mac test runner:** on macOS, every
  confined `run_command`/MCP spawn narrows its net scope to `net: none`
  (`NetGrant::DenyAll` → `deny_all_net`, `confined_exec.rs`) regardless of the
  operator's configured permissions. Before this backport, the vendored
  `SeatbeltSandbox::resolved_authority` mapped every restricted `net` shape —
  `net: none` included — to `Unknown`, so `AdmittedFence::admit`'s L3
  scope-bound check refused every confined macOS spawn with "not decidable
  against the delegated grant ∪ declared runtime closure (L3 BOUND)". Linux
  was unaffected — a separate, independent `newt-net-guard` seccomp floor
  supplies its own Kernel witness there.
- `net: none` with zero `mach:` grants now resolves `Bounded(∅)` at the L3
  bound AND reports `Kernel` at the L4 strength floor together (never one
  without the other) — the exact shape newt's `ConstrainedExecutor` produces.
  A named `mach:` grant, a loopback scope, or a general remote-host allowlist
  are unaffected and stay `Unknown`/`Advisory`.
- `src/sandbox.rs`/`src/report.rs` take the upstream branch's content
  directly; `src/spawn.rs` is a clean 3-way merge preserving newt's own
  trusted-worker-broker patch; `tests/unix_socket_grants.rs` is the straight
  upstream diff; `src/lib.rs` gains the matching public re-exports.
- Fixed two real-resource tests in `macos_seatbelt_adversarial.rs` that pinned
  the OLD behavior (one asserted the profile must never deny `mach-lookup`,
  with its own "update the register" panic message anticipating exactly
  this), and narrowed (not closed) the `mach-xpc-ambient-deputy` entry in
  `docs/security/ocap-deviations.md`: the zero floor applies only to the
  `net_direct_denied` family (deny-all/`unix:`-only/`mach:`-only), which is
  what `workspace_confined_caveats`/`build_tool_request` already use — a
  loopback or host-granted net caveat still installs no Mach floor at all,
  unchanged.

**Status**: neither the upstream agent-bridle PR (`fix/405-macos-net-none-audit`)
nor this backport has been opened for review yet, per instruction — both are
pushed and ready. Full mechanics, the exact upstream SHAs, and the removal
condition are in `vendor/agent-bridle-core/NEWT_PATCHES.md`.

## Test plan

- `cargo check`/`cargo test -p agent-bridle-core --manifest-path vendor/agent-bridle-core/Cargo.toml --all-features`
  (gnuc, standalone): clean, 339/339 passed.
- `cargo check -p newt-core --all-features` (gnuc): clean.
- `cargo test -p newt-core --lib confined_exec` (gnuc): 24/24 passed.
- `cargo fmt --all -- --check` (gnuc): clean.
- **Mac test runner, measured red → green**, the real production spawn path
  (`ConstrainedExecutor::run`, `ExecOrigin::AgentInfluenced`):
  - **Red** (`origin/main`, unmodified): `cargo test -p newt-core --features macos-seatbelt --test macos_seatbelt_adversarial -- --ignored --test-threads=1 seatbelt_net_deny_all_runs_kernel_denied` → **FAILED**: `ConfinementUnenforceable("... Net axis is not decidable ... (L3 BOUND) (program: \"/usr/bin/nc\")")`.
  - **Green** (this branch): the same test → **ok**. A `net: none` spawn now
    runs under `SandboxKind::Seatbelt`, and the real kernel still denies the
    child's own TCP connect attempt.
  - Full adversarial suite (22 tests, after splitting the one that covered
    both the now-admits and still-refuses shapes): **22/22 passed** (19
    passed / 2 failed before the follow-up fix in this PR's second commit).
  - `cargo clippy -p newt-core --features macos-seatbelt --all-targets -- -D warnings`: clean.

Part of #2673

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
