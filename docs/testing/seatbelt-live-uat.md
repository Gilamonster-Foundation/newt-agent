# Seatbelt live UAT — proving the macOS L3 jail end-to-end

A **live** acceptance plan that validates agent-bridle's macOS **Seatbelt** L3
backend — the kernel jail the whole object-capability story rests on — running
on a real developer Mac (Apple Silicon), then drives shell commands through it
from newt itself. It is the macOS analog of the Landlock proofs CI already runs
on Linux, plus the newt-side adoption pre-work tracked in
[newt#868](https://github.com/Gilamonster-Foundation/newt-agent/issues/868).

- **Status:** Phase A (agent-bridle kernel proofs) **PASSED** on owner Mac
  2026-07-04; Phases B/C (newt integration test + live headless run) are the
  post-approval plan below.
- **Tier** (see [`bat-uat.md`](./bat-uat.md)): UAT (L2/L3) — end-to-end on real
  kernel enforcement, owner-Mac-run, **not** a per-PR gate (newt CI has no macOS
  Seatbelt job; see §5).
- **Related:** agent-bridle ADR 0019 (sandboxed-host shell engine, Accepted),
  agent-bridle #194 / #199 (the engine), #196 (structured net denial, already in
  newt's pin), newt #868 (this adoption).

## 1. Why this exists

The object-capability model splits the shell **engine** (L2, convenience) from
the **boundary** (L3, the kernel jail): *the guarantee is the jail, not the
parser* (agent-bridle ADR 0005). On Linux that jail is Landlock + seccomp; on
macOS it is **Seatbelt** (`/usr/bin/sandbox-exec` with a generated SBPL profile,
agent-bridle-core `sandbox.rs`). agent-bridle's CI exercises the Seatbelt kernel
proofs on a GitHub `macos-latest` runner, but two things were never checked:

1. **A real developer Mac.** agent-bridle #199's macOS mirror test
   (`macos_dynamic_construct_runs_but_out_of_scope_write_is_seatbelt_denied`)
   was written explicitly for owner-Mac usability testing. This doc is that run.
2. **newt actually engaging Seatbelt.** newt today requests only the bridle
   `["shell", "web"]` features — **not** `macos-seatbelt` — so on macOS its
   confined shell falls back to the honest advisory posture (`sandbox_kind:
   "none"`). Nothing kernel-enforces the workspace fence on a Mac yet. Closing
   that is newt #868 item 4, and Phase B/C below are its pre-work.

A prompt: agent-bridle PR #195's macOS CI leg once failed on the net-proxy
kernel proof (`fenced_child_reaches_allowed_via_proxy…`, "Empty reply from
server"), while `main` stayed green — a flake worth chasing on real hardware.
See §4.

## 2. Environment (the machine under test)

| | |
|---|---|
| Host | Apple M4 MacBook, macOS Darwin 25.5.0 (arm64) |
| Toolchain | rustc/cargo 1.95.0 (Homebrew), just 1.51.0 |
| Sandbox | `/usr/bin/sandbox-exec` present; `/bin/sh`, `/usr/bin/curl` present |
| agent-bridle | `main` @ `bacb3b1` (ADR 0019 present) |
| newt pin | bridle git rev `195e1c7e` (#196) — 4 commits behind bridle `main` |

## 3. Phase A — agent-bridle Seatbelt kernel proofs (DONE, PASSED)

Ran the exact `check-macos` CI job commands (`.github/workflows/ci.yml`
`check-macos`, `RUSTFLAGS="-D warnings"`) on the machine above:

```sh
cargo clippy --workspace --exclude agent-bridle-py --all-targets --all-features -- -D warnings
BRIDLE_REQUIRE_SEATBELT=1 cargo test --workspace --exclude agent-bridle-py --all-features
cargo test -p agent-bridle-tool-shell --no-default-features --features host-shell   # #199 isolation corner
```

`BRIDLE_REQUIRE_SEATBELT=1` turns a missing `sandbox-exec` from a silent skip
into a hard failure, so a green run **guarantees the kernel boundary was
actually exercised** (`proof_gate_required_but_unsupported_is_a_failure`
verifies the gate itself).

**Result: all green — 0 failed, 0 ignored, 0 skipped** across the workspace.
The load-bearing proofs (each spawns a real `sandbox-exec` child and asserts
kernel denial — e.g. a forbidden `touch` returns EPERM "Operation not
permitted", the file never appears):

| Proof | Axis | Result |
|---|---|---|
| `seatbelt_kernel_tests::fs_write_is_kernel_enforced_outside_scope_denied_inside_allowed` | fs_write | ✅ |
| `seatbelt_kernel_tests::empty_fs_write_scope_denies_all_writes` | fs_write | ✅ |
| `seatbelt_kernel_tests::fs_read_is_kernel_enforced_outside_scope_denied_inside_allowed` | fs_read | ✅ |
| `seatbelt_kernel_tests::exec_allowlist_permits_listed_denies_unlisted_child` | exec | ✅ |
| `seatbelt_kernel_tests::granted_shell_cannot_exec_any_unlisted_child` | exec | ✅ |
| `seatbelt_kernel_tests::granted_interpreter_cannot_trampoline_to_unlisted_binary` | exec | ✅ |
| `seatbelt_kernel_tests::exec_confinement_does_not_break_dynamic_linking` | exec | ✅ |
| `seatbelt_kernel_tests::net_fully_denied_kernel_blocks_egress` | net | ✅ |
| `seatbelt_kernel_tests::net_loopback_only_permits_loopback_interface_denies_offbox` | net | ✅ |
| `net_proxy::…::fenced_child_reaches_allowed_via_proxy_denied_refused_direct_kernel_blocked` | net (proxy fence) | ✅ |
| `real_spawn::real_seatbelt_confines_a_spawned_childs_own_write` | fs (through `ShellTool`) | ✅ |
| `real_spawn::real_seatbelt_confines_a_spawned_childs_own_read` | fs (through `ShellTool`) | ✅ |
| `real_spawn::real_seatbelt_wrapped_pipeline_pipes_data_between_stages` | pipeline | ✅ |
| `real_spawn::real_seatbelt_denies_egress_when_net_is_empty` | net (through `ShellTool`) | ✅ |
| `host_shell_real::macos_dynamic_construct_runs_but_out_of_scope_write_is_seatbelt_denied` | **ADR 0019 keystone** | ✅ |

The keystone proves the whole ADR 0019 thesis on real hardware: a dynamic
construct the safe-subset engine *structurally refuses* (`$(...)`) **runs**
under the host-shell engine, yet an out-of-scope write from inside that same
full shell is **kernel-denied**, with `sandbox_kind == "seatbelt"`.

clippy `-D warnings` was clean on Homebrew rustc 1.95, and the `host-shell`
feature compiled in isolation (`--no-default-features --features host-shell`).

## 4. Phase A addendum — the PR #195 net-proxy flake

**Not reproduced** on this Mac: 40 additional parallel runs of the tool-shell
lib suite (default thread count = CI conditions, no `--test-threads=1`) — **0/40
failures** (41/41 including the Phase A run).

**But the root cause is inspection-confirmed.** Of the 17 `net_proxy` tests,
every one that binds a loopback listener and does an HTTP exchange serializes on
the module's `net_test_lock()` — **except**
`fenced_child_reaches_allowed_via_proxy_denied_refused_direct_kernel_blocked`,
which is the single heaviest network test (spawns a loopback origin, a proxy,
and a real `curl` child) yet is the only such test that **omits the lock**. It
can therefore run concurrently with sibling proxy tests that assume serialized
loopback access — exactly the shape of the observed "Empty reply from server" on
a contended CI runner (this M4 is too fast/uncontended to lose the race).

**Suggested fix (one line, matches every sibling):** add
`let _serial = net_test_lock();` as the test's first statement. Filed as
**agent-bridle #207** (still applicable on `main` as of 2026-07-06); a one-line
fix PR is a follow-up.

## 5. Phase B — newt integration test (PLAN, post-approval)

Prove Seatbelt engages **through newt's own dispatch path**, deterministically,
with no model in the loop.

- **Local, uncommitted wiring:** repoint the root `[patch.crates-io]` bridle
  crates from the `195e1c7e` git rev to `path = …/agent-bridle/<crate>`; add
  `macos-seatbelt` to newt-core's bridle feature list. (Drift is minimal — the
  current pin is only 4 commits behind bridle `main`, and #196 — the one net
  change newt already consumes — is *in* the pin.)
- **Feature gating (the subtle part):** newt-core has no `macos-seatbelt`
  feature of its own, so a naïve `#[cfg(feature = "macos-seatbelt")]` inside
  newt-core is always-false and the test would silently never run. Add a
  passthrough feature to `newt-core/Cargo.toml` — `seatbelt =
  ["agent-bridle/macos-seatbelt"]` — and gate the test
  `#[cfg(all(target_os = "macos", feature = "seatbelt"))]`.
- **The test** (`newt-core/tests/seatbelt_e2e.rs`) mirrors newt's real call site
  (`agentic/tools/shell.rs`, the `agent_bridle::registry().dispatch("shell", …,
  &caveats)` path — **not** the `--yolo` `host_shell_dispatch` raw-bash bypass,
  which is unconfined and unrelated to agent-bridle's `HostShellTool`): with
  `fs_write` fenced to a temp workspace, assert an in-fence write succeeds, an
  out-of-fence write is kernel-denied (file absent), and the envelope reports
  `sandbox_kind == "seatbelt"`.

## 6. Phase C — live headless run (PLAN, post-approval)

A model actually driving the fence, observed from outside.

- **Driver:** `newt plan --goal "…" --one-shot` (or `newt worker` over ACP, the
  newt-eval runner pattern), with a restricted permission posture — **not**
  `--yolo` / `--full-access`, which bypass the bridle.
- **Backend:** the configured gpu-runner loadout if reachable (watch the WireGuard
  on-demand WiFi gotcha), else the configured OpenAI backend.
- **Scenario:** the agent (a) creates a file inside the workspace — succeeds
  through the Seatbelt-wrapped subset engine; (b) tries to write outside the
  fence (e.g. `~/Documents/probe.txt`) — returns a **structured denial**, file
  never created, envelope `sandbox_kind: "seatbelt"`.
- **Capture:** transcript + envelopes into `docs/testing/results/`.

## 7. Follow-up ledger

| Item | Where | State |
|---|---|---|
| `[shell] engine` config surface + `--shell-engine` override + `--full-access`→host + `newt doctor` engine readout | newt #951 | ✅ **shipped** (#951) |
| newt Seatbelt/host-shell enablement (the earlier standalone `macos-seatbelt` pin-bump plan) | newt | **subsumed** by the engine-selection line (#951) + the agent-bridle **0.7.0-rc.1** brush-ocap release (#205); no standalone PR needed |
| net-proxy `net_test_lock()` one-liner | agent-bridle | filed as **#207**; one-line fix follow-up |
| Propose a `just uat-seatbelt` recipe so this is one command | newt | optional |

## 8. Update — 2026-07-06 (0.7.0-rc.1 gate)

Phase A's Seatbelt line was re-verified on the owner MacBook as part of the
agent-bridle **0.7.0-rc.1** pre-publication gate (**agent-bridle #205**): the
host-shell Seatbelt keystone, the brush engine's in-process confinement, Homebrew
PATH parity, carried coreutils under a scrubbed env (**#206**), and a live
**dgx1-inference** newt run (`--shell-engine host` running `$(...)` end-to-end)
all pass. The newt engine surface (`--shell-engine`, `newt doctor` → "Seatbelt —
available") landed via #951. Phases B/C here are superseded by that surface; this
doc stands as the record of the first owner-Mac Seatbelt run.
