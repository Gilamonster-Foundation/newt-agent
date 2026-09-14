"""codex_local.py — Harbor's Codex agent with inference routed to a local endpoint (#2318).

Harbor's built-in ``codex`` authenticates against OpenAI. This subclass seeds
``$CODEX_HOME/config.toml`` inside the task container with a
``model_providers.local`` entry and selects it, then runs the stock agent
unchanged: Harbor appends its own auth and MCP lines to the same file and keeps
its command line, session copy and ATIF trajectory.

    TB_LOCAL_BASE_URL=http://<endpoint>/v1 \\
    TB_LOCAL_CONTEXT_WINDOW=131072 \\
    PYTHONPATH=scripts/eval/harbor \\
    harbor run ... --agent-import-path codex_local:CodexLocal -m local/<model-as-served>

The endpoint is read from the harbor process's environment, never a literal and
never Harbor's ``--ae`` (that is recorded in the job config). Harbor passes the
part of ``-m`` after the last ``/`` as ``--model``.
"""

from __future__ import annotations

import json
import os
import shlex
from typing import override

from harbor.agents.installed.codex import Codex
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

PROVIDER = "local"


def provider_toml(base_url: str, context_window: str) -> str:
    """Top-level keys first: Harbor appends further lines after this block."""
    if not base_url:
        raise ValueError("TB_LOCAL_BASE_URL must name the OpenAI-compatible endpoint")
    window = (
        f"model_context_window = {int(context_window)}\n" if context_window.strip() else ""
    )
    return (
        f'model_provider = "{PROVIDER}"\n{window}'
        f"[model_providers.{PROVIDER}]\n"
        f'name = "{PROVIDER}"\n'
        f"base_url = {json.dumps(base_url)}\n"
        'wire_api = "responses"\n'
        "requires_openai_auth = false\n"
        "supports_websockets = false\n"
    )


class CodexLocal(Codex):
    @override
    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        config = provider_toml(
            os.environ.get("TB_LOCAL_BASE_URL", ""),
            os.environ.get("TB_LOCAL_CONTEXT_WINDOW", ""),
        )
        home = shlex.quote(self._REMOTE_CODEX_HOME.as_posix())
        await self.exec_as_agent(
            environment,
            command=f'mkdir -p {home} && printf %s "$CODEX_PROVIDER_TOML" > {home}/config.toml',
            env={"CODEX_PROVIDER_TOML": config},
        )
        await super().run(instruction, environment, context)
