# macOS OCAP evidence — Seatbelt

**Owner:** macOS agent (real macOS). **Coordinator:** the Linux maintainer of the
cross-platform OCAP closure (newt-agent#1631). **Board card:**
`knowledge/board/2026-08-08_ocap-macos-seatbelt-evidence-AGENT-BRIEF.md`.

**Evidence host:** macOS 26.5.2 (build 25F84), Apple Silicon.
**Witness suite:** `newt-core/tests/macos_seatbelt_adversarial.rs` — real
`sandbox-exec` + real subprocess, driving `ConstrainedExecutor::run`
(`ExecOrigin::AgentInfluenced`, `Kernel` strength floor: the route
`run_build_check` / crew / MCP-ACP workers share). **16/16 green**, run as:

```text
cargo test -p newt-core --features macos-seatbelt \
  --test macos_seatbelt_adversarial -- --ignored --test-threads=1
```

Backend: agent-bridle 0.7.15 `SeatbeltSandbox` (feature `os-sandbox` →
`macos-seatbelt`, compiled in on macOS). Confinement is a `sandbox-exec -p
<SBPL profile>` wrapper the child inherits; every enforcement test pins
`sandbox_kind == SandboxKind::Seatbelt` so a DENY can never be confused with
command-not-found or a silent advisory downgrade.

## 1. Route matrix (traced to the ACTUAL macOS launcher)

| route | launcher (concrete spawn) | fs boundary | net boundary | env policy | failure behavior (missing backend) |
|---|---|---|---|---|---|
| `run_command` | `dispatch_bridled_shell` → `agent_bridle::{ShellTool,HostShellTool,BrushShellTool}` with `b1_run_command_sandbox_policy()` (`ChildNetworkPolicy::DenyDirect`) → **Seatbelt** wrapper | **Seatbelt** SBPL `(deny file-write*/read*)` + re-allow workspace (caveat-driven) | **Seatbelt** `(deny network*)` under `net:none`; the b1 `DenyDirect` seccomp add-on is Linux-only (inert on macOS — Seatbelt already kernel-denies egress) | bridle shell tool starts env-scrubbed | **Advisory** strength floor (default `Gate`); but `command_prefix` still **fails closed** (`Denied`) for a governed axis if `sandbox-exec` is absent. `sandbox-exec` is a SIP binary, always present on macOS. *(Traced from source + shares the proven Seatbelt backend; an independent `run_command` lib test is the tracked follow-up.)* |
| `run_build_check` | `ConstrainedExecutor::run`, `ExecOrigin::AgentInfluenced`, **`NetGrant::DenyAll`** | Seatbelt (proven, §2) | **REFUSES on macOS** — `NetGrant::DenyAll` needs the Linux seccomp `newt-net-guard`; `resolve_net_floor` returns `ConfinementUnenforceable` | env-empty + explicit grants (`HOME`/`TMPDIR`/`PATH`) | **fail-closed UNAVAILABLE** (proven: `seatbelt_net_deny_all_grant_refuses_fail_closed`) |
| crew execution | `ConstrainedExecutor::run`, `AgentInfluenced` (shares §2 mechanism) | Seatbelt (proven, §2) | Seatbelt `(deny network*)` via `net:none` (proven); `NetGrant::DenyAll` callers refuse (above) | env-empty + grants | fail-closed (`ConfinementUnenforceable`) |
| MCP/ACP worker | `ConstrainedExecutor` / bridle spawn, `AgentInfluenced` | Seatbelt (proven, §2) | Seatbelt `(deny network*)` via `net:none` | env-empty + grants | fail-closed |
| git/helper subprocess | `hardened_git` argv → confined spawn (Seatbelt backend) | Seatbelt (proven: `seatbelt_follows_interpreters_and_helpers`) | Seatbelt via caveat | env-empty + grants | fail-closed |
| host-shell engine | `agent_bridle::HostShellTool` + b1 policy → Seatbelt wrapper | Seatbelt SBPL (caveat-driven) | Seatbelt `(deny network*)`; DenyDirect inert on macOS | env-scrubbed | Advisory floor; `command_prefix` fail-closed on absent `sandbox-exec` |

Every attacker-exec cell terminates in a **proven Seatbelt launcher** or an
**explicit refusal** — no silent third state. The one functional gap is
`NetGrant::DenyAll`, which fail-closed refuses on macOS (see §3, follow-up).

## 2. Adversarial matrix (real results)

| attack | macOS result | witness (`macos_seatbelt_adversarial::…`) |
|---|---|---|
| outside-workspace read | **DENIED + evidence** (content denied; metadata ambient) | `seatbelt_denies_outside_workspace_read` |
| outside-workspace write | **DENIED + evidence** | `seatbelt_denies_outside_workspace_write` |
| sibling-repo write | **DENIED + evidence** | `seatbelt_denies_sibling_repo_write` |
| symlink / canonicalization escape | **DENIED + evidence** (kernel matches the realpath) | `seatbelt_denies_symlink_escape` |
| parent credential inheritance | **DENIED + evidence** (child env = grants only) | `seatbelt_child_does_not_inherit_parent_credentials` |
| direct TCP | **DENIED + evidence** (`(deny network*)`) | `seatbelt_denies_direct_tcp` |
| direct UDP | **DENIED + evidence** | `seatbelt_denies_direct_udp` |
| loopback | **DENIED + evidence** (`net:none` is strictly deny-all; no loopback re-allow) | `seatbelt_loopback_behavior` |
| local IPC deputy (pathname AF_UNIX) | **DENIED + evidence** — *stronger than Linux:* Seatbelt `(deny network*)` governs AF_UNIX `connect` itself, not merely the fs fence | `seatbelt_pathname_af_unix_deputy` |
| Mach/XPC ambient deputy | **ACTIVE + tracking** (`mach-xpc-ambient-deputy`, macOS) — SBPL is `(allow default)`; mach-lookup is ambient | `seatbelt_mach_xpc_deputy_surface` |
| non-CLOEXEC descriptor inheritance | **DENIED + evidence** (child sees only stdio; no parent-fd leak) | `seatbelt_non_cloexec_fd_inheritance` |
| child/grandchild escape | **DENIED + evidence** (boundary inherited ≥2 generations) | `seatbelt_descendants_stay_confined` |
| interpreter/helper follows process tree | **DENIED + evidence** (python3 net, git fs) | `seatbelt_follows_interpreters_and_helpers` |
| missing-backend fallback | **UNSUPPORTED_FAIL_CLOSED + refusal proof** — governed-axis `command_prefix` never returns an empty (silently-unconfined) prefix; `sandbox-exec` is SIP-guaranteed so the absent branch is unreachable on a real host (covered by agent-bridle's own unit tests) | `seatbelt_missing_backend_refuses_not_host` |
| `NetGrant::DenyAll` kernel net floor | **UNSUPPORTED_FAIL_CLOSED + refusal proof** — Linux-seccomp-only; refuses on macOS (net denial is instead available via `net:none` → Seatbelt) | `seatbelt_net_deny_all_grant_refuses_fail_closed` |
| repo-controlled sandbox downgrade | **BOUNDED** — the SBPL profile is generated from the effective `Caveats`, pinned by `seatbelt_generated_profile_pins_the_boundary`; a repo cannot widen it without failing the pin | `seatbelt_generated_profile_pins_the_boundary` |
| model-controlled sandbox downgrade | **BOUNDED** — same pin; the model influences only the confined argv, never the profile | `seatbelt_generated_profile_pins_the_boundary` |

**Generated profile (pinned clauses)** for the workspace fence
(`fs_read`/`fs_write` = workspace, `net:none`):

```scheme
(version 1)
(allow default)
(deny file-write*)
(allow file-write* (subpath "<workspace realpath>"))
(allow file-write* (literal "/dev/null") …device sinks…)
(deny file-read*)
(allow file-read-metadata)
(allow file-read* (literal "/") (subpath "/usr") (subpath "/System") … (subpath "<workspace realpath>"))
(deny network*)
```

The pin asserts `(deny file-write*)`, `(deny file-read*)`, `(deny network*)`, the
canonicalized workspace write-root re-allow, and the **absence** of any
`(allow network*)` re-allow under `net:none`.

## 3. Feed back into the register
`docs/security/ocap-deviations.md` updated with platform-scoped states:

- `local-deputy-egress`: entry stays **OPEN** (Linux AF_UNIX residual is
  unfixed — this macOS result does NOT close it). Platform note added:
  `macos: DENIED` (Seatbelt `(deny network*)` governs AF_UNIX connect; witness
  above).
- `mach-xpc-ambient-deputy` (**NEW**, macOS-scoped, OPEN/ACTIVE): the SBPL
  `(allow default)` base leaves Mach/XPC service lookup ambient — a confined
  child can reach host XPC services that could act as deputies.
- `b1-os-isolation`: note that on macOS the direct-egress floor is provided by
  **Seatbelt `(deny network*)`**, not the Linux seccomp `newt-net-guard`; the
  `NetGrant::DenyAll` executor floor is fail-closed-unavailable on macOS (a
  per-axis strength floor that accepts the Seatbelt net witness is the
  coordinated follow-up with the Linux maintainer — see §1 `run_build_check`).

## 4. The macOS theorem
> On macOS (Seatbelt), every supported Newt attacker-exec route that reaches the
> `ConstrainedExecutor` / bridle-shell launcher runs under a real, kernel-enforced
> `sandbox-exec` profile whose filesystem (out-of-fence read/write, sibling,
> symlink-realpath), network (direct TCP/UDP, loopback, **and the pathname
> AF_UNIX local deputy**), credential-inheritance, descriptor-hygiene, and
> process-tree-descendant boundaries are all kernel-DENIED with adversarial
> evidence — with two axes honestly reported UNSUPPORTED and **fail-closed**
> (the `NetGrant::DenyAll` seccomp floor, and any absent `sandbox-exec` backend)
> and one named residual still ACTIVE (`mach-xpc-ambient-deputy`), so no route
> ever runs hostile code unconfined.
