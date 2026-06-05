# newt-core

Newt-Agent core types, errors, and the NeMoCode-style tier router.

The router is the NeMoCode inheritance: it classifies an incoming turn into a
`Tier` (FAST / STANDARD / COMPLEX / REVIEW) and asks the configured backends
which can serve that tier. The crate also carries the shared configuration
model (`~/.newt/config.toml`), session and memory types, MCP server
resolution, metrics, and capability caveat extensions used across the
workspace.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
small, fast, local-first agentic coder.

## License

Apache-2.0
