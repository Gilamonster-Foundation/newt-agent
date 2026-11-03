"""newt_agent.py — Harbor installed-agent adapter that drives the newt TUI (#1419).

Terminal-Bench measures the TUI a person runs, not a separate headless lane, so
bench behaviour cannot drift from TUI behaviour. Each trial:

1. ``install`` adds tmux (apt, as Harbor's own Terminus-2 agent does) and
   uploads the newt binary, the backend profile as ``~/.newt/config.toml``, the
   on-host summarizer model a set-up TUI already has, and a herdr stand-in;
2. ``run`` starts ``newt --no-splash`` in a detached tmux pane in the task's
   working directory. newt's herdr lifecycle reporting goes to the stand-in,
   which appends each idle/working/blocked report to /logs/agent/lifecycle.log;
3. once newt reports idle, the instruction goes in as one bracketed paste and
   Enter, like a person pasting it;
4. when newt has stayed idle for ``SETTLE_SEC`` (a retry or an approved plan
   starts its next turn without a person), or stops for a person (``blocked``:
   a question, a clarification, an approval), the driver closes any prompt with
   Ctrl-C and sends ``/exit``. newt's own records (``~/.newt/conversations.db``,
   ``usage.jsonl``) and the screen are copied to /logs/agent.

The lifecycle reports only say when to stop. herdr keeps the latest state, so
a turn that starts and ends before the reporter runs can leave no report, and a
failure can be overwritten by the idle that follows it. Whether a turn ran, and
how the last one ended, is read afterwards from newt's own record: the
``turn_outcome`` artifacts in the copied ``conversations.db``.

The driver types nothing but the instruction, that Ctrl-C and ``/exit``. A stop
for a person is newt's behaviour, so the trial ends there and is graded. A last
turn that failed, or newt exiting non-zero at any point after the paste, raises
``NonZeroAgentExitCodeError`` (Harbor records it and still grades). newt never
getting ready, or the instruction leaving no turn in newt's record, raises
``RuntimeError``, which Harbor does not grade: the apparatus failed, not the
model. Both happen within ``READY_TIMEOUT_SEC + START_TIMEOUT_SEC``, under the
shortest Terminal-Bench agent timeout (600 s).

The host polls /logs/agent through Harbor's bind mount, so this adapter needs
Harbor's docker environment.

Run it:

    NEWT_BENCH_BIN=/path/to/glibc-2.36-built/newt \\
    NEWT_BENCH_PROFILE=/path/to/bench.toml \\
    NEWT_BENCH_INITIATIVE=decisive \\
    PYTHONPATH=scripts/eval/harbor \\
    harbor run -a newt_agent:NewtAgent -m newt/<model> <task-or-dataset...>

The profile (endpoint + model) is host-secret and local; ``-m`` is only Harbor's
label. Harbor's own environment does not cross into the container, so every
knob below is applied inside it by this module.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import shlex
import shutil
import sqlite3
import tempfile
import time
from pathlib import Path
from typing import override

from harbor.agents.installed.base import (
    BaseInstalledAgent,
    NonZeroAgentExitCodeError,
    with_prompt_template,
)
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

_NEWT_BIN = os.environ.get("NEWT_BENCH_BIN", os.path.expanduser("~/bin/newt"))
_NEWT_PROFILE = os.environ.get("NEWT_BENCH_PROFILE", "")
# Hosted backends: a key file uploaded to /etc/newt/api-key, which the profile
# names with `api_key_file`. Never an env value or a committed literal.
_API_KEY_FILE = os.environ.get("NEWT_BENCH_API_KEY_FILE", "")
# The on-host context summarizer the default build compacts with. A set-up TUI
# already has it; without it the first TUI start downloads ~470 MB.
_SUMMARIZER = os.environ.get(
    "NEWT_BENCH_SUMMARIZER", os.path.expanduser("~/.newt/models/qwen2.5-0.5b")
)
_TENACITY = os.environ.get("NEWT_BENCH_TENACITY", "")
_INITIATIVE = os.environ.get("NEWT_BENCH_INITIATIVE", "")
# The TUI applies its own round cap (`[tui] max_tool_rounds`, default 40, raised
# by --tenacity). Unset or 40 restates that default, so it is accepted.
_MAX_ROUNDS = os.environ.get("NEWT_BENCH_MAX_ROUNDS", "")
_OCAP = os.environ.get("NEWT_BENCH_OCAP", "")
_SELF_VERIFY = os.environ.get("NEWT_BENCH_SELF_VERIFY", "")
_VERIFY_OUTCOMES = os.environ.get("NEWT_BENCH_VERIFY_OUTCOMES", "")
_SMART = os.environ.get("NEWT_BENCH_SMART", "")
_REQUIRE_FEATURES = os.environ.get("NEWT_BENCH_REQUIRE_FEATURES", "")
# Not read: NEWT_BENCH_CONTEXT_WINDOW (the TUI takes the window the server
# reports, which tb-campaign.sh checks against the pinned ctx before each cell)
# and NEWT_BENCH_MODEL_DIGEST (only the headless contract records a digest).

SESSION = "newt"
LIFECYCLE = "lifecycle.log"
HERDR_SHIM = "/usr/local/bin/newt-bench-herdr"
READY_TIMEOUT_SEC = 180
# newt reports `working` as soon as it accepts a submitted line.
START_TIMEOUT_SEC = 120
SETTLE_SEC = 30
EXIT_TIMEOUT_SEC = 60

# herdr's CLI fallback: newt runs `$HERDR_BIN_PATH pane report-agent ...` on each
# state change (newt-tui/src/herdr/transport.rs) and needs exit 0.
_SHIM = f"""#!/bin/sh
echo "$(date +%s) $*" >> /logs/agent/{LIFECYCLE}
"""

_KILL_NEWT = (
    'for p in /proc/[0-9]*; do [ "$(cat "$p/comm" 2>/dev/null)" = newt ] '
    '&& kill -9 "${p#/proc/}" 2>/dev/null; done; true'
)


def _on(raw: str) -> bool:
    return raw.strip().lower() in ("1", "true", "on", "yes")


def _off(raw: str) -> bool:
    return raw.strip().lower() in ("0", "false", "off", "no")


def _refusals() -> list[str]:
    """Headless-lane knobs the TUI cannot honour. Refused before the trial, so
    a treatment never silently measures the baseline."""
    out = []
    if _OCAP.strip().lower() == "on":
        out.append("NEWT_BENCH_OCAP=on: the confined lane was headless-only; the TUI has no twin yet")
    if _on(_SMART):
        out.append("NEWT_BENCH_SMART: no TUI arm is defined yet")
    if _REQUIRE_FEATURES.strip():
        out.append("NEWT_BENCH_REQUIRE_FEATURES: the TUI has no --require-feature")
    if _MAX_ROUNDS.strip() not in ("", "40"):
        out.append("NEWT_BENCH_MAX_ROUNDS: set `[tui] max_tool_rounds` in the profile instead")
    return out


def _newt_argv() -> list[str]:
    """The command a person would type. OCAP off is the headless
    `--unsafe-host-exec` lane's pair of switches (newt-cli/src/headless.rs)."""
    argv = ["newt", "--no-splash", "--disable-ocap", "--full-access"]
    if _INITIATIVE.strip():
        argv += ["--initiative", _INITIATIVE.strip()]
    if _TENACITY.strip():
        argv += ["--tenacity", _TENACITY.strip()]
    return argv


