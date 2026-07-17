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

Interactive front ends may inject the public `LiveToolOutput` interface into
`ChatCtx` to observe streaming shell bytes without changing the authoritative
tool result. Newt dispatches those bytes through a bounded presentation queue,
contains observer panics, and runs renderer startup off the tool-execution
task. Normal completion and responsive cancellation close the generation
before the canonical completed result is rendered. A bounded teardown timeout
instead calls the sink's no-output `abandon` transition synchronously,
invalidating delayed callbacks before canonical rendering resumes. Headless
callers set `live_tool_output` to `None`, preserving completion-only output.
Each tool invocation gets a new generation, so late bytes from an ended or
retried invocation are ignored. The front end owns display sanitization and
retained-history policy; neither can mutate the model-facing result.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## License

Apache-2.0
