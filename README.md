# Newt-Agent

<p align="center">
  <img src="docs/logos/newt-agent-logo_source.png" alt="Newt-Agent logo" width="256" />
</p>

> Small, fast, local-first agentic coder.
> **vi to Hermes-Agent's emacs.**

Newt-Agent is a single Rust binary with a sharp, minimal tool set. It runs
locally against your NVIDIA hardware by default — no cloud bytes leave your
machine unless you deliberately install a provider plugin.

Newt is the rewrite of NeMoCode and the successor to drake-agent. It carries
NeMoCode's tier-based router (FAST / STANDARD / COMPLEX / REVIEW) and shares
the Rust primitives that power [Hermes-Thoon](https://github.com/Gilamonster-Foundation/hermes-thoon),
but stops there: Newt is opinionated, not extensible.

## Install

```bash
cargo install newt-agent          # crates.io
pip install newt-agent            # PyPI (binary wheel, same binary)
```

## Modes

```
newt code [PATH]              # standalone TUI coder
newt pilot <flight-id>        # drake-swarm dashboard
newt worker                   # ACP worker (stdio JSON-RPC, headless)
newt mcp                      # MCP server (stdio JSON-RPC, headless)
newt doctor                   # health-check local backends + provider plugins
newt config                   # print resolved config
```

## Inference, by default, is local

The default binary speaks only to local backends:

- **Ollama** — `ollama-proxy.inference.svc.cluster.local:11434` (in-cluster)
  with `REDACTED-HOST` / `REDACTED-HOST` / `REDACTED-HOST`
  fallbacks.
- **vLLM** — local OpenAI-compatible HTTP for DGX-served models.

Cloud APIs (OpenAI, Anthropic) require **opt-in provider plugins** installed
separately:

```bash
pip install newt-provider-openai      # registers an opt-in provider
pip install newt-provider-anthropic   # registers an opt-in provider
```

Provider plugins run as subprocesses and speak the Newt-Provider JSON-RPC
schema in [`plugins-protocol/`](./plugins-protocol/). No cloud client code is
compiled into the default Newt binary — the opt-in is enforced at the build
level, not by a runtime feature flag.

## Status

v0.x — workspace scaffold landed; building toward v0.1 (`newt worker` +
`LocalOllamaBackend` end-to-end).

The work is broken into ~33 drake-flight-sized steps in
[`docs/ROADMAP.md`](./docs/ROADMAP.md). Each step is one PR, fully tested,
≥80% coverage. See the working design at
`~/.claude/plans/flickering-fluttering-otter.md` (internal).

## License

Apache-2.0. See [LICENSE](./LICENSE).
