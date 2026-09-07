# newt-mcp-server

Newt-Agent stdio MCP server — `code.read` / `code.edit` / `code.search` /
`goal.run`.

A stdio JSON-RPC MCP server exposing the vi-minimal v0 tool surface:

- `code_read` — read a file
- `code_edit` — apply a unified diff patch
- `code_search` — regex search across a directory tree
- `git` — local embedded Git operations, available only with unrestricted
  filesystem read authority; legacy repository reads cannot yet honor a
  bounded read grant. Operation-class write checks still apply.
- `goal_run` — tier-routed inference (wired through the Router +
  BackendRegistry; discovers a local Ollama on startup)

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## License

Apache-2.0
