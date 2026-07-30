---
name: tmux-drive
description: Drive an interactive TTY/TUI program hands-free from a non-interactive agent by launching it on a DEDICATED tmux server (a separate socket, never your own) and using send-keys + capture-pane. Isolation makes killing your own session structurally impossible.
when_to_use: When you must operate a program that requires a real terminal — a full-screen TUI, a REPL, an interactive installer/prompt, or anything that refuses to run under a pipe or redirect (e.g. errors like "No such device or address" / "not a tty"). Also when you want to observe and type into a long-lived interactive session a human is watching.
version: 2.1.0
license: Apache-2.0
caveats:
  exec: { only: ["tmux", "bash", "cat", "mkdir", "grep", "tail", "sleep", "printf", "seq", "chmod", "python", "winpty"] }
  fs_read: all
  net: { only: [] }
  max_calls: unlimited
---

# Drive an interactive TUI via tmux — on a server that can't kill you

A non-interactive agent shell has no controlling terminal, so programs that
demand a TTY die immediately (`No such device or address` / `not a tty`) and
piping input to them does not work — the program reads raw keystrokes from the
terminal, not from stdin. **A tmux pane is a real pseudo-terminal.** Launch the
program *inside a tmux pane* and you can type into it with `send-keys` and read
its rendered screen with `capture-pane`.

## The rule that supersedes every other rule: use a dedicated tmux server

You are (usually) already running inside a tmux pane on the **default** server
(`echo $TMUX` → `/tmp/tmux-1000/default,…`). If you launch and kill test panes
on *that same server*, one wrong target and you kill **yourself** — the agent
session — and everything stops.

Two things make that near-inevitable if you share the default server:

1. **An empty/wrong `-t` target defaults to your own active pane.** `tmux
   kill-window -t ""` does not error — it kills the *current* window. That's you.
2. **Shell variables do not survive between agent tool calls.** A `P=$(tmux
   new-window … )` var is gone by your next call. So on cleanup `$P` is empty,
   and rule 1 fires. (This is the exact bug that killed a Scrybe session: `tmux
   kill-window -t ""` run during cleanup, on the shared server.)

**The fix is structural, not vigilance:** run everything on a **separate tmux
server**, addressed by a socket label — `tmux -L <label> …`. A `-L` server is a
*different tmux process in a different namespace*. Your session isn't on it, so
**nothing you do there can reach you** — not `send-keys`, not `kill-window`, not
even `kill-server`. The catastrophic cleanup becomes a no-op against your
session. This is belt-and-suspenders made structural: even if you forget the
self-pane guard, get an empty target, or nuke the whole server, you are safe.

```
default server ($TMUX)           -L drive server (a separate process)
 └─ your agent session ← safe      └─ drive:0  the TUI you operate
                                      tmux -L drive kill-server ← nukes only THIS box
```

## Use the helper — `scripts/tdrive.sh`

The helper encodes the safe pattern so you don't hand-roll tmux. It pins every
call to `-L "$TDRIVE_SOCK"` and **persists the pane id to a file** (surviving
across your tool calls). One subcommand per tool call — that's the point.

```bash
TD=.newt/bundled-skills/tmux-drive/scripts/tdrive.sh   # path to the helper
SOCK=myjob                                             # names this isolated server

# launch on the isolated server (prints the pane id, also saved to a state file)
TDRIVE_SOCK=$SOCK bash "$TD" start "/path/to/the-tui --flags" /path/to/cwd

# drive it: send keys, then WAIT for expected text (poll, don't fixed-sleep)
TDRIVE_SOCK=$SOCK bash "$TD" send  "/some-command"
TDRIVE_SOCK=$SOCK bash "$TD" send  Enter          # key names: Enter Escape Tab C-c Up Down BSpace
TDRIVE_SOCK=$SOCK bash "$TD" wait  'expected text' 8   # poll ≤8s for the regex

# read the rendered screen
TDRIVE_SOCK=$SOCK bash "$TD" capture 30            # last 30 non-blank lines

# what's running on the isolated server
TDRIVE_SOCK=$SOCK bash "$TD" status

# tear it ALL down — kill-server on the isolated socket, provably harmless to you
TDRIVE_SOCK=$SOCK bash "$TD" stop
```

