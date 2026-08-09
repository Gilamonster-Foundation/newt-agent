# Windows OCAP Evidence - AppContainer

**Owner:** Windows agent. **Coordinator:** cross-platform OCAP closure.
**Board card:** `knowledge/board/2026-08-08_ocap-windows-appcontainer-evidence-AGENT-BRIEF.md`.

This page records real-resource evidence gathered on Windows for PR #1633. It
does not promote an AppContainer binary's existence into a guarantee: every
closed cell below is grounded by a test that either ran hostile code inside a
real AppContainer or proved the route refused before hostile code ran.

## Verification

Local Windows host:

- OS: Windows 10 Pro, version 2009, hardware abstraction layer `10.0.26100.1`
- Architecture: 64-bit / `x86_64-pc-windows-msvc`
- Rust: `rustc 1.93.1`

```powershell
cargo test -p newt-core --features windows-appcontainer --lib run_command_windows_ -- --test-threads=1 --nocapture
cargo test -p newt-core --features windows-appcontainer --test windows_appcontainer_adversarial -- --ignored --test-threads=1 --nocapture
```

Result on 2026-08-08: `run_command_windows_` passed 4/4. The ignored
AppContainer suite passed 14/14. The positive loopback-exemption control was
classified as `UNSUPPORTED_FAIL_CLOSED` on the local non-elevated token; default
loopback denial was still proved.

Required local tools:

- `agent-bridle-aclaunch.exe` from `cargo install agent-bridle-aclaunch --version 0.7.10 --locked`
- `ab-netprobe.exe`, installed by the same package

CI runs the same evidence commands on `windows-latest` with
`BRIDLE_REQUIRE_APPCONTAINER=1` and `BRIDLE_REQUIRE_UNC_CONTROL=1`, so a missing
launcher/probe or missing UNC positive control is a hard failure rather than a
silent skip. GitHub-hosted runners have a different temp-directory DACL shape
than the local Windows host, so route-level positive write controls explicitly
grant only their test workspace to AppContainer package SIDs. Filesystem denial
fixtures keep their sibling/outside targets ungranted, and the timeout cleanup
fixture uses stdout rather than filesystem writes so lifecycle remains separate
from authority.

Manual token snapshot:

```powershell
agent-bridle-aclaunch.exe --name newt-manual-token-groups cmd.exe /c "whoami /groups & powershell.exe -NoProfile -NonInteractive -Command whoami /groups"
```

Both the direct `cmd.exe` child and PowerShell grandchild reported
`Mandatory Label\Low Mandatory Level` (`S-1-16-4096`) and showed
`BUILTIN\Administrators` / local-admin groups as `Group used for deny only`.
GitHub-hosted runner token output is not identical, but the native suite checks
the stable two-generation AppContainer signal: both child and grandchild report
`Low Mandatory Level`.

## Route Matrix

| route | launcher / refusal | fs boundary | net boundary | env policy | missing backend |
|---|---|---|---|---|---|
| `run_command` | `dispatch_bridled_shell` -> agent-bridle `ShellTool` -> `agent-bridle-aclaunch.exe` -> AppContainer (`run_command_windows_*`) | CLOSED: granted workspace write succeeds and sibling write is denied (`run_command_windows_appcontainer_allows_granted_write_denies_sibling_write`) | CLOSED for direct TCP under `net: none` (`run_command_windows_appcontainer_denies_direct_tcp`) | ACTIVE: shared agent-bridle `ShellTool` on Windows inherits ambient parent env; `run_command_windows_provider_env_inheritance_is_active` proves `OPENAI_API_KEY` reaches the AppContainer child. This must be fixed in agent-bridle#323, not by a Newt-local bypass. | CLOSED for the `windows-appcontainer` feature path: hidden launcher refuses before command runs (`run_command_windows_missing_launcher_refuses_not_host_fallback`) |
| `ConstrainedExecutor` seam (`run_build_check`, roadmap verify, crew normalize/test) | `ConstrainedExecutor` -> `agent_bridle::ConfinedCommand` -> `agent-bridle-aclaunch.exe` -> AppContainer (`constrained_run` helper) | CLOSED at the launcher boundary for workspace/sibling/outside/reparse paths; route asserts AppContainer on env and timeout probes | CLOSED at the launcher boundary for direct TCP/UDP and default loopback denial | CLOSED: no parent provider key inheritance unless explicitly granted (`appcontainer_child_does_not_inherit_provider_credentials`) | CLOSED for the `windows-appcontainer` feature path: hidden launcher yields `ConfinementUnenforceable`/authorization refusal (`appcontainer_missing_backend_refuses_not_host`) |
| crew execution | Routed through the `ConstrainedExecutor` seam for attacker-influenced normalize/test commands; non-attacker git/gh helpers remain classified separately in `docs/security/spawn-inventory.toml` | Same executor boundary as above | Same executor boundary as above | Same executor boundary as above | Same executor refusal as above |
| MCP stdio worker | `newt-mcp-client` uses `ConfinedCommand::spawn_tokio`; classified by the shared `ConfinedCommand` primitive, not by a separate stdio-MCP scenario in this PR | Route is classified confined; AppContainer launcher evidence above covers the shared primitive | Route is classified confined; direct net semantics depend on the minted MCP caveats | Existing stdio-MCP env allow-list remains the source of truth | Not separately exercised in this PR |
| ACP worker | ACP/headless uses the same `run_command` and `ConstrainedExecutor` seams for attacker-exec | Covered where it dispatches `run_command` or `ConstrainedExecutor`; no distinct worker process proof here | Covered where it dispatches `run_command` or `ConstrainedExecutor`; no distinct worker process proof here | Covered at the invoked seam; inherits the `run_command` ACTIVE env residual when it calls that route | Not separately exercised in this PR |
| git/helper subprocess | AppContainer follows a staged workspace `.exe`; a `git.exe --version` helper probe is evidence-only because installed Git DACLs vary by host (`appcontainer_follows_shells_and_helpers`) | CLOSED for staged workspace helper; Git helpers are also hardened by `newt-core/src/git_hardening.rs` | Not a network proof | Inherits the invoked seam's env policy | Not separately exercised for Git |
| PowerShell/cmd shell path | AppContainer wraps `cmd.exe`, `powershell.exe`, and grandchildren (`appcontainer_descendants_stay_in_the_same_token`, `appcontainer_follows_shells_and_helpers`) | CLOSED by direct fs tests and route fs proof | CLOSED for direct TCP/UDP/default loopback denial | CLOSED on `ConstrainedExecutor`; ACTIVE on `run_command` until agent-bridle clears Windows ambient env | N/A outside the Newt route tests |

