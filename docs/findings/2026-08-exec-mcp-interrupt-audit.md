# newt-agent: improvements for `pa`/MCP use, hung commands, and Ctrl-C/Ctrl-D

> **Status: findings / audit note, 2026-08.** Relocated from `docs/design/`;
> this is a snapshot of an investigation, not a live design.
>
> **Partly landed:** Fix 1a is in — `host_exec_timeout()`, `kill_on_drop(true)`
> + own process group, and the `timed_out` / exit-code-124 envelope now live in
> `newt-core/src/agentic/tools.rs` (~966, ~1090-1103, ~1155). A first-class
> `git` tool exists (`newt-git` crate; §5a). `run_command` accepts an optional
> `cwd` (`resolve_exec_cwd`, #1159; §5b).
>
> **Still live** (nearest open issues: #1075, #784, #1148, #1720): Fix 1b —
> racing the turn cancel token against the running exec; Fix 2 — mid-turn
> Ctrl-D and an OS-level SIGINT/SIGTERM backstop; §3 MCP startup / per-call
> timeouts, degraded-server handling, child reaping and the `newt doctor` MCP
> probe; §5c confined-shell `$`/redirection/`/dev/null`; §5d round budget;
> §5e denial → `request_permissions` hints. Line references below are as of
> the original investigation and may have drifted.

Findings from an investigation into "ran a program, it got stuck, couldn't Ctrl-C / Ctrl-D."
File/line references are to the current tree.

---

## 1. Root cause of the hang: the host-bypass exec path has no timeout and no cancellation

There are **two** exec paths in `newt-core/src/agentic/tools.rs`:

- **Confined path** — `exec_confined_command` → `bridle_registry().dispatch("shell", …)`.
  Its envelope carries a `timed_out` field, so this path *does* bound runtime.
- **Host-bypass path** — taken when OCAP is disabled (`--disable-ocap` / `--yolo` /
  `NEWT_DISABLE_OCAP=1`) and the exec floor permits the command:
  `host_shell_dispatch` (line ~1483) → `host_shell_output` (line ~1589):

  ```rust
  async fn host_shell_output(cmd: &str, cwd: &str) -> std::io::Result<std::process::Output> {
      fn shell(program: &str, cmd: &str, cwd: &str) -> tokio::process::Command {
          let mut c = tokio::process::Command::new(program);
          c.arg("-c").arg(cmd).current_dir(cwd);
          c
      }
      match shell("bash", cmd, cwd).output().await { … }
  }
  ```

  `.output().await` blocks **forever** with:
  - no `tokio::time::timeout` wrapper,
  - no cancellation token checked while it runs,
  - no `kill_on_drop(true)`,
  - no dedicated process group, so a signal caught by the TUI can't be
    forwarded to the whole child tree.

If you hit this while a program hung, you were almost certainly running with
OCAP disabled (yolo/full-access). The confined path would have timed out; the
bypass path cannot.

### Fix 1a — bound the host-bypass exec with a timeout + kill-on-drop

```rust
#[cfg(not(windows))]
async fn host_shell_output(cmd: &str, cwd: &str) -> std::io::Result<std::process::Output> {
    use std::os::unix::process::CommandExt;
    fn shell(program: &str, cmd: &str, cwd: &str) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(program);
        c.arg("-c").arg(cmd).current_dir(cwd);
        c.kill_on_drop(true);
        // Own process group so we can signal the whole child tree, not just
        // the shell — a child that ignores SIGTERM on the shell alone would
        // otherwise survive.
        unsafe { c.pre_exec(|| { libc::setsid(); Ok(()) }); }
        c
    }
    let fut = async {
        match shell("bash", cmd, cwd).output().await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                shell("sh", cmd, cwd).output().await
            }
            other => other,
        }
    };
    match tokio::time::timeout(host_exec_timeout(), fut).await {
        Ok(res) => res,
        Err(_elapsed) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("command exceeded {}s host-exec timeout", host_exec_timeout().as_secs()),
        )),
    }
}
```

Make the timeout configurable (default e.g. 120s), mirroring the confined
shell's bound, and surface `timed_out: true` in `host_shell_dispatch`'s
envelope so the model sees the same shape either way:

```rust
async fn host_shell_dispatch(cmd: &str, cwd: &str) -> std::io::Result<serde_json::Value> {
    match host_shell_output(cmd, cwd).await {
        Ok(output) => Ok(serde_json::json!({
            "exit_code": output.status.code().unwrap_or(-1),
            "stdout": decode_shell_stream(&output.stdout),
            "stderr": decode_shell_stream(&output.stderr),
            "timed_out": false,
            "sandbox_kind": "none",
        })),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Ok(serde_json::json!({
            "exit_code": 124,               // conventional "timed out" code
            "stdout": "",
            "stderr": e.to_string(),
            "timed_out": true,
            "sandbox_kind": "none",
        })),
        Err(e) => Err(e),
    }
}
```

### Fix 1b — thread the turn cancel token into exec (make Esc/Ctrl-C actually stop a running command)

The TUI already trips a cancel flag on Ctrl-C/Esc (`turn_cancel` /
`with_interrupt_watch` in `newt-tui/src/lib.rs:6520,9314`), but the exec future
does **not** observe it. Pass an `Arc<AtomicBool>` (or a
`tokio_util::sync::CancellationToken`) down into `exec_confined_command` /
`host_shell_output` and race it against the command:

```rust
let out = tokio::select! {
    r = shell(…).output() => r?,
    _ = wait_for_cancel(cancel.clone()) => {
        // future dropped here => kill_on_drop fires => child tree dies
        return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"));
    }
};
```

`kill_on_drop(true)` + `setsid` means dropping the future on cancel kills the
process group, so a runaway child stops on the first Esc instead of the second
Ctrl-C.

---

## 2. Why Ctrl-C / Ctrl-D didn't reach you

The interrupt watcher (`newt-tui/src/lib.rs:9353` `watch_for_interrupt`) polls
stdin and looks for a lone `0x03` (Ctrl-C) byte. Two problems:

1. **Stdin contention.** While a host-bypass child runs on the real tty, the
   child (and its shell) may hold the terminal in a mode where the watcher's
   `poll`/read never sees the byte — the child consumed it. Put the child in
   its own process group (Fix 1a `setsid`) and, for host runs, redirect the
   child's stdin from `/dev/null` unless the command explicitly needs a tty, so
   the watcher keeps ownership of the keyboard.
2. **Ctrl-D (EOF) is not handled during a turn.** `ReadOutcome::Eof` is only
   handled at the input prompt (`lib.rs:6862`), not while a turn/command runs.
   Add an EOF branch in `watch_for_interrupt` that treats a mid-turn Ctrl-D as
   "hard cancel" (same as second Ctrl-C → `turn_hard`).

### Fix 2 — add a SIGINT/SIGTERM handler as a backstop

The byte-poll watcher is TUI-only. Register an OS-level handler
(`tokio::signal::ctrl_c()` on the async side, and `signal::unix` for SIGTERM)
that trips the same `turn_hard` flag and kills the tracked child process group.
That guarantees an escape hatch even when the keyboard watcher is starved.

---

## 3. MCP server enablement (`newt-mcp-client`, `newt-tui/src/mcp.rs`)

Observations:

- `newt-mcp-client/src/lib.rs` is the only source file in that crate — the
  client is thin. The TUI wires servers in `newt-tui/src/mcp.rs` /
  `mcp_token.rs`.

Recommended improvements to make `pa` and the MCP servers usable:

1. **Per-server startup timeout + health check.** An MCP server that never
   completes its `initialize` handshake will hang the TUI the same way a stuck
   shell command does. Wrap the handshake in `tokio::time::timeout` and, on
   failure, mark that server "degraded" and continue — never block the whole
   session on one server.
2. **Per-tool-call timeout.** Same treatment for individual MCP `tools/call`
   requests: bound each call, return a structured timeout error to the model
   rather than blocking.
3. **Graceful shutdown / child reaping.** On exit or Ctrl-C, send the MCP
   servers a shutdown and kill their subprocesses (again `kill_on_drop` +
   process group) so orphaned server processes don't linger.
4. **Config surface for `pa`.** Add a documented `[[mcp.servers]]` block
   (command, args, env, cwd, `startup_timeout`, `call_timeout`, `enabled`) so
   enabling `pa` is config, not code. Ship an example in the README.
5. **`newt doctor` MCP probe.** `newt-cli/src/doctor.rs` already exists —
   extend it to (a) launch each configured MCP server, (b) run `initialize`
   under timeout, (c) list advertised tools, (d) report pass/degraded/fail.
   That turns "MCP won't work" into a one-command diagnosis.

---

## 4. Concrete change checklist

- [ ] `host_shell_output`: add `tokio::time::timeout`, `kill_on_drop(true)`,
      `setsid`/process-group, `/dev/null` stdin (Fix 1a).
- [ ] `host_shell_dispatch`: emit `timed_out` + exit code 124 on timeout (Fix 1a).
- [ ] Add `host_exec_timeout()` reading config/env (default 120s).
- [ ] Thread the turn cancel token into `exec_confined_command` and race it
      with `tokio::select!` (Fix 1b).
- [ ] `watch_for_interrupt`: handle mid-turn Ctrl-D (EOF) as hard cancel (Fix 2).
- [ ] Register `tokio::signal::ctrl_c()` + SIGTERM backstop that trips
      `turn_hard` and kills the tracked child group (Fix 2).
- [ ] MCP: startup + per-call timeouts, degraded-server handling, child reaping
      (Section 3).
- [ ] `newt doctor`: add MCP server probe (Section 3.5).
- [ ] README: document `[[mcp.servers]]` config incl. `pa` example.

The single highest-value change is **Fix 1a + 1b**: it directly resolves
"ran a program, it got stuck, couldn't Ctrl-C." Everything else hardens around
that.

---

## 5. Agent-facing harness friction (observed live while writing this doc)

These are not about the hang — they are concrete affordances that blocked *the
agent itself* from completing an ordinary task (make a branch, stage fixes,
commit). Each one caused a dead end this session. Fixing them is the difference
between "the agent can do version control" and "it cannot."

### 5a. No usable `git` — the biggest blocker

Version control is currently **impossible** from inside the harness:

- Passing `git …` to `run_command` is rejected:
  `"'git' is a tool, not a shell command. Call it as a separate tool
  invocation"` — but **no `git` tool is actually exposed** in the tool set.
  The error points at an affordance that doesn't exist.
- Invoking `/usr/bin/git` directly trips the macOS sandbox:
  `xcrun: error: unable to load libxcrun … file system sandbox blocked
  open()`. The sandbox profile blocks the Xcode/git toolchain's file access.

**Fix:** either (a) expose a first-class `git` tool (subcommand + args + cwd),
or (b) whitelist `git` in the confined-shell exec floor AND widen the sandbox
profile to allow the Developer-tools paths git needs
(`/Library/Developer/CommandLineTools/…`, `libxcrun`, the selected
`xcode-select` toolchain). Without one of these, the agent can read and edit
files but can never commit them — every multi-step coding task dead-ends at
"now commit."

### 5b. `cd` is blocked; `run_command` has no `cwd`

`cd anywhere` inside `run_command` fails:
`sandbox-exec: execvp() of 'cd' failed: Operation not permitted`. There is also
no `cwd` parameter on `run_command`, so the only way to act on a subdirectory
is to pass absolute paths to every command (`git -C <path>`, `ls <path>`).

**Fix:** add an optional `cwd` parameter to `run_command` (chdir before exec),
or allow `cd` as a shell builtin in the confined engine. This alone removes a
large class of "command works in the wrong directory" retries.

### 5c. Confined shell rejects common metacharacters

- Bare `$` is rejected:
  `"not yet supported by the confined shell engine: bare `$` (escape as
  `\$`)… (agent-bridle#34)"`. So `echo $?`, `$(…)`, and `${VAR}` all fail
  unless escaped, and the escaping isn't obvious at call time.
- Output redirection to some targets is denied by fs_write scope even for
  throwaway sinks: `echo x > /dev/null` →
  `"denied: write of /dev/null is not within the granted fs_write scope."`

**Fix:** either support `$`, command substitution, and `>` redirection in the
confined shell, or emit a one-line "here is how to escape it" hint in the error
(the error already references `agent-bridle#34` — surface the workaround inline).
Whitelist `/dev/null` (and `/dev/stdout`, `/dev/stderr`) as always-writable
sinks; writing to them is never a real filesystem mutation.

### 5d. Round budget too low for real tasks

Hit the `[tui].max_tool_rounds` ceiling (25) mid-task at ~97k in / 13k out
tokens — well under the context window. A single "read a few files, make edits,
verify, commit" loop routinely needs more than 25 tool rounds.

**Fix:** raise the default `max_tool_rounds` (e.g. 60–80), and/or make the limit
a soft warning the agent can acknowledge and continue past, rather than a hard
turn-ending stop. A hard stop mid-edit leaves uncommitted work — the worst
possible failure mode.

### 5e. Capability-grant loop is opaque

When an action needs authority the agent lacks (git, a path outside the
workspace, a network host), the failure text is good but the *recovery* path
(`request_permissions`) is easy to miss, and in a headless run there's no
operator to grant it. 

**Fix:** in the denial message, name the exact `request_permissions` call that
would unblock (capability + target), and in `newt doctor` list which
capabilities the current launch actually granted, so a user can relaunch with
`--write <path>` / a git grant *before* starting a task rather than discovering
the gap 20 rounds in.

### 5f. Priority for the next rebuild-and-relaunch cycle

To make the agent measurably more effective, in order:

1. **Expose `git`** (5a) — unblocks all version-control work. Highest leverage.
2. **`cwd` on `run_command`** (5b) — removes the largest retry class.
3. **Raise/soften `max_tool_rounds`** (5d) — stops mid-task truncation.
4. **`$` + redirection + `/dev/null` in confined shell** (5c) — removes small
   constant friction on nearly every command.
5. **Sharper denial → `request_permissions` hints** (5e) — faster recovery.

Then the exec/interrupt fixes (Sections 1–2) so a launched program can't hang
the session, and the MCP work (Section 3) so `pa` and friends come up reliably.
