# newt-git

An embedded, pure-Rust Git engine for Newt. It wraps
[`grit-lib`](https://crates.io/crates/grit-lib) behind the `GitCaveats`
capability surface from [`agent-toolchain`](../agent-toolchain/README.md).
Every read or write takes the caller's caveats and fails closed without them.

**Where it sits.** A backend for the harness's `GitTool`, used by `newt-cli`,
`newt-tui` and `newt-mcp-server`. In the TUI and headless loops the model is
not shown a `git` tool: commits go through ordinary `git` in `run_command`,
and the native commit broker stamps attribution. `newt-mcp-server` still
exposes Git operations to MCP clients. It is a separate crate to keep
`grit-lib` and its heavy dependencies out of `newt-core`; `newt-cli` still
links it.

**Direction.** `agent-toolchain` with real, digest-pinned Git is intended to
replace this engine. New Git behaviour belongs on that path, not here.

Behaviour, limits and grant rules: [docs/design/newt-git.md](../docs/design/newt-git.md).

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent).
