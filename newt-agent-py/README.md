# newt-agent-py

Python bindings for newt-agent.

The umbrella PyO3 extension module: one cdylib stitching together the
`pyo3_module::register` hooks of the underlying crates (core, tools, coder,
eval, inference, acp_worker, mcp, frame, harness) into a single `_newt_agent` module. The
Python import path is `newt_agent`:

```python
from newt_agent.core import Router, Tier
from newt_agent.coder import build_prompt, normalize_emission
from newt_agent.harness import Session
```

`frame` supplies admitted derivation and causal-event bindings; `harness.Session`
supplies durable recording, bounded projection/retrieval, and exact request
replay. Foreign extensions can reuse these two submodules through
[`agent-harness-py`](../agent-harness-py/README.md)'s registration hook without
linking this umbrella, newt-core, or the terminal UI. Its independent consumer
fixture builds and imports a real extension to verify that dependency boundary.

Distributed on PyPI as `newt-agent-py` (`pip install newt-agent-py`).

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## License

Apache-2.0
