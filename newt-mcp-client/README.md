# newt-mcp-client

Newt-Agent MCP client — connect to discovered MCP servers over stdio or
streamable HTTP and aggregate their tools.

Connects to the MCP servers resolved by `newt_core::mcp` and reads their tool
lists. Stdio JSON-RPC 2.0 (newline-delimited) and streamable HTTP share a
`Transport` seam; legacy SSE-only servers are unsupported. Tools from different
servers are namespaced `server__tool` so two servers exposing the same tool name
do not collide. The protocol logic (`McpConnection`) is generic over `Transport`
and unit-tested against an in-memory mock — no subprocess needed.

Part of [Newt-Agent](https://github.com/Gilamonster-Foundation/newt-agent), a
free, friendly, local agentic coder.

## Durable HTTP network grants

Grant exact connector and authentication hostnames in a trusted config selected
with `--config`, or in the operator-owned user configuration:

```toml
[tui.permissions]
preset = "workspace_dev"
net = ["mcp.corp.example", "login.corp.example"]
mcp_net_prompt_default = "allow_once"
```

The same saved names work in an OCAP-confined session, with `--full-access`,
and with `preset = "full_access"`. A wildcard can coexist with exact names:
`net = ["*", "mcp.corp.example"]` retains the private-host approval for
`mcp.corp.example`. `"*"` alone does not approve private destinations.
Exact approvals remain bounded by the effective session capability; DNS
pinning, TLS verification, redirect checks, and metadata-address blocking
still apply during connection, OAuth, refresh, and reconnect.

Interactive sessions prompt for a missing MCP hostname grant at startup by
default. Choose **allow once** for the current server connection, **session
allow** to also permit reconnecting and other configured servers on that host,
or **Allow permanently (adds host to config)** to save the exact hostname for
future launches. **allow once** is selected by default; press Enter to confirm.
Set `mcp_net_prompt_default` to `allow_once`, `allow_session`, `allow_permanent`,
`deny`, `deny_always`, or `deny_permanent` to choose a different default. The
selected choice is marked `(default)`; configuring it grants nothing until
you answer. Other permission prompts retain their existing defaults, and
web-shared decisions still ignore Enter until you explicitly select an action.
Session and permanent choices
reuse the permission gate's cache across servers sharing a hostname.
In the rich terminal, the request appears in a bordered modal with the server,
hostname, and reason. The dialog covers the normal prompt and owns input until
you answer or cancel; your draft returns afterward.
`--no-prompt-for-permissions` and headless runs deny missing grants without
opening a prompt. A connection approval does not approve unrelated tool
operations; authentication hostnames still need explicit grants.

Permanent approval writes to the config pinned by `--config`, otherwise to
the operator-owned user config when no ambient `newt.toml` shadows it. If a
project config shadows the user config, the grant is session-only with a
message directing you to select a trusted config using `--config`. A write
failure also reports that approval is session-only.

`newt mcp import --grant-net` also persists the selected connector's exact
hostname with its registration. Add any required authentication hostnames
explicitly. Network grants and login are separate: use `newt auth` to inspect
registration/token status, then `newt auth <server>` to authenticate.

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

Model: GPT-6 | Harness: Codex | Operator: S Hartsock | Time: 17:30 EDT | Date: 2026-09-15
