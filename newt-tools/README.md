# newt-tools

A file and code tool set: `read`, `list_dir`, `search`, `edit`, and
`apply_patch` with pluggable appliers (fuzzy by default, `diffy` behind a
feature).

**Where it sits.** Used by `newt-mcp-server`, `newt-acp-worker`, `newt-coder`
and the Python module. The tools the model gets in the TUI and headless are a
separate implementation in `newt-core/src/agentic/tools/`. Folding the two into
one is tracked in
[#2956](https://github.com/Gilamonster-Foundation/newt-agent/issues/2956).

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent).

## License

Apache-2.0
