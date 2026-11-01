# newt-mcp-server

Newt-Agent stdio MCP server.

A stdio JSON-RPC MCP server exposing:

- `read_file` / `write_file` / `edit_file` / `list_dir` / `find` / `grep` —
  newt-core's file tools, by the same name, schema and implementation the TUI
  gives the model, under the granted `[caveats]` leash (#2956)
- `git` — local embedded Git operations, available only with unrestricted
  filesystem read authority; legacy repository reads cannot yet honor a
  bounded read grant. Operation-class write checks still apply.
- `shell_run` / `web_fetch` — agent-bridle's confined shell and fetch
- `goal_run` — tier-routed inference (wired through the Router +
  BackendRegistry; discovers a local Ollama on startup)

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## License

Apache-2.0
