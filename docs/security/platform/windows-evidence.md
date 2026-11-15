# Windows OCAP Evidence - AppContainer

**Owner:** Windows agent. **Coordinator:** cross-platform OCAP closure.

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

## Native Git evidence

Installation and conditional host preparation are described in the
[Windows prerequisites](../../guide/windows-prerequisites.md). The pinned rc.7
launcher restores traverse and delete rights within its workspace write grant;
the old ancestry-access diagnosis was incorrect. Newt adds no Git-specific host
fallback or ownership bypass.

### Windows Server CI

[Windows CI 43659855982](https://github.com/Gilamonster-Foundation/newt-agent/actions/runs/43659855982/job/132646509759)
and [diagnostic 43659882248](https://github.com/Gilamonster-Foundation/newt-agent/actions/runs/43659882248)
passed real Git status, add, exec-grant retry and refreshed-workspace-grant
effects, no stale index lock, and subsequent host Git. CI runs the existing NUL
and bounded DOS-path preparation; elevated fixtures set repository/.git owner
to the invoking user SID and assert unchanged DACLs. A separate config probe
proves global `safe.directory` stays ignored even with a file read grant:
`GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` point to the null device. Explicit
`git config --file` is denied without its file grant and succeeds with it.

The config probe `run_command_windows_global_git_config_stays_ignored` is an
ignored real-resource ground. The Windows leg of `mcp-import-real.yml` selects
its exact full name with the nonempty wrapper, `--ignored` and `--test-threads=1`
on weekly/release runs; the manual diagnostic does the same alongside four
existing Git effects whose passing count is asserted. Ordinary per-PR CI
intentionally skips the ignored ground, including its `run_command_windows_`
evidence filter. The pure `sandbox_git_env_tests::ambient_config_is_ignored`
guard asserts both global and system config point at the null device.

At f8f1e620, three diagnostic runs
([43666516120](https://github.com/Gilamonster-Foundation/newt-agent/actions/runs/43666516120),
[43667425578](https://github.com/Gilamonster-Foundation/newt-agent/actions/runs/43667425578),
[43667427725](https://github.com/Gilamonster-Foundation/newt-agent/actions/runs/43667427725))
measured **3 failures in 9 jobs** on D: through the bare launcher. Each launch
order failed once; original-first also passed twice, refuting an order-specific
cause. System-drive required probes passed in all nine; the through-Newt tests
ran in three of them and passed.
Failing scratch probes resolved NT names and read granted files but returned
error 5 for DOS/GUID names; the underlying host-dependent cause is unresolved.
v0.8.6 ships with this second-drive limit: the single-job manual
diagnostic now retains scratch Git/DOS observations as report-only, while host
preparation, system-drive required effects and through-Newt tests remain gates.
Through-Newt second-drive Git is unmeasured because changing the scratch fixture
owner failed the unchanged-DACL guard. Bare-launcher `dir` was denied on both
CI drives; Newt-confined workspace `dir` is unmeasured.

### Windows 11 Pro desktop (build 26200)

The measurements used one NTFS system drive, Git for Windows
2.53.0.windows.1, the checksum-verified rc.7 launcher, no system/global
`safe.directory`, and **no NUL or DOS-path host preparation**. Each bare-launcher
probe granted only a fresh repository containing a committed tracked file and
an untracked `pending.txt`, with that repository as cwd.

| Probe | Interactive, non-elevated; user-owned repo | Interactive, elevated; Administrators-owned repo | ssh, session 0, elevated |
|---|---|---|---|
| `cmd /c echo hello` | exit 0 | exit 0 | exit 0 |
| `cmd /c "cd & type tracked.txt"` | exit 0; cwd and file printed | exit 0 | exit 0 |
| `cmd /c "dir /b /a"` | Access is denied; exit 1 | Access is denied; exit 1 | Access is denied; exit 1 |
| `whoami.exe` | exit 0 | exit 0 | `0xC0000142` |
| `git --version` | exit 0 | exit 0 | `0xC0000142` |
| `git status --short` | exit 0; `?? pending.txt` | dubious ownership; exit 128 | `0xC0000142` |
| `git add -- pending.txt` | exit 0 | dubious ownership; exit 128 | `0xC0000142` |
| Stale `.git/index.lock` afterward | none | none | none |
| Later host `git diff --cached --name-only` | `pending.txt` | empty | empty |

The non-elevated Git commands warned that the profile's `.config/git/ignore`
was unreadable but still succeeded. The table measures the bare launcher.

On the same desktop at f8f1e620, all five diagnostic tests also ran
through Newt in an interactive, non-elevated session, with the verified launcher
staged by `Install-AppContainerLauncher.ps1` and **neither host-preparation script
run**. Status, add, exec-grant retry, refreshed workspace grants and global-config
isolation passed: **5 passed, 0 failed, 0 ignored**, in 8.87 seconds. The tests
required AppContainer (`BRIDLE_REQUIRE_APPCONTAINER=1`) and ran serially with the
`windows-appcontainer` feature. Through-Newt tests elevated or over ssh, the
interactive TUI, a second desktop drive and a Windows Server desktop remain
unmeasured.

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
