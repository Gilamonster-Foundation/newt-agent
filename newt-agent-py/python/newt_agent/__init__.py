"""newt-agent: small, fast, local-first agentic coder.

Submodules
----------
- ``newt_agent.core``        Tier router, config, session/model ids, errors
- ``newt_agent.data``        load_csv_to_sqlite / query / summarize (SQLite EDA)
- ``newt_agent.tools``       read / edit / search / apply_patch / apply_whole_files
- ``newt_agent.coder``       prompt builder + emission normalizer
- ``newt_agent.eval``        TestCase + evaluators + scorecard
- ``newt_agent.inference``   ChatRequest/ChatReply + LocalOllamaBackend (async)
- ``newt_agent.acp_worker``  TaskReply + Session data types
- ``newt_agent.mcp``         McpServer dispatch shell + handler registry

Install: ``pip install newt-agent-py``

The PyPI distribution name has a ``-py`` suffix because PyPI's similarity
check may block the bare ``newt-agent`` against the existing ``newt``
package. The Python import path is unaffected — it stays
``newt_agent``.

A short library example::

    from newt_agent.core import Router, Tier

    router = Router()
    print(router.classify("rename foo to bar"))   # Tier.Fast

    detailed = router.classify_detailed("review this PR")
    print(detailed.tier, detailed.confidence, detailed.reasons)

For the ``newt`` CLI binary, install separately::

    git clone https://github.com/Gilamonster-Foundation/newt-agent
    cargo install --path newt-agent/newt-cli
    newt --help

(A ``pip``-installable Python CLI script is planned as a follow-up.)
"""

from __future__ import annotations

import sys as _sys

# The native PyO3 extension module ships as `_newt_agent` next to this
# package. It registers the submodules below as attributes of the
# parent module.
from . import _newt_agent as _native  # type: ignore[attr-defined]

# Re-export the native submodules as plain Python attributes and stitch
# them into ``sys.modules`` so ``import newt_agent.core`` works (Python's
# import system needs the submodule entries to resolve dotted imports).
core = _native.core
data = _native.data
tools = _native.tools
coder = _native.coder
eval = _native.eval  # noqa: A001 — shadows builtin `eval`; deliberate parity
inference = _native.inference
acp_worker = _native.acp_worker
mcp = _native.mcp

_sys.modules["newt_agent.core"] = core
_sys.modules["newt_agent.data"] = data
_sys.modules["newt_agent.tools"] = tools
_sys.modules["newt_agent.coder"] = coder
_sys.modules["newt_agent.eval"] = eval
_sys.modules["newt_agent.inference"] = inference
_sys.modules["newt_agent.acp_worker"] = acp_worker
_sys.modules["newt_agent.mcp"] = mcp

__all__ = [
    "core",
    "data",
    "tools",
    "coder",
    "eval",
    "inference",
    "acp_worker",
    "mcp",
]
