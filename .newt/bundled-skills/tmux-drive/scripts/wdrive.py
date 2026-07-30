#!/usr/bin/env python3
"""wdrive — the Windows-native equivalent of tmux-drive's tdrive.sh.

Windows has no tmux, and WSL needs a reboot. But a Windows PSEUDOCONSOLE
(ConPTY, exposed by `pywinpty`) IS a real PTY: spawn a console program inside
it, send keystrokes, and read the rendered screen — exactly what a full-screen
TUI / REPL needs and what a plain pipe cannot do.

Difference from tdrive.sh: a tmux pane lives on a server, so tdrive.sh can be
one-subcommand-per-tool-call. A pseudoconsole is an IN-PROCESS object, so on
Windows you write a short DRIVER SCRIPT (one persistent Python process that
spawns the TUI and drives it) instead of separate CLI calls. Import `Tui` and
script the interaction:

    from wdrive import Tui
    t = Tui([r"C:\\path\\to\\app.exe", "--flag"])   # env=... to set vars
    t.wait(r"ready|>")                               # poll (never fixed-sleep)
    t.send("/some-command")                          # types text + Enter
    t.wait(r"expected output")
    print(t.screen())                                # ANSI-stripped screen
    t.quit("/quit")

Requirements (all no-admin, no-reboot on a standard Git-for-Windows box):
  * pywinpty      —  pip/uv install pywinpty   (often already present)
  * winpty        —  bundled with Git Bash (/usr/bin/winpty)

Two gotchas this helper handles for you:
  1. cp1252 print crash — a TUI emits unicode (spinner glyphs, ❯). Python's
     default Windows stdout is cp1252 and CRASHES on print. `Tui` reconfigures
     stdout to utf-8; also fine to set PYTHONUTF8=1.
  2. A pseudoconsole presents as a REAL TTY. Programs that gate expensive
     first-run work on `isatty()` (e.g. newt provisions a 468 MB on-host
     summarizer) WILL fire it here, where a pipe would not. Pass their opt-out
     in `env` (for newt: NEWT_NO_MODEL_PULL=1) or the prompt never comes ready.
"""
import os
import re
import sys
import time
import threading

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except Exception:
    pass

from winpty import PtyProcess  # noqa: E402

_ANSI_CSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
_ANSI_OSC = re.compile(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)")


class Tui:
    """Drive a console TUI in a Windows pseudoconsole."""

    def __init__(self, cmd, env=None, cols=120, rows=40):
        spawn_env = None
        if env:
            spawn_env = dict(os.environ)
            spawn_env.update({k: str(v) for k, v in env.items()})
        self.p = PtyProcess.spawn(cmd, dimensions=(rows, cols), env=spawn_env)
        self._buf = []
        self._lock = threading.Lock()
        self._t = threading.Thread(target=self._pump, daemon=True)
        self._t.start()

    def _pump(self):
        while True:
            try:
                d = self.p.read(4096)
            except (EOFError, OSError):
                break
            if not d:
                break
            with self._lock:
                self._buf.append(d)

    def screen(self):
        """The full captured output, ANSI stripped, for plain-text asserts."""
        with self._lock:
            raw = "".join(self._buf)
        return _ANSI_CSI.sub("", _ANSI_OSC.sub("", raw))

    def wait(self, pattern, timeout=12.0, raise_on_timeout=True):
        """Poll (0.2s) until `pattern` (regex, case-insensitive) appears.

        RAISES `TimeoutError` on timeout by default — for an automation driver a
        timeout should be LOUD: a script that silently continued past a missing
        expectation would then inspect or act on stale terminal contents. Pass
        `raise_on_timeout=False` (or use `try_wait`) for boolean polling when a
        match is genuinely optional.
        """
        end = time.time() + timeout
        while time.time() < end:
            if re.search(pattern, self.screen(), re.I):
                return True
            time.sleep(0.2)
        if raise_on_timeout:
            raise TimeoutError(f"wdrive.wait timed out after {timeout}s for {pattern!r}")
        return False

    def try_wait(self, pattern, timeout=12.0):
        """Boolean poll that never raises — for optional / branching expectations."""
        return self.wait(pattern, timeout, raise_on_timeout=False)

    def send(self, text, enter=True):
        """Type `text` (and Enter). Use send_key for named keys."""
        self.p.write(text + ("\r" if enter else ""))

    def send_key(self, key):
        """Send a control/named key: 'Enter'|'Escape'|'Tab'|'C-c'|'Up'|'Down'."""
        codes = {
            "Enter": "\r", "Escape": "\x1b", "Tab": "\t", "C-c": "\x03",
            "Up": "\x1b[A", "Down": "\x1b[B", "BSpace": "\x7f",
        }
        self.p.write(codes.get(key, key))

    def quit(self, cmd="/quit", grace=1.0):
        try:
            if cmd:
                self.send(cmd)
            time.sleep(grace)
        finally:
            try:
                self.p.terminate(force=True)
            except Exception:
                pass


if __name__ == "__main__":
    # Smoke: `python wdrive.py <exe> [args...]` — spawn, dump 3s of screen.
    if len(sys.argv) < 2:
        print("usage: python wdrive.py <exe> [args...]")
        sys.exit(2)
    t = Tui(sys.argv[1:])
    time.sleep(3)
    print(t.screen()[-2000:])
    t.quit()
