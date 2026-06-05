# newt-agent-py

Python bindings for newt-agent.

The umbrella PyO3 extension module: one cdylib stitching together the
`pyo3_module::register` hooks of the underlying crates (core, tools, coder,
eval, inference, acp_worker, mcp) into a single `_newt_agent` module. The
Python import path is `newt_agent`:

```python
from newt_agent.core import Router, Tier
from newt_agent.coder import build_prompt, normalize_emission
```

Distributed on PyPI as `newt-agent-py` (`pip install newt-agent-py`).

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
small, fast, local-first agentic coder.

## License

Apache-2.0
