# Windows cockpit via ConPTY — feasibility spike (#1746)

Status: **spike / investigation** — no production behaviour change. Windows
stays on the classic per-turn surface.
Follows: #1669 (the cockpit), #1744 (cockpit hardening).

Every claim below carries an evidence level, and they are not
interchangeable:

- **PROVEN** — a probe in `cockpit::conpty_probe` demonstrates it directly on
  Windows CI.
- **INFERRED** — strongly suggested by what the probes observed, but not
  isolated by an experiment of its own. Do not cite it as demonstrated.
- **HYPOTHESIS** — plausible, still needs a discriminating test.
- **OUT OF SCOPE** — deliberately not answered here.

The distinction is the point of the spike. One configuration working while
another fails does not by itself establish *why*, and an implementation built
on a mislabelled inference inherits a constraint nobody measured.

## Context

The unix cockpit (`cockpit::pty`) swings the process's **own** fd 1/2 onto a
pty slave with `dup2` and reads the master **in the same process**; the session
thread writes, the presenter thread reads. A pty, **not a pipe**, is
load-bearing: three `is_terminal()` checks decide *behaviour* — `LineCaps`, the
permission gate's `interactive`, and the modal raw path — and a pipe flips all
three. This spike asks whether ConPTY can give Windows the same capability, and
what shape the work takes.

## PROVEN

1. **In-process self-capture is impossible (`probe_a`).** Redirecting our own
   `STD_OUTPUT_HANDLE` onto a pipe makes it `FILE_TYPE_PIPE` and `GetConsoleMode`
   fails on it — `is_terminal()` is false. There is no analogue of the unix
   `dup2(slave, 1)` self-capture: a process cannot attach itself to a ConPTY it
   created. **So the unix cockpit's whole capture mechanism has no Windows
   port** — the terminal-producing work must live in a *different* process.

2. **A ConPTY-hosted child's own stdout AND stderr traverse the pty, and it sees
   a terminal (`probe_b`).** A child launched under `CreatePseudoConsole` +
   `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` had its `println!`/`eprintln!` come back
   through the pseudoconsole output channel (verified by reading the captured pty
   bytes from a file, **not** the parent/test stdio), tagged with the child's own
   `is_terminal()` reading = `true` for both streams. Captured evidence:

   ```
   allocated=true hr=0 created=1 child_exit=0
   …^[[2;1Hrunning 1 test…NEWT_CONPTY_STDOUT_7Z[tty=true]NEWT_CONPTY_STDERR_7Z[tty=true]
   ```

   (An earlier version of this probe asserted only on conhost's *init* bytes and
   wrongly called that success; it did not. This version requires the **child's
   own** output to arrive through the pty.)

3. **Under pipe stdio and no console, the child's output does NOT reach the
   pty — it leaks to the inherited pipes (`probe_c`).** This is the
   discriminating experiment for #2, not a restatement of it: `probe_c` runs the
   *identical* host path with the console acquisition removed, from a parent
   whose std handles are pipes (`cargo test` stdio). Creation is already the
   controlled kind — `bInheritHandles = FALSE`, a zeroed `STARTUPINFOEXW` (so
   **no `STARTF_USESTDHANDLES`** and NULL `hStd*`), and
   `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` as the only handle source — so the
   console is the single remaining variable. Verbatim from Windows CI:

   ```
   NEWT_CONPTY_PROBE_C verdict=leaked header="allocated=false hr=0 created=1 child_exit=0"
   NEWT_CONPTY_PROBE_C pty="\u{1b}[?9001h\u{1b}[?1004h\u{1b}[?25l…\u{1b}[2J\u{1b}[m\u{1b}[H…"
   NEWT_CONPTY_PROBE_C host_stdio_has_markers=true
   ```

   The pseudoconsole was created (`hr=0`) and the child ran and exited cleanly
   (`created=1 child_exit=0`), so the negative result is about handle routing,
   not a broken setup. The pty carried **only conhost's init bytes** — no child
   markers — while the child's stdout AND stderr markers appeared on the host's
   inherited pipes. Two consequences worth stating separately: the child does
   not get the pty, **and** its output escapes to whatever the parent's handles
   point at, which any implementation must treat as an output-containment
   failure, not merely a missing feature.

   `probe_c` asserts this verdict rather than only printing it, so a future
   Windows or toolchain that changes the behaviour fails the spike instead of
   silently invalidating the ADR.

   The probe's host acquires its console with `FreeConsole` → `AllocConsole` →
   repoint std handles to `CONOUT$`/`CONIN$`; because that mutates
   process-global console state it runs in a **separate subprocess**, leaving
   the test process untouched. A real `newt.exe` run interactively already owns
   a console; when its stdout is redirected the cockpit does not engage anyway
   (`is_terminal()` is false → classic path).

