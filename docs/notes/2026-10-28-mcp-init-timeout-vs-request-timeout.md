# MCP init timeout vs. request timeout: one knob for two different jobs

**Date:** 2026-10-28
**Status:** suggestion (not implemented)
**Tags:** mcp, timeout, startup, diagnostics, ocap

> **Thesis.** Newt uses a single timeout — `McpServerEntry::request_timeout_secs`,
> resolved by `newt_mcp_client::resolve_timeout` — for **both** the `initialize`
> handshake and **every subsequent tool call**. Those are two different failure
> modes. A server that is slow to *start* (spawning sub-processes, waiting on a
> multiplex wrapper, resolving auth) needs a longer startup window but should
> **not** get a longer per-tool-call window. Newt has no distinct startup-timeout
> concept; it even rejects Codex's `startup_timeout_sec` field as unpreservable.
> The result is that a legitimately-slow-to-start server is indistinguishable from
> a wedged one, and the only lever an operator has widens *both* windows.

---

## TL;DR

- `finish_connect` wraps `conn.initialize()` in `tokio::time::timeout(timeout, …)`
  where `timeout = resolve_timeout(entry)` — the **same** duration used for every
  `tools/call` (`newt-mcp-client/src/lib.rs:2150`).
- `resolve_timeout` maps `request_timeout_secs` → `[1s, 600s]`, else
  `DEFAULT_REQUEST_TIMEOUT` = **20s** (`newt-mcp-client/src/lib.rs:97,109`).
- There is **no** `startup_timeout_secs` field on `McpServerEntry`
  (`newt-core/src/mcp.rs:524`). The Codex TOML parser explicitly drops any entry
  carrying `startup_timeout_sec` because Newt "cannot preserve its failure
  contract" (`newt-core/src/mcp.rs:1234`).
- When `initialize` times out, the error is opaque: `"initializing MCP server
  `<name>`"` / `"timed out awaiting `initialize` response"`. It names neither the
  resolved timeout, nor whether the child emitted *any* output before the
  deadline, nor the fix (`request_timeout_secs`).

## The incident that surfaced it

An MCP server was launched through a wrapper that multiplexes several
sub-servers and holds back `initialize`/`tools/list` until every sub-server
has answered. The underlying CLI had changed to send one composite response, so
the wrapper waited for answers that never came. Newt's 20s init deadline fired
first and reported the server `skipped` with `tool_count: 0`, though the same
server worked from a plain shell.

The fix in that case was to point Newt at the CLI directly (drop the wrapper),
not to widen a timeout. But the incident exposes a real gap in Newt's own model:

## Suggestions

### 1. Add a distinct `startup_timeout_secs` to `McpServerEntry`

Introduce a startup/initialize timeout separate from `request_timeout_secs`:

- `finish_connect` uses `startup_timeout_secs` (or a new
  `DEFAULT_STARTUP_TIMEOUT`, larger than 20s) for the `initialize` +
  `tools/list` phase only.
- `request_timeout_secs` continues to govern `tools/call` and every other
  request.
- This lets a slow-to-start server (multiplex wrappers, lazy auth, cold caches)
  complete its handshake without widening the per-call window that guards against
  a wedged tool.

This is the inverse of today's situation, where the *only* lever is
`request_timeout_secs`, which widens both windows at once.

### 2. Make the init-timeout error diagnostic, not opaque

When `initialize` (or `tools/list`) times out, the error should carry:

- the **resolved timeout** that fired (so the operator knows it was 20s, not
  their override);
- whether the child emitted **any** stdout/stderr before the deadline (a server
  that printed a startup banner is "slow", one that printed nothing is "wedged
  or misconfigured" — very different next steps);
- a hint naming the per-entry fix (`request_timeout_secs`, or the new
  `startup_timeout_secs` once it exists).

The `mcp_status` tool (added in #3048 / `f0a40e49`) already surfaces
`last_error` per server, so this is a matter of enriching the error string
`finish_connect` produces, not new plumbing.

### 3. Distinguish "slow" from "wedged" via first-byte detection

A cheap, high-signal heuristic: if the child produced **any** output before the
init deadline, report `timed out awaiting initialize (server emitted output —
likely slow startup, try raising <startup-timeout>)`; if it produced **none**,
report `timed out awaiting initialize (no output — server may be wedged or the
command is misconfigured)`. This converts the current single ambiguous message
into two actionable ones.

## Where the code lives

| Concern | Location |
|---|---|
| Per-request timeout default (20s) | `newt-mcp-client/src/lib.rs:97` |
| Timeout resolver / clamp | `newt-mcp-client/src/lib.rs:109` `resolve_timeout` |
| `initialize` + `tools/list` under one timeout | `newt-mcp-client/src/lib.rs:2150` `finish_connect` |
| `McpServerEntry` (no startup field) | `newt-core/src/mcp.rs:524` |
| Codex parser rejects `startup_timeout_sec` | `newt-core/src/mcp.rs:1234` |

---

Model: deepseek-v4-pro | Harness: newt-agent | Operator: Shawn Hartsock | Time: 07:16 PDT | Date: 2026-10-28