`TDRIVE_SOCK` namespaces independent jobs — use a distinct label per program so
their state files and servers don't collide. `stop` is the *only* cleanup you
need and it can never touch your session.

## If you must hand-roll it

Every tmux command carries `-L "$SOCK"`. Never a bare `tmux` mutation.

```bash
SOCK=drive; mkdir -p /tmp/tdrive
# launch on the isolated server; save the pane id to a FILE, not a var
tmux -L "$SOCK" new-session -d -s drive -P -F '#{pane_id}' -c /path 'the-tui --flags' \
  > /tmp/tdrive/$SOCK.pane
P=$(cat /tmp/tdrive/$SOCK.pane)              # re-read from disk every tool call

tmux -L "$SOCK" send-keys -t "$P" "text"      # type (no submit)
tmux -L "$SOCK" send-keys -t "$P" Enter        # submit
tmux -L "$SOCK" capture-pane -t "$P" -p | tail -20   # read screen (add -S -200 for scrollback)

tmux -L "$SOCK" kill-server                     # cleanup: nukes ONLY this server
```

Defense in depth (cheap, keep it): a guard that refuses an empty target or your
own pane, in case a stray bare-`tmux` slips in.

```bash
send() { local p="$1"; shift
  [ -n "$p" ] && [ "$p" != "$TMUX_PANE" ] || { echo "REFUSING self/empty target"; return 1; }
  tmux -L "$SOCK" send-keys -t "$p" "$@"; }
```

## The drive loop: send → wait → capture

- **Wait, don't fixed-sleep.** TUIs redraw asynchronously; poll `capture-pane`
  until the text you expect appears (or a timeout). The helper's `wait` does
  exactly this. A blind `sleep 0.6` is a race.
- `capture-pane -p` dumps the **visible** screen; `-S -200` adds scrollback.
- `capture-pane -p` strips trailing whitespace — a footer `" 0% "` arrives as
  `…0%`. Assert with a regex like `(^| )0%`, never a literal trailing space.
- Send literal text and control keys in **separate** sends when unsure;
  `send-keys "foo" Enter` (each arg a key/keystring) also works.

## Gotchas learned the hard way

- **Splash / "press any key" screens hang forever** waiting on a keystroke you
  never sent — looks like a freeze at 0% CPU. Send a key to dismiss it, or use
  the program's flag (newt: `--no-splash`).
- **Empty/blank capture** usually means the app is still initializing (model
  load, MCP handshake, backend probe) — not dead. Check:
  `tmux -L "$SOCK" list-panes -a -F '#{pane_id} #{pane_current_command} dead=#{pane_dead}'`
  and `ps -o stat,etimes,cmd <pid>`. `Ssl+ … 0.0` = blocked-waiting, not busy.
- **The isolated server hides your work from the human by design.** It's a
  private server on its own socket. If a human wants to watch, they attach in a
  spare terminal: `tmux -L "$SOCK" attach`. You are not sharing their pane, so
  the old "take turns / two writers corrupt the input line" hazard is gone.
- **Cleanup is one call:** `tmux -L "$SOCK" kill-server` (or the helper's
  `stop`). It releases every pane/process/lock the drive held (for apps like
  newt, the session claim) and cannot reach your own server.

## Worked example — driving `scrybe-tui` (tested, real)

```bash
TD=.newt/bundled-skills/tmux-drive/scripts/tdrive.sh
BIN=target/debug/scrybe-tui
printf '# Doc\n\n' > /tmp/d/long.md; for i in $(seq 1 200); do printf 'line %s\n\n' "$i" >> /tmp/d/long.md; done

TDRIVE_SOCK=scrybe bash "$TD" start "$BIN long.md" /tmp/d
TDRIVE_SOCK=scrybe bash "$TD" wait '(^| )0%' 8      # opens at top
TDRIVE_SOCK=scrybe bash "$TD" send G                 # jump to bottom
TDRIVE_SOCK=scrybe bash "$TD" wait '100%' 5
TDRIVE_SOCK=scrybe bash "$TD" send g                 # back to top
TDRIVE_SOCK=scrybe bash "$TD" wait '(^| )0%' 5
TDRIVE_SOCK=scrybe bash "$TD" stop                   # done — your session untouched
```

