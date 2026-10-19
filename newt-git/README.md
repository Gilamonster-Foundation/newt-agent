# newt-git

An embedded, pure-Rust Git engine for Newt. It wraps
[`grit-lib`](https://crates.io/crates/grit-lib) behind the `GitCaveats`
capability surface from [`agent-toolchain`](../agent-toolchain/README.md).
Every read or write takes the caller's caveats and fails closed without them.

**Where it sits.** An optional backend for the harness's `GitTool`, used by
`newt-cli`, `newt-tui` and `newt-mcp-server`. The model is never shown a `git`
tool: commits go through ordinary `git` in `run_command`, and the native
commit broker stamps attribution. It is a separate crate to keep `grit-lib` and
its heavy dependencies out of `newt-core` and the lean build.

**Direction.** `agent-toolchain` with real, digest-pinned Git is intended to
replace this engine. New Git behaviour belongs on that path, not here.

Behaviour, limits and grant rules: [docs/design/newt-git.md](../docs/design/newt-git.md).

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent).
