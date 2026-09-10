# agent-harness

Host-controlled context projection, bounded navigation, and recorded agent
sessions over `agent-frame`. Models propose relevance and classifications;
the host validates them before changing a projection or delivering a reply.

This library has no inference, HTTP, terminal, or async runtime dependency.
Consumers supply model calls and authorization. `agent-harness-py` exposes
the same operations through PyO3 for other Python hosts.

License: Apache-2.0.
