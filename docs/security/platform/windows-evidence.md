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

- `agent-bridle-aclaunch.exe` from the [v0.8.0-rc.7 release archive](https://github.com/Gilamonster-Foundation/agent-bridle/releases/download/v0.8.0-rc.7/agent-bridle-mcp-v0.8.0-rc.7-windows-x86_64.tar.gz),
  SHA256 `6ca23258e1bf8c3eb7881d0836c8186039c176e6f472cfe9d948acf313c4816d`.
  `scripts/windows/Install-AppContainerLauncher.ps1` verifies it before extracting
  `agent-bridle-mcp-v0.8.0-rc.7-windows-x86_64/agent-bridle-aclaunch.exe`.
- `ab-netprobe.exe`, absent from that archive: install only this evidence helper
  with `cargo install agent-bridle-aclaunch --bin ab-netprobe --version 0.7.10 --locked`.

Current native Git tests additionally require the explicit elevated
[NUL and bounded DOS-path host preparation](../../guide/windows-prerequisites.md)
once per boot. CI runs those existing bounded DACL updates before ordinary tests; it is not performed by
the sandbox launcher or by Newt at runtime. The historical evidence below
predates this prerequisite and is not itself proof of native Git effects.
The native Git fixtures represent user-owned repositories: elevated CI gives
their repository and `.git` directories to the invoking user SID without
changing either DACL. Administrators-owned repositories remain subject to Git's
dubious-ownership refusal inside AppContainer; production applies no bypass. Newt's existing Git environment ignores global
and system Git configuration (`GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` point
to `/dev/null`), so a global `safe.directory` entry does not resolve that refusal,
even if its file is readable. Explicit config-file access still needs the
corresponding filesystem read grant.

The current native Git success evidence uses user-owned fixtures on the system
drive. Separate scratch-drive controls reproduce Git's cwd permission denial and
`GetFinalPathNameByHandleW(VOLUME_NAME_DOS)` error 5 despite successful NT-name
lookup and granted-file reads. Neither preceding DOS probes nor a directory
listing repaired the failure; a fresh-run comparison also diverged before the
differing launch sequence. The underlying host-dependent cause remains open;
[diagnostic controls](https://github.com/Gilamonster-Foundation/newt-agent/actions/runs/43664803228)
do not establish general second-drive support. A user-owned second-drive fixture
through Newt is unmeasured because its owner change failed the unchanged-DACL
guard. Newt-confined workspace `dir` is also unmeasured; bare-launcher listing
controls were denied on both drives.

CI runs the same evidence commands on `windows-latest` with
`BRIDLE_REQUIRE_APPCONTAINER=1` and `BRIDLE_REQUIRE_UNC_CONTROL=1`, so a missing
launcher/probe or missing UNC positive control is a hard failure rather than a
silent skip. GitHub-hosted runners have a different temp-directory DACL shape
than the local Windows host, so route-level positive write controls explicitly
grant only their test workspace to AppContainer package SIDs. Filesystem denial
fixtures keep their sibling/outside targets ungranted, and the timeout cleanup
fixture uses `cmd.exe` stdout plus a CPU-only timed command rather than
filesystem writes or PowerShell filesystem providers, so lifecycle remains
separate from authority.

### Vacuity guards — attempt first, then denial

Most denials here are proved by an *absent effect*: no file written, no
datagram delivered, no credential in stdout. That evidence is worthless unless
the child actually reached the forbidden operation — a failed AppContainer
launch, a PowerShell that never started, a crashed shell, a harness timeout, a
missing artifact, or a stale binary all produce the same absent effect as a
kernel denial. Each hostile fixture therefore proves the attempt happened
*before* asserting the denial:

- `cmd.exe` / PowerShell fixtures emit `DENIAL-ATTEMPT-BEGIN` on stdout
  immediately before the forbidden call (the UDP fixture uses its
  `POWERSHELL-RAN` marker file for the same purpose).
- `ab-netprobe` fixtures require the probe's own
  `ab-netprobe: ... failed: ...` line, which also proves the staged binary that
  ran really is `ab-netprobe` rather than a stale or silent executable.
- Must-succeed control runs assert `success` plus their expected stdout, so a
  child killed at its budget cannot stand in for a denial.
- The inheritable-HANDLE fixture *classifies* a residual rather than asserting a
  denial, so its guard gates the classification instead: without the marker it
  reports `PROBE_DID_NOT_RUN` and draws no conclusion. This immediately caught a
  live gap — on GitHub-hosted `windows-latest` the PowerShell child of the
  `ConstrainedExecutor` route exits 1 with empty stdout and stderr (the
  `cmd.exe` control in the same fixture runs fine), so every CI run before this
  guard recorded `CLOSED_ON_THIS_RUNNER` without evidence. The platform row
  stays ACTIVE regardless. Set `BRIDLE_REQUIRE_HANDLE_PROBE=1` on a host where
  the probe is expected to work to make the gap fatal.
- The launcher/probe requirements above (`BRIDLE_REQUIRE_*`) turn a missing
  artifact into a hard failure rather than a skip.

Correspondingly, the suite's timing policy separates the two directions.
Must-arrive waits (positive controls, the pipe payload, the deputy relay) get
generous budgets whose clocks start only once the awaited event is possible,
and the loopback listeners' accept loop is bounded by test lifetime rather than
by a wall clock armed before the child is launched. Must-NOT-arrive windows are
kept short and are never widened to make a run green — widening them would trade
proof for wall clock.

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
| `run_command` | `dispatch_bridled_shell` -> agent-bridle `ShellTool` -> `agent-bridle-aclaunch.exe` -> AppContainer (`run_command_windows_*`) | CLOSED: granted workspace write succeeds and sibling write is denied (`run_command_windows_appcontainer_allows_granted_write_denies_sibling_write`) | CLOSED for direct TCP under `net: none` (`run_command_windows_appcontainer_denies_direct_tcp`) | CLOSED (agent-bridle 0.8.0-rc.4): `run_command_windows_provider_env_is_not_inherited` proves a parent-only `OPENAI_API_KEY` does not reach the AppContainer child (agent-bridle#323). | CLOSED for the `windows-appcontainer` feature path: hidden launcher refuses before command runs (`run_command_windows_missing_launcher_refuses_not_host_fallback`) |
| `ConstrainedExecutor` seam (`run_build_check`, roadmap verify, crew normalize/test) | `ConstrainedExecutor` -> `agent_bridle::ConfinedCommand` -> `agent-bridle-aclaunch.exe` -> AppContainer (`constrained_run` helper) | CLOSED at the launcher boundary for workspace/sibling/outside/reparse paths; route asserts AppContainer on env and timeout probes | CLOSED at the launcher boundary for direct TCP/UDP and default loopback denial | CLOSED: no parent provider key inheritance unless explicitly granted (`appcontainer_child_does_not_inherit_provider_credentials`) | CLOSED for the `windows-appcontainer` feature path: hidden launcher yields `ConfinementUnenforceable`/authorization refusal (`appcontainer_missing_backend_refuses_not_host`) |
| crew execution | Routed through the `ConstrainedExecutor` seam for attacker-influenced normalize/test commands; non-attacker git/gh helpers remain classified separately in `docs/security/spawn-inventory.toml` | Same executor boundary as above | Same executor boundary as above | Same executor boundary as above | Same executor refusal as above |
| MCP stdio worker | NOT confined on Windows: `ConfinedCommand::spawn_tokio` is Unix-only, so `newt-mcp-client` spawns a raw `tokio::process::Command` and records `SandboxKind::None` (advisory); a workspace-protected session refuses this transport | OPEN: no AppContainer boundary on this route | OPEN: no kernel net boundary on this route | `env_clear()` plus the explicit stdio-MCP env allow-list | OPEN: no AppContainer launcher path for stdio MCP |
| ACP worker | ACP/headless uses the same `run_command` and `ConstrainedExecutor` seams for attacker-exec | Covered where it dispatches `run_command` or `ConstrainedExecutor`; no distinct worker process proof here | Covered where it dispatches `run_command` or `ConstrainedExecutor`; no distinct worker process proof here | Covered at the invoked seam; inherits the `run_command` ACTIVE env residual when it calls that route | Not separately exercised in this PR |
| git/helper subprocess | Native Git uses the ordinary AppContainer exec route with the pinned rc.7 launcher; CI explicitly runs status, add, exec-grant retry and refreshed-workspace-grant tests | Required effects: actual repository output and staged file, no stale index lock, later host Git success; ungranted exec and filesystem declarations remain denied. Installed Git DACLs remain host-specific. | Not a network proof | Inherits the invoked seam's env policy | Ordinary confined dispatch refuses a missing backend; no Git-specific host fallback |
| PowerShell/cmd shell path | AppContainer wraps `cmd.exe`, `powershell.exe`, and grandchildren (`appcontainer_descendants_stay_in_the_same_token`, `appcontainer_follows_shells_and_helpers`) | CLOSED by direct fs tests and route fs proof | CLOSED for direct TCP/UDP/default loopback denial | CLOSED on `ConstrainedExecutor`; ACTIVE on `run_command` until agent-bridle clears Windows ambient env | N/A outside the Newt route tests |

## Adversarial Matrix

| attack | Windows result |
|---|---|
| outside-workspace / profile secret read | DENIED: `appcontainer_denies_profile_secret_read` wrote a host-readable `%USERPROFILE%` secret and AppContainer `cmd.exe /c type` could not read it. |
| outside-workspace write | DENIED: `appcontainer_denies_outside_workspace_write` proved the same write command works when the directory is granted, then fails against an ungranted directory. |
| sibling-dir write | DENIED: `appcontainer_denies_sibling_dir_write` and the run_command sibling test left the sibling sentinel unchanged. |
| reparse/junction/UNC/alt-spelling escape | DENIED: `appcontainer_denies_reparse_and_unc_escape` covered `..`, `\\?\` extended spelling, a junction out of the workspace, and a `\\localhost\C$` UNC admin-share positive control before the AppContainer denial attempt. |
| provider credential inheritance | DENIED on `ConstrainedExecutor` (`appcontainer_child_does_not_inherit_provider_credentials` proves only explicitly granted env crosses) and on `run_command` (`run_command_windows_provider_env_is_not_inherited`, agent-bridle 0.8.0-rc.4). |
| direct TCP | DENIED: direct launcher and run_command netprobe controls prove host success and AppContainer `net:none` denial. |
| direct UDP | DENIED: host PowerShell sends a UDP datagram; AppContainer PowerShell runs and writes a marker, but the datagram is not delivered. |
| loopback | DENIED by default: loopback TCP is blocked without the exemption. Positive exemption proof is `UNSUPPORTED_FAIL_CLOSED` on this non-elevated token; set `BRIDLE_REQUIRE_ELEVATED=1` on an elevated runner to force that positive control. |
| named-pipe / local IPC deputy | ACTIVE residual: `appcontainer_named_pipe_deputy` proves an AppContainer child can connect to an `ALL APPLICATION PACKAGES` named pipe and cause a host deputy to relay over loopback. |
| inheritable HANDLE inheritance | ACTIVE residual on this Windows host: `appcontainer_inheritable_handle_inheritance` deliberately creates an inheritable parent file handle and records whether the AppContainer child can use it. The local host produced `HANDLE-LEAK`; GitHub-hosted runner behavior may close this specific raw handle, but the platform row remains ACTIVE until the shared Windows spawn path proves arbitrary inheritable handles are blocked on every supported launcher chain. |
| child/grandchild token escape | DENIED: `appcontainer_descendants_stay_in_the_same_token` shows cmd and PowerShell descendants report low/restricted token evidence at two generations. |
| shell/helper follows process tree | DENIED for tested helpers: cmd, PowerShell, and staged workspace `.exe` stay under AppContainer. Native Git status/staging is required by the CI effect tests above; desktop Git ACLs remain host-specific. |
| timeout/cancellation cleanup | BOUNDED cleanup, not authority: `appcontainer_timeout_cleanup_is_distinct_from_authority` proves a fast `cmd.exe` AppContainer child captures stdout and a CPU-only timed child returns promptly on the deadline. This is implemented with a Windows Job Object plus immediate-child kill fallback. |
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
