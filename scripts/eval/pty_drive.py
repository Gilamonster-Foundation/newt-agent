#!/usr/bin/env python3
"""pty_drive.py — drive an interactive TUI (e.g. `newt --plain`) headlessly.

grade-loop.sh reproduces the yardstick incident *faithfully*: a human typing
into the interactive TUI. A blind pipe can't — `newt --plain` opens a
controlling terminal (ENXIO on a pipe). This pacer spawns the command under a
real PTY (so the TUI runs interactively, isatty()==True) and types each scripted
line only when the TUI is actually waiting for input.

Readiness is MARKER-ANCHORED, not idle-timed: the TUI is waiting for input
exactly when its visible tail (ANSI stripped) ENDS WITH the prompt glyph `❯`.
While a turn runs, the tail ends with the model's output / spinner instead. This
is robust to periodic redraws and to slow turns (an idle-silence heuristic is
not — it either fires mid-turn or never fires when the prompt redraws).

Prompts: one user turn per line from --prompts (the caller includes the final
`/exit`). Submit is a carriage return `\r` (Enter in a raw-mode TUI); a bare
`\n` just appends. A throwaway priming `\r` absorbs the first-write byte loss a
freshly-raw PTY exhibits. Env (HOME, NEWT_FULL_ACCESS, …) is inherited.

Exit 0 on clean child exit; 124 on overall timeout or an unrecoverable stall.

Usage:
  pty_drive.py --prompts <file> [--workdir <dir>] [--timeout 900]
               [--debounce 2] [--stall 90] [--marker '❯'] [--debug] -- <cmd>...
"""
import argparse
import os
import pty
import re
import select
import sys
import time

# CSI / OSC / 2-char escapes, plus CR and backspace — enough to recover the
# *visible* trailing text so we can test what the prompt actually ends with.
_ANSI = re.compile(rb"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]|[\r\x08]")


def visible_tail(buf: bytes) -> str:
    return _ANSI.sub(b"", buf).decode("utf-8", "replace").rstrip()


def _kill(pid):
    for sig in (15, 9):
        try:
            os.kill(pid, sig)
        except OSError:
            return
        time.sleep(0.3)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--prompts", required=True, help="file, one user turn per line")
    ap.add_argument("--workdir", default=".")
    ap.add_argument("--timeout", type=float, default=900.0, help="overall wall cap (s)")
    ap.add_argument("--debounce", type=float, default=2.0, help="prompt must stay ready this long before typing (s)")
    ap.add_argument("--stall", type=float, default=90.0, help="give up after this much with input pending and no progress (s)")
    ap.add_argument("--marker", default="❯", help="prompt glyph that means 'ready for input'")
    ap.add_argument("--debug", action="store_true")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    a = ap.parse_args()

    cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd
    if not cmd:
        sys.stderr.write("pty_drive: no command after --\n")
        return 2
    with open(a.prompts, encoding="utf-8") as f:
        lines = [ln.rstrip("\n") for ln in f]

    def dbg(msg):
        if a.debug:
            sys.stderr.write("[pty_drive] %s\n" % msg)
            sys.stderr.flush()

    pid, mfd = pty.fork()
    if pid == 0:  # child: run the TUI on the PTY slave
        try:
            os.chdir(a.workdir)
        except OSError:
            pass
        os.execvp(cmd[0], cmd)
        os._exit(127)

    marker = a.marker
    start = time.time()
    last_out = start
    tail = b""
    sent = 0
    primed = False
    output_since_send = True   # startup splash unlocks the first (priming) type
    ready_since = None         # when the visible tail first ended with the marker
    outb = sys.stdout.buffer

    while True:
        now = time.time()
        if now - start > a.timeout:
            dbg("overall timeout")
            _kill(pid)
            return 124
        r, _, _ = select.select([mfd], [], [], 0.3)
        now = time.time()
        if r:
            try:
                data = os.read(mfd, 4096)
            except OSError:
                data = b""
            if not data:
                dbg("child closed PTY (exited)")
                break
            outb.write(data)
            outb.flush()
            tail = (tail + data)[-4096:]
            last_out = now
            output_since_send = True
            continue

        # idle tick: is the TUI sitting at a ready prompt?
        ready_now = visible_tail(tail).endswith(marker)
        if ready_now:
            if ready_since is None:
                ready_since = now
        else:
            ready_since = None

        idle = now - last_out
        if sent < len(lines):
            if not primed:
                # First keystroke: fire once startup output has SETTLED, NOT on
                # the marker. newt 0.7.1 opens on a full-screen splash ("Enter
                # start coder · q quit") that shows no `❯`; this Enter dismisses
                # it (and absorbs the first-write byte loss). Marker-anchored
                # detection then governs the real prompts below.
                if output_since_send and idle >= a.debounce:
                    os.write(mfd, b"\r")
                    primed = True
                    output_since_send = False
                    ready_since = None
                    last_out = now
                    dbg("primed (dismiss splash / absorb first-write)")
                elif idle >= a.stall:
                    dbg("no startup output in %.0fs, giving up" % idle)
                    _kill(pid)
                    return 124
            else:
                stable = ready_since is not None and (now - ready_since) >= a.debounce
                # Backstop: if stuck at a stable ready prompt far too long, type
                # anyway rather than hang forever.
                forced = ready_since is not None and (now - ready_since) >= a.stall
                if stable and (output_since_send or forced):
                    os.write(mfd, (lines[sent] + "\r").encode("utf-8"))
                    dbg("typed[%d]: %r" % (sent, lines[sent]))
                    sent += 1
                    output_since_send = False
                    ready_since = None
                    last_out = now
                elif ready_since is None and idle >= a.stall:
                    dbg("stalled mid-turn %.0fs, giving up" % idle)
                    _kill(pid)
                    return 124
        elif idle >= a.stall:
            dbg("all typed; child did not exit, stopping")
            break

    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
