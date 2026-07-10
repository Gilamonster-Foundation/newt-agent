---
name: tmux-drive
description: Drive an interactive TTY/TUI program hands-free from a non-interactive agent by launching it in a tmux pane and using send-keys + capture-pane.
when_to_use: When you must operate a program that requires a real terminal — a full-screen TUI, a REPL, an interactive installer/prompt, or anything that refuses to run under a pipe or redirect (e.g. errors like "No such device or address" / "not a tty"). Also when you want to observe and type into a long-lived interactive session a human is watching.
version: 1.0.0
license: Apache-2.0
caveats:
  exec: { only: ["tmux"] }
  fs_read: all
  net: { only: [] }
  max_calls: unlimited
---

# Drive an interactive TUI via tmux

A non-interactive agent shell has no controlling terminal, so programs that
demand a TTY die immediately (`No such device or address` / `not a tty`) and
piping input to them does not work — the program reads raw keystrokes from the
terminal, not from stdin. **A tmux pane is a real pseudo-terminal.** Launch the
program *inside a tmux pane* and you can type into it with `send-keys` and read
its rendered screen with `capture-pane`. This turns any interactive TUI into
something an agent can operate step by step.

## The one rule that will bite you: never target your own pane

You are (usually) already running inside a tmux pane. `send-keys` with a
**missing, empty, or wrong** `-t` target defaults to the **active pane** — which
may be *you*. Sending keys to your own pane injects text into your own input and
submits it as if the user typed it. Guard every send:

```bash
# Your own pane id — NEVER send here.
echo "self = $TMUX_PANE"

# Resolve the TARGET pane id deterministically (see below), then guard:
send() {  # send "<pane_id>" "<literal keys...>"
  local p="$1"; shift
  [ -n "$p" ] && [ "$p" != "$TMUX_PANE" ] || { echo "REFUSING self/empty target"; return 1; }
  tmux send-keys -t "$p" "$@"
}
```

## Get the target pane id deterministically

Do **not** compute the id with `tmux list-panes -t <session>:` — that lists only
the session's *active* window, so a new window won't appear and your variable
comes out empty (→ you hit your own pane). Instead capture the id at creation:

```bash
# New window in the current session (user can switch to it to watch);
# -d = don't steal their focus; -P -F prints the new pane id.
P=$(tmux new-window -d -P -F '#{pane_id}' -n worktui -c /path/to/cwd 'the-tui-command')

# ...or a fully separate detached session:
# tmux new-session -d -s drive -P -F '#{pane_id}' -c /path 'the-tui-command'

echo "target pane = $P"   # e.g. %13
```

`#{pane_id}` (`%13`) is stable for the life of the pane — persist it (a file or
a var) and reuse it for every subsequent `send-keys`/`capture-pane`.

## The drive loop: send → wait → capture

```bash
send "$P" "some text here"      # types the text (no submit)
send "$P" Enter                 # submits — key names (Enter, Escape, Tab,
                                #   C-c, Up, Down, BSpace) are literal args
sleep 0.6                       # TUIs redraw asynchronously — always pause
tmux capture-pane -t "$P" -p | grep -vE '^\s*$' | tail -20   # read the screen
```

- `capture-pane -p` dumps the **visible** screen; add `-S -200` for scrollback.
- Prefer **polling** over a fixed sleep for slow steps: re-capture in a loop
  until the text you expect appears (or a timeout), e.g. a spinner clears or a
  prompt returns.
- Send literal text and control keys in **separate** `send-keys` calls when in
  doubt; `send-keys "foo" Enter` also works (each arg is a key/keystring).

## Gotchas learned the hard way

- **Splash / "press any key" screens hang forever** because they wait on a
  keystroke you never sent — it looks like a freeze at 0% CPU. Either send a
  key to dismiss it, or use the program's own flag (newt: `--no-splash`).
- **Empty/blank capture** usually means the app is still initializing (model
  load, MCP handshake, backend probe). Check it's alive and *why* it's idle:
  `tmux list-panes -a -F '#{pane_id} #{pane_current_command} dead=#{pane_dead}'`
  and `ps -o stat,etimes,cmd <pid>`. `Ssl+ … 0.0` = blocked-waiting, not busy.
- **A human may be driving the same pane.** If you share a window with the user,
  take turns — two writers into one input line corrupt each other. Capture and
  read the input line before you type; if you left a stray char, back it out
  (`send "$P" BSpace`) rather than assuming it's gone.
- **Don't steal focus:** create windows with `-d`. The user switches to your
  window with `<prefix> <n>` when they want to watch.
- **Clean up:** `tmux kill-window -t "$P"` (or `kill-session`) when done so you
  don't leak panes/processes (and, for apps like newt, release any lock/claim
  the session holds).

## Worked example — driving `newt` (a TTY-only TUI)

```bash
NB=~/workspaces/bin/newt
# Launch in a scratch dir, no splash so it lands on the prompt directly:
P=$(tmux new-window -d -P -F '#{pane_id}' -n newt -c /tmp/scratch "$NB code --no-splash")
sleep 3
tmux capture-pane -t "$P" -p | tail -20            # confirm the input box is up

# Guarded slash-command drive:
tmux send-keys -t "$P" "/roadmap new Demo" Enter;  sleep 0.6
tmux send-keys -t "$P" "/tree" Enter;              sleep 0.6
tmux capture-pane -t "$P" -p | tail -30            # read the rendered tree

tmux kill-window -t "$P"                            # done — releases the claim
```

The same pattern drives any REPL, `ssh` prompt, `top`, `vi`, an interactive
`gh`/`git rebase -i`, or a language shell — anything that needs a terminal.
