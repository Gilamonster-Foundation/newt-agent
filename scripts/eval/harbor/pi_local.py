"""pi_local.py — Harbor's Pi agent with inference routed to a local endpoint (#2318).

Harbor's built-in ``pi`` only knows hosted providers. This subclass writes a
``~/.pi/agent/models.json`` provider entry inside the task container and then
runs the stock agent unchanged, so the command line, the JSON session log and
Harbor's token accounting are exactly Harbor's.

    TB_LOCAL_BASE_URL=http://<endpoint>/v1 \\
    TB_LOCAL_CONTEXT_WINDOW=131072 \\
    PYTHONPATH=scripts/eval/harbor \\
    harbor run -a ... --agent-import-path pi_local:PiLocal -m local/<model-as-served>

The endpoint is read from the harbor process's environment, never a literal and
never Harbor's ``--ae`` (that is recorded in the job config). The model is the
part of ``-m`` after ``local/``, so the label Harbor records is the model sent.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import override

from harbor.agents.installed.base import NonZeroAgentExitCodeError
from harbor.agents.installed.node_install import nvm_node_install_snippet
from harbor.agents.installed.pi import Pi
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from pi_log import inference_failure

PROVIDER = "local"
# Harbor 0.20 installs @mariozechner/pi-coding-agent, deprecated upstream in
# favour of this package; the deprecated name is frozen at 0.73.1.
PACKAGE = "@earendil-works/pi-coding-agent"


def models_json(base_url: str, model: str, context_window: str) -> str:
    """The pi provider entry for one locally served model."""
    if not base_url:
        raise ValueError("TB_LOCAL_BASE_URL must name the OpenAI-compatible endpoint")
    if not context_window.strip():
        # Unset, pi assumes 128000 whatever the server actually serves.
        raise ValueError("TB_LOCAL_CONTEXT_WINDOW must be the ctx-size as served")
    entry = {"id": model, "contextWindow": int(context_window)}
    return json.dumps(
        {
            "providers": {
                PROVIDER: {
                    "baseUrl": base_url,
                    "api": "openai-completions",
                    # pi lists a model only once it has a key; the server ignores it.
                    "apiKey": PROVIDER,
                    "models": [entry],
                }
            }
        }
    )


def raise_on_inference_failure(log: Path) -> None:
    """pi exits 0 when every model call failed; turn that into an agent error
    so Harbor does not grade an untouched workspace as the model's work."""
    lines = log.read_text(errors="replace").splitlines() if log.exists() else []
    if failure := inference_failure(lines):
        raise NonZeroAgentExitCodeError(f"pi exited 0 without a usable model reply ({failure})")


class PiLocal(Pi):
    @override
    async def install(self, environment: BaseEnvironment) -> None:
        await self.exec_as_root(
            environment,
            command="apt-get update && apt-get install -y curl",
            env={"DEBIAN_FRONTEND": "noninteractive"},
        )
        spec = f"@{self._version}" if self._version else "@latest"
        await self.exec_as_agent(
            environment,
            command=(
                f"set -euo pipefail; {nvm_node_install_snippet()} && "
                f"npm install -g {PACKAGE}{spec} && pi --version"
            ),
        )

    @override
    async def run(
        self, instruction: str, environment: BaseEnvironment, context: AgentContext
    ) -> None:
        provider, _, model = (self.model_name or "").partition("/")
        if provider != PROVIDER or not model:
            raise ValueError(f"model must be {PROVIDER}/<model-as-served>")
        config = models_json(
            os.environ.get("TB_LOCAL_BASE_URL", ""),
            model,
            os.environ.get("TB_LOCAL_CONTEXT_WINDOW", ""),
        )
        # Passed as env, not spliced into the command, so no quoting can break it.
        await self.exec_as_agent(
            environment,
            command='mkdir -p ~/.pi/agent && printf %s "$PI_MODELS_JSON" > ~/.pi/agent/models.json',
            env={"PI_MODELS_JSON": config},
        )
        await super().run(instruction, environment, context)
        raise_on_inference_failure(self.logs_dir / self._OUTPUT_FILENAME)
