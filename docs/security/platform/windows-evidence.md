# Windows OCAP evidence — AppContainer (AGENT WORKSHEET)

**Owner:** Windows agent (runs on real Windows CI). **Coordinator:** the Linux
maintainer of the cross-platform OCAP closure. **Board card:**
`knowledge/board/2026-08-08_ocap-windows-appcontainer-evidence-AGENT-BRIEF.md`.

> Fill every cell from **real-resource evidence** on Windows CI. `UNVERIFIED`
> until a test proves it. Distinguish DENIED-BY-OS-POLICY from
> command-happened-not-to-work. Do not promote an unexecuted test to
> DENIED/CLOSED. Where AppContainer cannot enforce an axis, mark it
> `UNSUPPORTED_FAIL_CLOSED` (refuses) or a named residual.

## 1. Route matrix (trace the ACTUAL Windows launcher — not the aclaunch binary's mere existence)

| route | launcher (AppContainer / LPAC / restricted token / refusal) | fs boundary | net boundary | env policy | failure behavior (missing backend) |
|---|---|---|---|---|---|
| `run_command` | _TBD_ | UNVERIFIED | UNVERIFIED | UNVERIFIED | **UNVERIFIED** (Linux truth: advisory-fallback — `unconfined-fallback-on-missing-backend`) |
| `run_build_check` | _TBD (ConstrainedExecutor)_ | UNVERIFIED | UNVERIFIED | UNVERIFIED | fail-closed? (Kernel floor — verify) |
| crew execution | _TBD_ | UNVERIFIED | UNVERIFIED | UNVERIFIED | UNVERIFIED |
| MCP/ACP worker | _TBD_ | UNVERIFIED | UNVERIFIED | UNVERIFIED | UNVERIFIED |
| git/helper subprocess | _TBD_ | UNVERIFIED | UNVERIFIED | UNVERIFIED | UNVERIFIED |
| PowerShell/cmd shell path | _TBD_ | UNVERIFIED | UNVERIFIED | UNVERIFIED | UNVERIFIED |

Every attacker-exec cell must terminate in a **proven AppContainer launcher** or
an **explicit refusal** — no silent third state.

## 2. Adversarial matrix (fill the Windows column from the test results)

| attack | Windows result |
|---|---|
| outside-workspace / profile secret read | UNVERIFIED |
| outside-workspace write | UNVERIFIED |
| sibling-dir write | UNVERIFIED |
| reparse/junction/UNC/alt-spelling escape | UNVERIFIED |
| provider credential inheritance | UNVERIFIED |
| direct TCP | UNVERIFIED |
| direct UDP | UNVERIFIED |
| loopback | UNVERIFIED |
| named-pipe / local IPC deputy | UNVERIFIED |
| inheritable HANDLE inheritance | UNVERIFIED |
| child/grandchild token escape | UNVERIFIED |
| shell/helper follows process tree | UNVERIFIED |
| timeout/cancellation cleanup (distinct) | UNVERIFIED |
| missing-backend fallback | UNVERIFIED |
| repo-controlled sandbox downgrade | UNVERIFIED |
| model-controlled sandbox downgrade | UNVERIFIED |

Each cell: `DENIED + evidence` / `BOUNDED + named invariant` / `GATED + machine
proof` / `ACTIVE + tracking issue` / `UNSUPPORTED_FAIL_CLOSED + refusal test` /
`OPERATOR-OPT-OUT`. No "N/A", "probably", "expected".

## 3. Feed back into the register
Make each residual **platform-scoped** in `docs/security/ocap-deviations.md`
(e.g. `local-deputy-egress: windows: <state>`). Do NOT let a Windows result
cosmetically close the Linux AF_UNIX residual, and keep timeout-cleanup distinct
from authority containment.

## 4. The Windows theorem (write it last, from evidence)
> _TBD by the agent — one precise sentence naming the achieved property and the
> AppContainer/token witness, or the fail-closed refusal where an axis is
> unsupported._
