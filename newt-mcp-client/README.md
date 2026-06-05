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
small, fast, local-first agentic coder.

## License

Apache-2.0