## Worked example — driving `newt` (a TTY-only TUI)

```bash
TD=.newt/bundled-skills/tmux-drive/scripts/tdrive.sh
NB=~/workspaces/bin/newt
TDRIVE_SOCK=newt bash "$TD" start "$NB code --no-splash" /tmp/scratch
TDRIVE_SOCK=newt bash "$TD" wait '❯' 8               # input box is up
TDRIVE_SOCK=newt bash "$TD" send '/roadmap new Demo'; TDRIVE_SOCK=newt bash "$TD" send Enter
TDRIVE_SOCK=newt bash "$TD" send '/tree';             TDRIVE_SOCK=newt bash "$TD" send Enter
TDRIVE_SOCK=newt bash "$TD" wait 'Demo' 5
TDRIVE_SOCK=newt bash "$TD" capture 30
TDRIVE_SOCK=newt bash "$TD" stop                      # releases the session claim
```

The same pattern drives any REPL, `ssh` prompt, `top`, `vi`, an interactive
`gh`/`git rebase -i`, or a language shell — anything that needs a terminal.
Always on its own `-L` server.

## Windows addendum — the pseudoconsole is the tmux equivalent

There is no tmux on native Windows (Git Bash), and WSL — the usual way to get
one — needs a reboot you may not be able to take. But everything above still
applies conceptually: a program that demands a TTY needs a real pseudo-terminal,
and Windows has one — **ConPTY**, exposed to Python by `pywinpty` (a pipe still
won't do). Spawn the TUI inside a pseudoconsole and you `send`-keys / read-screen
exactly as with a tmux pane.

**The one structural difference:** a tmux pane lives on a *server*, so `tdrive.sh`
can be one-subcommand-per-tool-call (state persists on the socket). A
pseudoconsole is an **in-process object** — it dies when the Python process
exits. So on Windows you write a short **driver script** (one persistent process
that spawns the TUI and drives it), rather than separate CLI calls. There is no
"kill yourself" hazard to guard against — the PTY is a child of your own script,
not a shared server.

Use the helper `scripts/wdrive.py` (the Windows sibling of `tdrive.sh`; needs
`pywinpty` + Git Bash's bundled `winpty`, both no-admin/no-reboot):

```python
from wdrive import Tui   # scripts/wdrive.py

# env= sets vars for the child (see gotcha 2). A pseudoconsole is a REAL tty.
t = Tui([r"C:\path\to\newt.exe", "-n", "--no-splash"],
        env={"NEWT_NO_MODEL_PULL": "1"})
t.wait(r"❯|ready")            # poll for the live prompt (never fixed-sleep)
t.send("/persona switch bob") # types text + Enter
t.wait(r"persona .bob.")      # RAISES TimeoutError if it never appears (loud)
t.send("/persona")            # show status
t.wait(r"role: researcher")
print(t.screen())             # ANSI-stripped screen for asserts
t.quit("/quit")
```

`wait()` **raises `TimeoutError` on timeout by default** — a driver must never
silently sail past a missing expectation and then assert on stale screen bytes.
For a genuinely optional/branching match, use `try_wait()` (or
`wait(..., raise_on_timeout=False)`), which returns a bool instead.

**Two Windows gotchas the helper handles / you must know:**

1. **cp1252 print crash.** A TUI emits unicode (spinner glyphs, `❯`); Python's
   default Windows stdout is cp1252 and *crashes on `print`*. `wdrive` reconfigures
   stdout to utf-8 — or run under `PYTHONUTF8=1`.
2. **A pseudoconsole presents as a REAL tty**, so any program that gates
   expensive first-run work on `isatty()` will fire it here where a pipe would
   not. For `newt` specifically, that's the 468 MB on-host summarizer pull —
   pass `NEWT_NO_MODEL_PULL=1` in `env` or the prompt never comes ready. (General
   rule: find the program's headless/no-provision opt-out and set it.)

Same pattern drives any Windows console REPL, installer prompt, or full-screen
TUI — anything that refuses to run under a pipe.
