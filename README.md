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

### From PyPI (recommended)

```bash
pip install newt-agent          # ships the `newt` CLI
pip install newt-mcp-server     # ships just the MCP server binary
```

Both wheels are binary-only — they ship the same release binary the
GitHub release does, packaged so you don't need a Rust toolchain to
install. After install:

```bash
newt --help
newt-mcp-server                  # stdio JSON-RPC MCP server
python -m newt_agent --help      # equivalent to `newt --help`
```

### From source

```bash
cargo install --path newt-cli                 # newt
cargo install --path newt-mcp-server          # newt-mcp-server
```

Or from crates.io once published:

```bash
cargo install newt-agent          # newt
cargo install newt-mcp-server     # newt-mcp-server
```

## Modes

```
newt code [PATH]              # standalone TUI coder
newt pilot <flight-id>        # drake-swarm dashboard
newt worker [--coder]         # ACP worker (stdio JSON-RPC, headless)
newt mcp                      # MCP server (stdio JSON-RPC, headless)
newt doctor                   # health-check local backends + provider plugins
newt config                   # print resolved config
```

### Coder mode

`newt worker --coder` (or `NEWT_CODER=1 newt worker`) activates the
**newt-coder** plugin: tasks are handled by injecting the relevant file
contents into the prompt and asking the model to emit the **complete
updated file**. The plugin parses the reply, writes any whole-file blocks
to the workspace atomically, then captures a real `git diff` so the
foreman gets a hunk-shaped diff to grade.

This closes failure mode **T0b** (model invents file contents) that the
default newt-flat path hits on every local Ollama coder model tested in
the 2026-05-29 bake-off. See
`~/workspaces/knowledge/board/drake/2026-05-29_newt-coder-failure-mode-taxonomy.md`
for the failure-mode taxonomy, the bake-off results, and the design
rationale.

Per-session opt-in (ACP):

```jsonrpc
{ "method": "new_session", "params": { "workspace_path": "/path/to/repo", "coder": true } }
```

Coder-path replies carry an additional `emission_shape` field on
`TaskReply` (`"whole_files"`, `"unified_diff"`, or `"prose"`) so the
foreman's scorecard can distinguish T0a / T0b / T0c instead of lumping
them as "empty diff."

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

## Evaluation

The [`newt-eval`](./newt-eval/) crate is the end-to-end scorecard for
the worker. It spawns the real `newt worker` binary, drives ACP against
a mock or real Ollama, then grades the captured diff with five
evaluators (`diff_nonempty`, `diff_applies`, `rust_compiles`,
`tests_pass`, `pattern_match`).

```bash
cargo test -p newt-eval --test mock_e2e   # CI gate (mock Ollama)
just eval                                 # live mode (real Ollama)
```

See [`newt-eval/README.md`](./newt-eval/README.md) for how to add a
new case.

## Status

v0.x — workspace scaffold landed; building toward v0.1 (`newt worker` +
`LocalOllamaBackend` end-to-end).

The work is broken into ~33 drake-flight-sized steps in
[`docs/ROADMAP.md`](./docs/ROADMAP.md). Each step is one PR, fully tested,
≥80% coverage. See the working design at
`~/.claude/plans/flickering-fluttering-otter.md` (internal).

## License

Apache-2.0. See [LICENSE](./LICENSE).
