#!/usr/bin/env bash
# tdrive — drive a TTY/TUI program from a non-interactive agent, SAFELY.
#
# SAFETY MODEL — a dedicated tmux server, addressed by socket label:
#   Every tmux call is pinned to `tmux -L "$TDRIVE_SOCK"`. Your own agent
#   session lives on the DEFAULT server ($TMUX). A `-L <label>` server is a
#   SEPARATE tmux process in a separate namespace. Nothing you do against it —
#   not send-keys, not kill-window, not even `kill-server` — can reach the
#   session you are running in. Self-termination is structurally impossible,
#   which the plain-`tmux` method could not guarantee: an empty/wrong `-t`
#   target defaults to YOUR active pane.
#
# STATE LIVES IN A FILE, not a shell variable:
#   In an agent harness, shell state does NOT persist between tool calls. A
#   `P=$(...)` var is gone on the next call, and `kill-window -t ""` then kills
#   the active (your) window. So each subcommand re-reads the pane id from disk.
#
# Usage (each line is a separate agent tool call — that's the point):
#   TDRIVE_SOCK=scrybe tdrive.sh start "<cmd...>" [cwd]   # launch; prints pane id
#   TDRIVE_SOCK=scrybe tdrive.sh send  <keys...>          # send-keys to the pane
#   TDRIVE_SOCK=scrybe tdrive.sh capture [tail_n]         # visible screen
#   TDRIVE_SOCK=scrybe tdrive.sh wait  "<regex>" [secs]   # poll until it appears
#   TDRIVE_SOCK=scrybe tdrive.sh status                   # list panes on the server
#   TDRIVE_SOCK=scrybe tdrive.sh stop                     # kill the WHOLE server (safe)
#
# TDRIVE_SOCK namespaces independent jobs; default "tdrive". State dir override:
# TDRIVE_STATE_DIR (default $TMPDIR/tdrive).
set -uo pipefail

SOCK="${TDRIVE_SOCK:-tdrive}"
[ -n "$SOCK" ] || { echo "tdrive: empty TDRIVE_SOCK" >&2; exit 2; }
# Refuse the one label that would defeat the whole design.
[ "$SOCK" != "default" ] || { echo "tdrive: refusing socket 'default' — that is your own server" >&2; exit 2; }

STATE_DIR="${TDRIVE_STATE_DIR:-${TMPDIR:-/tmp}/tdrive}"
mkdir -p "$STATE_DIR"
PANE_FILE="$STATE_DIR/$SOCK.pane"

# EVERY tmux invocation goes through here — always pinned to the isolated server.
tm() { tmux -L "$SOCK" "$@"; }

read_pane() {
  local p
  p="$(cat "$PANE_FILE" 2>/dev/null || true)"
  [ -n "$p" ] || { echo "tdrive: no pane for -L $SOCK (run 'start' first)" >&2; exit 2; }
  printf '%s' "$p"
}

cmd="${1:-}"; shift 2>/dev/null || true
case "$cmd" in
  start)
    run="${1:?usage: start \"<cmd>\" [cwd]}"; cwd="${2:-$PWD}"
    # A fresh detached session on the isolated server. If a stale server exists
    # from a prior run, reuse it; new-session gives us a clean pane regardless.
    p="$(tm new-session -d -s drive -P -F '#{pane_id}' -c "$cwd" "$run")"
    printf '%s\n' "$p" > "$PANE_FILE"
    printf '%s\n' "$p"
    ;;
  send)
    p="$(read_pane)"
    tm send-keys -t "$p" "$@"
    ;;
  capture)
    p="$(read_pane)"; n="${1:-40}"
    tm capture-pane -t "$p" -p | grep -vE '^[[:space:]]*$' | tail -n "$n"
    ;;
  wait)
    p="$(read_pane)"; re="${1:?usage: wait <regex> [secs]}"; secs="${2:-10}"
    tries=$(( secs * 3 )); i=0
    while [ "$i" -lt "$tries" ]; do
      if tm capture-pane -t "$p" -p 2>/dev/null | grep -qE "$re"; then
        echo "MATCH"; exit 0
      fi
      sleep 0.33; i=$(( i + 1 ))
    done
    echo "TIMEOUT after ${secs}s waiting for /$re/" >&2
    tm capture-pane -t "$p" -p 2>/dev/null | grep -vE '^[[:space:]]*$' | tail -n 8 >&2
    exit 1
    ;;
  status)
    tm list-panes -a -F '#{session_name}:#{window_index}.#{pane_index} #{pane_id} #{pane_current_command} dead=#{pane_dead}' 2>/dev/null \
      || echo "(no server on -L $SOCK)"
    ;;
  stop)
    # Nuclear-but-safe: tears down ONLY the isolated server. Cannot reach $TMUX.
    tm kill-server 2>/dev/null || true
    rm -f "$PANE_FILE"
    # kill-server can leave a stale socket file behind — unlink ours so runs
    # with unique labels don't accumulate dead sockets under the tmux tmpdir.
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SOCK"
    echo "stopped: killed server -L $SOCK"
    ;;
  *)
    echo "usage: tdrive.sh {start|send|capture|wait|status|stop}" >&2
    exit 2
    ;;
esac
