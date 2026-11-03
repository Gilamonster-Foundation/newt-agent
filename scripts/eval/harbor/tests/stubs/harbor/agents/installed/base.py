# TEST-ONLY STUB. The adapter (scripts/eval/harbor/newt_agent.py) imports these names from the real
# `harbor` package, which CI does not install. Just enough is defined here for
# tests/test_lanes.py to import the module and drive it against a fake environment.
# Never put this directory on PYTHONPATH for a real Harbor run.


class NonZeroAgentExitCodeError(RuntimeError):
    """Stand-in for Harbor's: Harbor records it and still grades the trial."""


class BaseInstalledAgent:
    """Stand-in base class: the logs dir and the two exec helpers the adapter uses."""

    def __init__(self, logs_dir):
        self.logs_dir = logs_dir

    async def exec_as_agent(self, environment, command, **kwargs):
        return await environment.exec(command, **kwargs)

    async def exec_as_root(self, environment, command, **kwargs):
        return await environment.exec(command, user="root", **kwargs)


def with_prompt_template(fn):
    """Stand-in decorator: identity."""
    return fn
