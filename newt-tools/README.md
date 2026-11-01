# newt-tools

`apply_patch` and `apply_whole_files`: the landing pads for a model's
unified-diff or whole-file emission, with pluggable appliers (fuzzy by
default, `diffy` behind a feature).

**Where it sits.** Used by `newt-coder`, `newt-acp-worker` and the Python
module. The file tools a model calls by name (`read_file`, `edit_file`,
`list_dir`, `grep`, …) are newt-core's, in `newt-core/src/agentic/tools/`,
and the MCP server serves those same ones
([#2956](https://github.com/Gilamonster-Foundation/newt-agent/issues/2956)).

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent).

## License

Apache-2.0