4. **The #1744 scanner is portable, verbatim.** `cockpit::ansi` compiles on
   Windows and parses the real ConPTY byte stream; its #3 DEC private-mode
   allowlist drops the pseudoconsole's own modes (win32-input `?9001`, focus
   `?1004`, cursor `?25`). The presenter geometry (`plan_insert`,
   `render_insert`, `resize_erase_from`) is pure `u16`/byte code over
   `crossterm` (cross-platform) — portable, though not extracted here.

## INFERRED (supported, not isolated — do not cite as demonstrated)

- **That a console is the only way to bind a ConPTY child's std handles.**
  What is PROVEN is narrower: *the one alternative tested* — controlled creation
  with no explicit std handles and no console — routes the child to the
  inherited pipes instead of the pty. Configurations **not** tested, and
  therefore not ruled out:
  - passing ConPTY-derived handles explicitly via `STARTF_USESTDHANDLES`
    (`CreatePseudoConsole` does not hand back child-side handles, so this needs
    a different construction to even attempt);
  - a host that already owns a console but whose std handles have been
    redirected away from it;
  - `ClosePseudoConsole`/attach-order variations.

  The console requirement is therefore stated at the strength actually
  demonstrated: **under the tested spawn configuration a console is required**.
  If an implementation wants to avoid process-global console ownership, the
  above are the specific experiments left to run — not a reason to assume it is
  impossible.

- **That the leak is inherent rather than a property of `bInheritHandles`
  semantics.** The child received the parent's pipe handles even with
  `bInheritHandles = FALSE`, which suggests ConPTY's own handle assignment is
  the path — but that mechanism was observed, not instrumented.

## HYPOTHESIS (not demonstrated — for the implementation to settle)

- **A full Windows cockpit works end-to-end.** The presenter mounting an editor
  + scrollback over the pty, routing keys, Ctrl-C, resize — none of that is
  built or tested here. This spike proves only the *capture channel*.
- **The minimum viable process boundary (see below).**

## What ConPTY *requires* vs. what newt *must* do

**Required (PROVEN #1):** the **terminal-producing workload** — whatever writes
the transcript/spinner to fd 1/2 during a turn — must run as a **child process**
hosted under a ConPTY the presenter creates. On unix this is a thread and the
capture is in-process; on Windows a process boundary is unavoidable.

**NOT established — do not assume it:** that the boundary is "the entire Newt
session." The open design question for the implementation is *where the seam
goes*:

- Is only the turn's output-producing execution hosted as the child, with the
  presenter/editor/orchestration staying in the parent?
- Or does the session as a whole move across, turning the in-process
  `SurfaceRequest`/reply `mpsc` into cross-process IPC?

`SurfaceRequest` IPC is warranted **only if** the chosen seam actually separates
the session from the presenter across the process boundary. A narrower seam
(host only the output-producing step, keep the surface protocol in one process)
may avoid it. This spike does **not** recommend session-as-child + IPC; it
records that *some* boundary is required and that its minimum extent must be
scoped before any build.

## Decision

- **In-process self-capture: ruled out** (PROVEN #1).
- **ConPTY child-hosting: viable** — the child's stdout/stderr genuinely
  traverse the pty and it sees a terminal (PROVEN #2), and the host must present
  console std handles **for the spawn configuration tested** (PROVEN #3). That
  a console is the ONLY such configuration is INFERRED, not proven; the
  outstanding experiments are listed there. Prefer the narrowest construction
  that passes them — process-global console ownership is a heavy commitment to
  adopt on an inference.
- **Reuse:** the `ansi` scanner and presenter geometry (PROVEN #4).
- **Before any build:** scope the **minimum viable process boundary** (above)
  and decide whether cross-process `SurfaceRequest` IPC is actually required.
  Do not commit to session-as-child until that is settled.
- Windows stays on the classic per-turn surface. **No behaviour change.**

## OUT OF SCOPE for this spike

- Building the Windows cockpit (presenter/editor/scrollback over the pty).
- The session/`SurfaceRequest` process-boundary rearchitecture.
- Extracting the presenter geometry into a shared module (mechanical follow-up).
- Legacy Windows consoles without VT (`ENABLE_VIRTUAL_TERMINAL_PROCESSING`) —
  classic path.
