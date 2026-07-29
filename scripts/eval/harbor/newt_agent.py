"""newt_agent.py — Harbor installed-agent adapter for ``newt solve`` (WS3, #1419).

The bridge that lets Terminal-Bench (Harbor) drive newt on real, containerized
tasks — the entry to the release-champion ceremony's reproducible lane.

It injects the locally-built ``newt`` binary + a pinned backend profile into each
task container, then drives ``newt solve`` headless in ``/app`` and writes the
trace to ``/logs/agent``. Two lanes, chosen by ``NEWT_BENCH_OCAP``:

- unset / off — the default ``--non-interactive`` (``--yolo --full-access``)
  lane: OCAP off, host shell, no prompts. The lane the published floor was set
  on (deliberate variable isolation).
- ``on`` — the ``--confined`` lane: OCAP stays ON, writes fenced to the
  workspace + the container's mutable roots, reads/exec/net open. The 0.7.6
  parity gate runs the SAME suite once per lane and requires the scores match.

Inference reaches the pinned endpoint from *inside* the container (verified
reachable). The binary is *injected* rather than package-installed because newt
has no public crates/npm release yet — that is the only reason this isn't a
stock ``harbor adapter``.

Run it:

    NEWT_BENCH_BIN=~/bin/newt \\
    NEWT_BENCH_PROFILE=/path/to/bench.toml \\
    NEWT_BENCH_TENACITY=insistent \\
    NEWT_BENCH_OCAP=on \\        # omit / off for the --yolo lane
    NEWT_BENCH_SELF_VERIFY=1 \\  # run the workspace's own checks before RTB (#1 lever)
    PYTHONPATH=scripts/eval/harbor \\
    harbor run -a newt_agent:NewtAgent -m newt/qwen3-coder_30b <task-or-dataset...>

Robustness/capability knobs injected INTO the container (the harbor process's env
does not cross the container boundary): NEWT_BENCH_HTTP_RETRIES (default 10 — a
more patient retry window so a transient router drop doesn't zero a task on a
pure infra fault) and NEWT_BENCH_SELF_VERIFY (opt-in self-verify gate).

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
# OCAP lane: ``on`` runs the confined lane (OCAP enabled, writes fenced to the
# workspace + the container's mutable roots, reads/exec/net open) instead of the
# default ``--yolo`` full-access lane. This is the variable the 0.7.6 parity gate
# flips: the SAME suite is run once per model with NEWT_BENCH_OCAP unset (off)
# and once with ``on``, and the two scores must match before 0.7.6 tags.
_OCAP = os.environ.get("NEWT_BENCH_OCAP", "")
# Inference-robustness knob. The local llama.cpp router drops connections under
# memory pressure (co-hosted vLLM near the 121G ceiling), and a task that
# exhausts newt's retry window scores 0 on a pure infra fault — not the agent.
# Bench runs get a MORE patient retry budget than the interactive default so a
# brief router restart doesn't zero a task. `newt`'s `RetryPolicy::from_env`
# reads these; overridable, empty keeps newt's own default.
_HTTP_RETRIES = os.environ.get("NEWT_BENCH_HTTP_RETRIES", "10")
# Self-verify gate (the measured #1 capability lever): make the agent RUN the
# workspace's own checks before declaring done. Opt-in per run — set
# NEWT_BENCH_SELF_VERIFY=1 to inject NEWT_SELF_VERIFY=1 into the container.
_SELF_VERIFY = os.environ.get("NEWT_BENCH_SELF_VERIFY", "")


def _container_env_prefix() -> str:
    """Env vars exported INSIDE the task container ahead of `newt solve` — the
    harbor process's own env does NOT cross into the container, so anything newt
    must read (retry budget, self-verify) is injected here as a `K=V ` prefix."""
    parts = []
    if _HTTP_RETRIES.strip():
        # More patient retry window: 10 retries over the 2s→30s backoff rides a
        # ~5 min router blip instead of the default ~90s.
        parts.append(f"NEWT_HTTP_MAX_RETRIES={shlex.quote(_HTTP_RETRIES)}")
        parts.append("NEWT_HTTP_BACKOFF_BASE_MS=2000")
        parts.append("NEWT_HTTP_BACKOFF_MAX_MS=30000")
    if _SELF_VERIFY.strip().lower() in ("1", "true", "on", "yes"):
        parts.append("NEWT_SELF_VERIFY=1")
    return (" ".join(parts) + " ") if parts else ""


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
        # OCAP-on lane: append --confined. Off (empty/anything else) keeps the
        # default --yolo full-access lane. The flag flips OCAP on AND seeds the
        # workspace-fenced caveat inside `newt solve`.
        confined = " --confined" if _OCAP.strip().lower() == "on" else ""
        # --non-interactive defaults true in `newt solve`, so it's omitted here
        # (passing it bare requires a value under the current arg definition).
        command = (
            "mkdir -p /logs/agent; "
            f"{_container_env_prefix()}newt solve --cwd /app "
            "--instruction-file /tmp/newt-task.md "
            "--config /etc/newt/bench.toml "
            "--events /logs/agent/newt-events.jsonl "
            f"--max-rounds {shlex.quote(_MAX_ROUNDS)}{tenacity}{ctx}{confined}"
        )
        await self.exec_as_agent(environment, command=command)
