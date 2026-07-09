# newt-core

Newt-Agent core types, errors, and the NeMoCode-style tier router.

The router is the NeMoCode inheritance: it classifies an incoming turn into a
`Tier` (FAST / STANDARD / COMPLEX / REVIEW) and asks the configured backends
which can serve that tier. The crate also carries the shared configuration
model (`~/.newt/config.toml`), session and memory types, MCP server
resolution, metrics, and capability caveat extensions used across the
workspace.

It also hosts the shared agentic tool executor used by the TUI and headless
paths. Built-in file tools include `read_file`, `write_file`, `edit_file`,
`delete_file`, `list_dir`, and `find`, all mediated by the same caveat and
prompted-permission checks.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
small, fast, local-first agentic coder.

## License

Apache-2.0
