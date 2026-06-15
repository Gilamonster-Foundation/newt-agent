# MCP transport security — secure-by-default, warn-always

**Status:** policy · **Applies to:** outbound HTTP MCP connections (`type = "http"`) ·
**Implemented in:** `newt-tui/src/mcp.rs` (`apply_transport_security`), `[tui].mcp_allow_insecure_hosts`

## The rule

A connection that carries a credential must be encrypted. newt sends a stored
OAuth **Bearer token** (from `~/.hermes/mcp-tokens/`) to HTTP MCP servers; over
plaintext `http://` that token — a shared, refreshable credential with the
server's session authority — would be exfiltrable to any on-path observer, with
no user-visible signal. So:

1. **Prefer `https://`. Avoid `http://`.** Configure MCP server URLs with TLS.
2. **The Bearer is injected only when the transport is trusted:**
   - `https://` — always; or
   - **loopback** (`localhost` / `127.0.0.1` / `::1`) — the dev exception; or
   - a non-loopback `http://` host that is **explicitly allow-listed** (below).
   Anything else (plain `http://` to a real host, or an unparseable URL) →
   the token is **withheld** (fail safe). An operator-configured explicit
   `Authorization` header is always respected and never overridden.
3. **Warn on every unencrypted connection.** Any non-loopback `http://` MCP
   connection emits a `WARN`, whether or not a token is involved — so an
   unencrypted link is never silent. Loopback is the only exception (no warning).

## The opt-out (explicit, per-host)

When you genuinely must talk to a non-TLS MCP host on a trusted network and want
the Bearer sent anyway, list the host:

```toml
[tui]
mcp_allow_insecure_hosts = ["REDACTED-IP", "mcp.internal.lan"]
```

An allow-listed host **still warns** ("sending the OAuth Bearer anyway") — the
opt-in suppresses the *withholding*, not the *warning*. Matching is by host
(IP or hostname), case-insensitive, port-independent. Empty by default.

## Why warn even when allow-listed

The allow-list is an escape hatch for a constrained environment, not a blessing
of the practice. Keeping the warning means an unencrypted credential path is
always visible in the logs — there is no configuration under which newt silently
ships a token in cleartext. "No https" is a warning by default, full stop.

## Scope / non-goals

- This governs the **Bearer-over-transport** decision and the unencrypted-connection
  warning. It does not (yet) pin certificates, enforce a minimum TLS version, or
  validate the OAuth server's metadata TLS — those are the TLS stack's concern.
- stdio MCP servers are local-process and out of scope.
- IPv6 loopback is recognized as `::1` (bracketed `[::1]` in URLs); exotic URL
  forms that don't parse fall through to *withhold* (the safe default).