def _pane_env() -> dict[str, str]:
    env = {
        "LANG": "C.UTF-8",
        "HERDR_ENV": "1",
        "HERDR_PANE_ID": "bench",
        "HERDR_BIN_PATH": HERDR_SHIM,
    }
    if _on(_SELF_VERIFY):
        env["NEWT_SELF_VERIFY"] = "1"
    elif _off(_SELF_VERIFY):
        env["NEWT_SELF_VERIFY"] = "0"
    if _on(_VERIFY_OUTCOMES):
        env["NEWT_VERIFY_OUTCOMES"] = "1"
    return env


def _tmux(*commands: str) -> str:
    """One tmux client running `commands` in order."""
    return "LANG=C.UTF-8 tmux -u " + " \\; ".join(commands)


def _start_command(workdir: str) -> str:
    """Start newt in a detached pane. The pane's shell outlives newt (`sleep`):
    if that shell exited, the kernel would hang up the terminal's foreground
    process group, killing background servers a task asked the agent to leave
    running, which the headless lane never did."""
    env = " ".join(shlex.quote(f"{k}={v}") for k, v in _pane_env().items())
    newt = " ".join(shlex.quote(a) for a in _newt_argv())
    inner = f"env {env} {newt}; echo $? > /logs/agent/newt.exit; exec sleep infinity"
    return f"mkdir -p {shlex.quote(workdir)} && " + _tmux(
        "start-server",
        "set-option -g history-limit 100000",
        "set-option -g default-terminal xterm-256color",
        f"new-session -d -s {SESSION} -x 200 -y 50 -c {shlex.quote(workdir)} {shlex.quote(inner)}",
        f"pipe-pane -o -t {SESSION} 'cat >> /logs/agent/tui.pane'",
    )


