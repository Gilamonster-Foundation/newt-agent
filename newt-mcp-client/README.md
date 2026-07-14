# newt-mcp-client

Newt-Agent MCP client — connect to discovered MCP servers (stdio JSON-RPC)
and aggregate their tools.

Connects to the MCP servers resolved by `newt_core::mcp` and reads their tool
lists. Speaks stdio JSON-RPC 2.0 (newline-delimited) behind a `Transport`
seam so SSE/HTTP can follow without touching the protocol logic. Tools from
different servers are namespaced `server__tool` so two servers exposing the
same tool name do not collide. The protocol logic (`McpConnection`) is
generic over `Transport` and unit-tested against an in-memory mock — no
subprocess needed.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## Per-server request timeout

Each `tools/call` is bounded by a per-request timeout so a wedged server
cannot hang the agent — `DEFAULT_REQUEST_TIMEOUT` (20s) unless the server
entry overrides it. Raise it for a server whose tools legitimately run long
(e.g. a routine engine that fans out across many repos and live APIs in one
call) via `request_timeout_secs` on the entry — `requestTimeoutSecs` in
Claude-format JSON:

```toml
# newt TOML — a [[mcp_servers]] entry
[[mcp_servers]]
name = "modulex"
command = "modulex-mcp"
request_timeout_secs = 180
```

```json
// Claude-format .mcp.json
{ "mcpServers": { "modulex": { "command": "modulex-mcp", "requestTimeoutSecs": 180 } } }
```

The resolved value is clamped to `[1s, MAX_REQUEST_TIMEOUT]` (600s), so even a
patient server still gives up on a genuinely wedged call.

## License

Apache-2.0