## Adversarial Matrix

| attack | Windows result |
|---|---|
| outside-workspace / profile secret read | DENIED: `appcontainer_denies_profile_secret_read` wrote a host-readable `%USERPROFILE%` secret and AppContainer `cmd.exe /c type` could not read it. |
| outside-workspace write | DENIED: `appcontainer_denies_outside_workspace_write` proved the same write command works when the directory is granted, then fails against an ungranted directory. |
| sibling-dir write | DENIED: `appcontainer_denies_sibling_dir_write` and the run_command sibling test left the sibling sentinel unchanged. |
| reparse/junction/UNC/alt-spelling escape | DENIED: `appcontainer_denies_reparse_and_unc_escape` covered `..`, `\\?\` extended spelling, a junction out of the workspace, and a `\\localhost\C$` UNC admin-share positive control before the AppContainer denial attempt. |
| provider credential inheritance | MIXED: DENIED on `ConstrainedExecutor` (`appcontainer_child_does_not_inherit_provider_credentials` proves only explicitly granted env crosses). ACTIVE on `run_command`: `run_command_windows_provider_env_inheritance_is_active` proves shared agent-bridle `ShellTool` Windows children inherit parent `OPENAI_API_KEY`. |
| direct TCP | DENIED: direct launcher and run_command netprobe controls prove host success and AppContainer `net:none` denial. |
| direct UDP | DENIED: host PowerShell sends a UDP datagram; AppContainer PowerShell runs and writes a marker, but the datagram is not delivered. |
| loopback | DENIED by default: loopback TCP is blocked without the exemption. Positive exemption proof is `UNSUPPORTED_FAIL_CLOSED` on this non-elevated token; set `BRIDLE_REQUIRE_ELEVATED=1` on an elevated runner to force that positive control. |
| named-pipe / local IPC deputy | ACTIVE residual: `appcontainer_named_pipe_deputy` proves an AppContainer child can connect to an `ALL APPLICATION PACKAGES` named pipe and cause a host deputy to relay over loopback. |
| inheritable HANDLE inheritance | ACTIVE residual on this Windows host: `appcontainer_inheritable_handle_inheritance` deliberately creates an inheritable parent file handle and records whether the AppContainer child can use it. The local host produced `HANDLE-LEAK`; GitHub-hosted runner behavior may close this specific raw handle, but the platform row remains ACTIVE until the shared Windows spawn path proves arbitrary inheritable handles are blocked on every supported launcher chain. |
| child/grandchild token escape | DENIED: `appcontainer_descendants_stay_in_the_same_token` shows cmd and PowerShell descendants report low/restricted token evidence at two generations. |
| shell/helper follows process tree | DENIED for tested helpers: cmd, PowerShell, and staged workspace `.exe` stay under AppContainer. Git is evidence-only because installed Git ACLs vary. |
| timeout/cancellation cleanup | BOUNDED cleanup, not authority: `appcontainer_timeout_cleanup_is_distinct_from_authority` proves a timed-out PowerShell child returns promptly and does not write a late marker after the timeout. This is implemented with a Windows Job Object plus immediate-child kill fallback. |
| missing-backend fallback | CLOSED for the `windows-appcontainer` feature path: both run_command and `ConstrainedExecutor` refuse when `agent-bridle-aclaunch.exe` is hidden from PATH. |
| repo-controlled sandbox downgrade | CLOSED by existing config-plane stripping plus route-level missing-backend refusal; this PR adds no repo-controlled switch that can disable AppContainer. |
| model-controlled sandbox downgrade | CLOSED for tested routes: model/tool input cannot hide the launcher or request host fallback; explicit operator opt-out remains the separate `--disable-ocap` / `--full-access` lane. |

## Windows Theorem

On a Windows build compiled with `windows-appcontainer` and with
`agent-bridle-aclaunch.exe` available, Newt's tested attacker-exec routes either
run inside a real AppContainer token with ACL-scoped filesystem grants and
default-denied direct TCP/UDP/loopback egress, or they refuse before hostile code
runs. `ConstrainedExecutor` additionally strips parent provider credentials and
has bounded timeout cleanup. The shared `run_command`/agent-bridle `ShellTool`
path still inherits ambient Windows environment, so provider credential stripping
is ACTIVE there until agent-bridle grows Windows env-clear parity. The other
remaining Windows residuals are named-pipe local-deputy egress and host-sensitive
inheritable HANDLE inheritance.