def _states(text: str) -> list[str]:
    """The `--state` of each report-agent line in order, with its `--message`
    when there is one (`idle:failed`)."""
    out = []
    for line in text.splitlines():
        if " report-agent " not in line:
            continue
        state = re.search(r"--state (\S+)", line)
        message = re.search(r"--message (\S+)", line)
        if state:
            out.append(state.group(1) + (f":{message.group(1)}" if message else ""))
    return out


def _stop_reason(state: str) -> str:
    """What ended the drive, from the last state seen."""
    if state.startswith("idle:"):
        return "turn-" + state.split(":", 1)[1]
    return {"idle": "turn-ended", "blocked": "waiting-for-a-person", "exited": "newt-exited"}.get(
        state.split(":")[0], state
    )


def _probe(instruction: str) -> str:
    """The end of the instruction's last line, whitespace collapsed: the part of
    a long paste still on screen, next to the cursor."""
    last = next((ln for ln in reversed(instruction.splitlines()) if ln.strip()), "")
    return " ".join(last.split())[-16:]


class NewtAgent(BaseInstalledAgent):
    """Drive the newt TUI inside a Harbor task container."""

    def __init__(self, *args, **kwargs):
        # Before Harbor builds the task container, not in install().
        refused = _refusals()
        if refused:
            raise ValueError("newt TUI drive: " + "; ".join(refused))
        super().__init__(*args, **kwargs)

    @staticmethod
    @override
    def name() -> str:
        return "newt"

    @override
    def get_version_command(self) -> str | None:
        return "newt --version"

    @override
    def parse_version(self, stdout: str) -> str:
        # "newt 0.8.5 (bc618a3630e2)" -> "0.8.5 (bc618a3630e2)"
        return stdout.strip().removeprefix("newt").strip()

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        if not _NEWT_PROFILE:
            raise ValueError("NEWT_BENCH_PROFILE must point at a backend profile toml.")
        gguf = sorted(Path(_SUMMARIZER).glob("*.gguf"))
        tokenizer = Path(_SUMMARIZER) / "tokenizer.json"
        if not gguf or not tokenizer.is_file():
            raise ValueError(f"NEWT_BENCH_SUMMARIZER: no GGUF + tokenizer.json in {_SUMMARIZER}")
        await self.exec_as_root(
            environment,
            command="command -v tmux >/dev/null || { export DEBIAN_FRONTEND=noninteractive; "
            "apt-get update -qq && apt-get install -y -qq --no-install-recommends tmux; } >/dev/null 2>&1; "
            "command -v tmux",
        )
        await environment.upload_file(_NEWT_BIN, "/usr/local/bin/newt")
        with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as fh:
            fh.write(_SHIM)
        try:
            await environment.upload_file(fh.name, HERDR_SHIM)
        finally:
            os.unlink(fh.name)
        await self.exec_as_root(environment, command=f"chmod 0755 /usr/local/bin/newt {HERDR_SHIM}")
        probe = await self.exec_as_agent(environment, command='printf %s "$HOME"')
        home = (getattr(probe, "stdout", None) or "").strip() or "/root"
        models = f"{home}/.newt/models/{Path(_SUMMARIZER).name}"
        await self.exec_as_root(environment, command=f"mkdir -p {shlex.quote(models)}")
        await environment.upload_file(_NEWT_PROFILE, f"{home}/.newt/config.toml")
        await environment.upload_file(str(gguf[0]), f"{models}/{gguf[0].name}")
        await environment.upload_file(str(tokenizer), f"{models}/tokenizer.json")
        if _API_KEY_FILE:
            await self.exec_as_root(environment, command="mkdir -p /etc/newt")
            await environment.upload_file(_API_KEY_FILE, "/etc/newt/api-key")
            await self.exec_as_root(environment, command="chmod 0600 /etc/newt/api-key")

    async def _screen(self, environment: BaseEnvironment, history: bool = False) -> str:
        scope = " -S -" if history else ""
        result = await self.exec_as_agent(
            environment, command=_tmux(f"capture-pane -p -J{scope} -t {SESSION}") + " 2>/dev/null; true"
        )
        return getattr(result, "stdout", None) or ""

    async def _type(self, environment: BaseEnvironment, *commands: str) -> None:
        await self.exec_as_agent(environment, command=_tmux(*commands))

    def _log(self, event: str, **detail: object) -> None:
        with open(self.logs_dir / "driver.jsonl", "a") as fh:
            fh.write(json.dumps({"t": round(time.time(), 1), "event": event, **detail}) + "\n")

    def _lifecycle(self) -> list[str]:
        path = self.logs_dir / LIFECYCLE
        text = path.read_text(errors="replace") if path.exists() else ""
        return _states(text[: text.rfind("\n") + 1])  # complete lines only

    def _turn_outcomes(self) -> list[str]:
        """The `end_reason` of each turn newt saved, oldest first, from the copy
        of its own store. Read from a scratch copy, so SQLite replays the WAL."""
        src = self.logs_dir / "newt-home" / "conversations.db"
        if not src.exists():
            return []
        with tempfile.TemporaryDirectory() as tmp:
            for suffix in ("", "-wal"):
                if Path(f"{src}{suffix}").exists():
                    shutil.copy(f"{src}{suffix}", f"{tmp}/c.db{suffix}")
            try:
                db = sqlite3.connect(f"{tmp}/c.db")
                rows = db.execute(
                    "SELECT metadata FROM prompt_artifacts WHERE kind = 'turn_outcome' ORDER BY rowid"
                ).fetchall()
                db.close()
            except sqlite3.Error:
                return []
        return [json.loads(meta or "{}").get("end_reason", "unknown") for (meta,) in rows]

    def _exit_code(self) -> int | None:
        path = self.logs_dir / "newt.exit"
        text = path.read_text().strip() if path.exists() else ""
        return int(text) if text.lstrip("-").isdigit() else None

    async def _wait(self, seen: int, want: set[str], timeout: float) -> tuple[str | None, int]:
        """The first lifecycle state in `want` after the first `seen` reports,
        and how many reports that consumes. `exited` once newt has exited (it
        reports nothing on the way out); `None` on timeout."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            states = self._lifecycle()
            for i in range(seen, len(states)):
                if states[i].split(":")[0] in want:
                    return states[i], i + 1
            if self._exit_code() is not None:
                return "exited", len(states)
            await asyncio.sleep(1)
        return None, seen

    @override
    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as fh:
            fh.write(instruction.rstrip())
            local_instr = fh.name
        try:
            await environment.upload_file(local_instr, "/tmp/newt-task.md")
        finally:
            os.unlink(local_instr)
        # prove-plus-comm works in /workspace, not /app: ask Harbor, then the
        # container, before falling back.
        workdir = (getattr(getattr(environment, "task_env_config", None), "workdir", None) or "").strip()
        if not workdir:
            probe = await self.exec_as_agent(environment, command="pwd")
            workdir = ((getattr(probe, "stdout", None) or "").strip().splitlines() or ["/app"])[0] or "/app"

        meta: dict[str, object] = {"driver": "newt-tui", "argv": _newt_argv(), "workdir": workdir}
        context.metadata = meta  # filled in as the drive goes
        await self.exec_as_agent(environment, command=_start_command(workdir))
        self._log("started", workdir=workdir)
        finished = False
        try:
            state, seen = await self._wait(0, {"idle"}, READY_TIMEOUT_SEC)
            if state != "idle":
                raise RuntimeError(f"newt TUI never reported ready ({state or 'timeout'}):\n"
                                   + (await self._screen(environment))[-2000:])
            self._log("ready")

            await self._type(environment, "load-buffer -b task /tmp/newt-task.md",
                             f"paste-buffer -p -d -b task -t {SESSION}")
            probe = _probe(instruction)
            for _ in range(10):
                if probe in " ".join((await self._screen(environment)).split()):
                    break
                await asyncio.sleep(1)
            else:
                self._log("paste-not-seen", probe=probe)
            await self._type(environment, f"send-keys -t {SESSION} Enter")
            self._log("submitted")

            # None: no start reported. The turn may still have run (see the
            # module docstring); newt's record decides after the exit.
            state, seen = await self._wait(seen, {"working", "blocked"}, START_TIMEOUT_SEC)
            while state is not None and state.startswith("working"):
                self._log("working")
                # Runs until Harbor's agent timeout cancels the wait.
                state, seen = await self._wait(seen, {"idle", "blocked"}, float("inf"))
                if state.startswith("idle"):
                    nxt, after = await self._wait(seen, {"working", "blocked"}, SETTLE_SEC)
                    if nxt is not None:
                        state, seen = nxt, after
            meta["stop"] = _stop_reason(state) if state else "no-start-reported"
            self._log("stopped", stop=meta["stop"])
            finished = True
        finally:
            await self._finish(environment, meta, graceful=finished)
        outcomes = meta["outcomes"]
        if not outcomes and meta["stop"] == "no-start-reported":
            raise RuntimeError("the instruction left no turn in newt's record; see /logs/agent/tui-screen.txt")
        last = outcomes[-1] if outcomes else meta["stop"].removeprefix("turn-")
        if last == "failed":
            raise NonZeroAgentExitCodeError("newt's last turn failed; see /logs/agent/tui-screen.txt")
        if meta["newt_exit"] != 0:
            # tb_campaign reads the status as `exit N` (137 is an infra kill).
            raise NonZeroAgentExitCodeError(f"newt exit {meta['newt_exit']}")

    async def _finish(self, environment: BaseEnvironment, meta: dict[str, object], graceful: bool) -> None:
        """Save the screen, /exit newt (or kill it after a failure, a timeout,
        or an /exit it ignores), and copy its own records to /logs/agent. tmux
        stays up: see `_start_command`."""
        (self.logs_dir / "tui-screen.txt").write_text(await self._screen(environment, history=True))
        if graceful and self._exit_code() is None:
            if meta.get("stop") == "waiting-for-a-person":
                await self._type(environment, f"send-keys -t {SESSION} C-c")
                await asyncio.sleep(1)
            await self._type(environment, "set-buffer -b quit /exit",
                             f"paste-buffer -p -d -b quit -t {SESSION}", f"send-keys -t {SESSION} Enter")
            deadline = time.monotonic() + EXIT_TIMEOUT_SEC
            while self._exit_code() is None and time.monotonic() < deadline:
                await asyncio.sleep(1)
        code = self._exit_code()
        if code is not None:
            meta["newt_exit"] = code
        else:
            meta["newt_exit"] = "killed"
            await self.exec_as_root(environment, command=_KILL_NEWT)
        meta["states"] = self._lifecycle()
        await self.exec_as_agent(
            environment,
            command='mkdir -p /logs/agent/newt-home && cd "$HOME/.newt" && '
            "cp -p conversations.db* history *.jsonl /logs/agent/newt-home/ 2>/dev/null; true",
        )
        meta["outcomes"] = self._turn_outcomes()
        self._log("finished", newt_exit=meta["newt_exit"], outcomes=meta["outcomes"])
