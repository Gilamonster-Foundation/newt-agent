"""newt_agent.py — Harbor installed-agent adapter for ``newt solve`` (WS3, #1419).

The bridge that lets Terminal-Bench (Harbor) drive newt on real, containerized
tasks — the entry to the release-champion ceremony's reproducible lane.

It injects the locally-built ``newt`` binary + a pinned backend profile into each
task container, then drives ``newt solve`` headless (``--non-interactive`` =
--yolo --full-access) in ``/app`` and writes the trace to ``/logs/agent``.
Inference reaches the pinned endpoint from *inside* the container (verified
reachable). The binary is *injected* rather than package-installed because newt
has no public crates/npm release yet — that is the only reason this isn't a
stock ``harbor adapter``.

Run it:

    NEWT_BENCH_BIN=~/bin/newt \\
    NEWT_BENCH_PROFILE=/path/to/bench.toml \\
    NEWT_BENCH_TENACITY=insistent \\
    PYTHONPATH=scripts/eval/harbor \\
    harbor run -a newt_agent:NewtAgent -m newt/qwen3-coder_30b <task-or-dataset...>

The backend (endpoint + model) is pinned by NEWT_BENCH_PROFILE (host-secret,
local); ``-m`` is required by Harbor but the profile is authoritative here.
"""

from __future__ import annotations

import os
import shlex
import tempfile
from typing import override

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

# The locally-built binary + the pinned backend profile to inject. Host-secret:
# the endpoint lives only in the profile toml (RATCHET.md invariant).
_NEWT_BIN = os.environ.get("NEWT_BENCH_BIN", os.path.expanduser("~/bin/newt"))
_NEWT_PROFILE = os.environ.get("NEWT_BENCH_PROFILE", "")
# Optional tenacity dial (relaxed|standard|insistent|relentless) and round cap.
_TENACITY = os.environ.get("NEWT_BENCH_TENACITY", "")
_MAX_ROUNDS = os.environ.get("NEWT_BENCH_MAX_ROUNDS", "40")
# The served model's FULL context window (dgx1 serves qwen3-coder at
# --ctx-size 65536 as of 2026-07-28; the router's global `-c` is the ctx knob —
# per-model preset `ctx-size` is ignored in that build). newt reserves ~20% of
# this for the reply, so the input budget becomes 0.8x — leaving KV headroom so
# generation doesn't overrun the window. Set this to match whatever `--ctx-size`
# the endpoint actually serves. Overrideable; empty disables the pin.
_CONTEXT_WINDOW = os.environ.get("NEWT_BENCH_CONTEXT_WINDOW", "65536")


class NewtAgent(BaseInstalledAgent):
    """Drive ``newt solve`` inside a Harbor task container."""

    @staticmethod
    @override
    def name() -> str:
        return "newt"

    @override
    def get_version_command(self) -> str | None:
        return "newt --version"

    @override
    def parse_version(self, stdout: str) -> str:
        # e.g. "newt 0.7.5"
        return stdout.strip().split()[-1] if stdout.strip() else stdout

    @override
    async def install(self, environment: BaseEnvironment) -> None:
        if not _NEWT_PROFILE:
            raise ValueError(
                "NEWT_BENCH_PROFILE must point at a backend profile toml "
                "(the pinned endpoint + model)."
            )
        # Inject the binary + profile; the container reaches the endpoint over
        # the network (verified). No package manager needed.
        await environment.upload_file(_NEWT_BIN, "/usr/local/bin/newt")
        await self.exec_as_root(environment, command="chmod 0755 /usr/local/bin/newt")
        await self.exec_as_root(environment, command="mkdir -p /etc/newt")
        await environment.upload_file(_NEWT_PROFILE, "/etc/newt/bench.toml")

    @override
    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        # Land the instruction inside the container, then solve headless.
        with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as fh:
            fh.write(instruction)
            local_instr = fh.name
        try:
            await environment.upload_file(local_instr, "/tmp/newt-task.md")
        finally:
            os.unlink(local_instr)

        tenacity = f" --tenacity {shlex.quote(_TENACITY)}" if _TENACITY else ""
        ctx = f" --context-window {shlex.quote(_CONTEXT_WINDOW)}" if _CONTEXT_WINDOW else ""
        # --non-interactive defaults true in `newt solve`, so it's omitted here
        # (passing it bare requires a value under the current arg definition).
        command = (
            "mkdir -p /logs/agent; "
            "newt solve --cwd /app "
            "--instruction-file /tmp/newt-task.md "
            "--config /etc/newt/bench.toml "
            "--events /logs/agent/newt-events.jsonl "
            f"--max-rounds {shlex.quote(_MAX_ROUNDS)}{tenacity}{ctx}"
        )
        await self.exec_as_agent(environment, command=command)
