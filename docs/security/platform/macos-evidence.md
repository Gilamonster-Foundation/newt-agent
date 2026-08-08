# macOS OCAP evidence — Seatbelt (AGENT WORKSHEET)

**Owner:** macOS agent (runs on real macOS CI). **Coordinator:** the Linux
maintainer of the cross-platform OCAP closure. **Board card:**
`knowledge/board/2026-08-08_ocap-macos-seatbelt-evidence-AGENT-BRIEF.md`.

> Fill every cell from **real-resource evidence** on macOS CI. `UNVERIFIED` until a
> test proves it. Compiling ≠ evidence. Do not promote an unexecuted test to
> DENIED/CLOSED. Where Seatbelt cannot enforce an axis, mark it
> `UNSUPPORTED_FAIL_CLOSED` (refuses) or a named residual — never fake Landlock
> equivalence.

## 1. Route matrix (trace the ACTUAL macOS launcher — do not infer from feature flags)

| route | launcher (concrete spawn fn) | fs boundary | net boundary | env policy | failure behavior (missing backend) |
|---|---|---|---|---|---|
| `run_command` | _TBD (agent-bridle ShellTool/HostShell/Brush → ?)_ | UNVERIFIED | UNVERIFIED | UNVERIFIED | **UNVERIFIED** (Linux truth: advisory-fallback — see `unconfined-fallback-on-missing-backend`) |
| `run_build_check` | _TBD (ConstrainedExecutor → ConfinedCommand)_ | UNVERIFIED | UNVERIFIED | UNVERIFIED | fail-closed? (Kernel floor — verify on macOS) |
| crew execution | _TBD_ | UNVERIFIED | UNVERIFIED | UNVERIFIED | UNVERIFIED |
| MCP/ACP worker | _TBD_ | UNVERIFIED | UNVERIFIED | UNVERIFIED | UNVERIFIED |
| git/helper subprocess | _TBD (hardened_git)_ | UNVERIFIED | UNVERIFIED | UNVERIFIED | UNVERIFIED |
| host-shell engine | _TBD_ | UNVERIFIED | UNVERIFIED | UNVERIFIED | UNVERIFIED |

Every attacker-exec cell must terminate in a **proven Seatbelt launcher** or an
**explicit refusal** — no silent third state.

## 2. Adversarial matrix (fill the macOS column from the test results)

| attack | macOS result |
|---|---|
| outside-workspace read | UNVERIFIED |
| outside-workspace write | UNVERIFIED |
| sibling-repo write | UNVERIFIED |
| symlink / canonicalization escape | UNVERIFIED |
| parent credential inheritance | UNVERIFIED |
| direct TCP | UNVERIFIED |
| direct UDP | UNVERIFIED |
| loopback | UNVERIFIED |
| local IPC deputy (pathname AF_UNIX) | UNVERIFIED |
| Mach/XPC ambient deputy | UNVERIFIED |
| non-CLOEXEC descriptor inheritance | UNVERIFIED |
| child/grandchild escape | UNVERIFIED |
| interpreter/helper follows process tree | UNVERIFIED |
| missing-backend fallback | UNVERIFIED |
| repo-controlled sandbox downgrade | UNVERIFIED |
| model-controlled sandbox downgrade | UNVERIFIED |

Each cell must be exactly one of: `DENIED + evidence` / `BOUNDED + named
invariant` / `GATED + machine proof` / `ACTIVE + tracking issue` /
`UNSUPPORTED_FAIL_CLOSED + refusal test` / `OPERATOR-OPT-OUT`. No "N/A",
"probably", "expected".

## 3. Feed back into the register
When the evidence exists, update `docs/security/ocap-deviations.md` to make each
residual **platform-scoped** (e.g. `local-deputy-egress: macos: <state>`). Do NOT
let a macOS result cosmetically close the Linux AF_UNIX residual, and do NOT let
Linux evidence conceal an unsupported macOS path.

## 4. The macOS theorem (write it last, from evidence)
> _TBD by the agent — one precise sentence naming the achieved property and the
> Seatbelt witness, or the fail-closed refusal where an axis is unsupported._
