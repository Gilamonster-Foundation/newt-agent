# Phase 21 — Centaur Data Scientist

**Status:** Step 21.1 done (the headless `newt-data` engine ships); step 21.2 in
progress — `newt-mcp-data`, the thin stdio MCP server over that engine, is the
**first usable Centaur slice**: net-new SQL EDA reachable from newt chat via one
`[[mcp_servers]]` line (see [Wiring it in](#wiring-it-in-21.2) below).

## Purpose

Bring notebook-driven data-science work (pandas / numpy / matplotlib / seaborn /
scikit-learn, CSV-driven EDA) into the agent family as a **Centaur** helper —
AI-use type 3, where the *human stays on top* and the agent is the powerful
engine making them faster. Not automation (type 1), not a fully-delegated
"easy button" (type 2). Every agent action is visible, reviewable, and the
human directs each step.

The standing thesis is "use PyO3 to create data-science tools": fast Rust
primitives, exposed both to the agent (as tools) and to the human's notebook
(as an importable Python module).

## Architecture (the binding shape)

Three pieces, all built in the `newt-agent` workspace (which builds today and
carries the PyO3 / maturin / release / coverage template):

1. **`newt-data`** — a headless engine library. No MCP, no JSON-RPC. Holds the
   `DataStore` trait (the DuckDB-later seam), the SQLite backend, CSV ingest,
   summaries, and (later steps) the Jupyter `KernelClient` and `.ipynb`
   persistence. This is where ~all coverage is earned (pure functions,
   in-memory SQLite, mock kernel).
2. **`newt-mcp-data`** — a thin stdio MCP server binary wrapping `newt-data`,
   mirroring `newt-mcp-server`. The agent gets the tools through the existing,
   tested MCP discovery→connect→route chain (`newt-core/src/mcp.rs` →
   `newt-tui/src/mcp.rs` → `newt-core/src/agentic/mcp.rs`) — **zero newt-core /
   newt-tui / agentic-loop changes**, one `[[mcp_servers]]` config line.
3. **`newt_data` PyO3 submodule** — registered through the existing
   `newt-agent-py` umbrella; exposes the same Rust primitives into notebook
   cells (the thesis). Wired in step 21.6.

**Why MCP-server delivery.** The family rule (`docs/decisions/plain_scroller_tui.md`,
gilamonster's `scrybe-markdown-surface.md`) is *extend via a separate binary or
MCP peer, never by bloating newt-core*. newt stays lean; gilamonster and
hermes-agent inherit the same server unchanged (MCP is the interchange bus). MCP
is plumbing; the chat/TUI co-pilot is the experience — the agentic loop already
streams every tool call + result inline, which **is** the Centaur seat. This
also makes the capability the **pilot** of the broader gilabot→family migration:
each re-homed plugin becomes an MCP peer (Python may stay Python until a hot path
earns a Rust+PyO3 rewrite), consumed identically by newt, gilamonster, hermes.

**Live-kernel transport (later steps).** Option A (pure-Rust client), REST-first:
the agent talks to the human's running JupyterLab over the Jupyter Server REST +
kernel websocket (reuses in-tree `reqwest`; no ZMQ/HMAC; matches the existing
`gila jupyter setup` flow). The server stays a lean Rust binary — **no libpython
linked at runtime** (the rejected Option B). PyO3 is orthogonal: it exposes Rust
*into* the notebook, it does not drive the kernel.

**Store.** SQLite now (reuse the family's bundled `rusqlite 0.31`, MSRV 1.75),
DuckDB later behind the same `DataStore` trait — swapping backends never changes
a tool signature. The DS database is **separate** (`<workspace>/.newt-data/data.db`),
never entangled with the conversation store.

## Tool surface (delivered incrementally; all namespaced `data__*`)

- **SQL EDA (21.2, no kernel needed — *shipped*, the first usable slice):**
  `sql_ingest_csv`, `sql_query` (exact SQL shown before run; honest `truncated`
  flag), `sql_summarize` (schema / dtypes / null-count / pandas-style
  `describe`), `sql_list_tables`. Delivered by the `newt-mcp-data` binary
  (mirrors `newt-mcp-server`); every tool returns the MCP content envelope and
  surfaces any failure (bad SQL, no such table, missing arg) as an **in-band**
  MCP tool error (`isError: true`) the model can read and recover from — never a
  `-32603` transport fault. The bare tool names are namespaced `data__*` by the
  client. See [Wiring it in](#wiring-it-in-21.2).
- **Kernel co-pilot (21.3):** `kernel_attach`, `run_cell` (proposed code visible
  before it runs; PNG written to `.newt-data/plots/…` and reported as path +
  honest text summary — never inline; rich render deferred to gilamonster),
  `interrupt_kernel` / `restart_kernel` (21.7).
- **Notebook artifact (21.4):** `notebook_read`, `notebook_insert_cell`
  (proposes; does not execute), `notebook_persist_executed_cell` — so
  `run_cell(persist_to=…)` leaves a faithful, reviewable, git-diffable record.
- **Dataframe introspection (21.5):** `list_dataframes`, `inspect_dataframe`.

## Roadmap (Drake-flight-sized; one PR each; full acceptance contract)

- **21.1** `newt-data` skeleton + `DataStore` trait + SQLite ingest/query/summarize. *(done)*
- **21.2** `newt-mcp-data` server with the SQL tools (first shippable Centaur slice). *(in progress — shipping)*
- **21.3** `KernelClient` trait + REST/websocket client + `kernel_attach`/`run_cell`.
- **21.4** notebook read/insert/persist-executed-cell + `run_cell(persist_to=…)`.
- **21.5** dataframe introspection · **21.6** PyO3 `newt_data` submodule + umbrella/release wiring ·
  **21.7** interrupt/restart + reconnect hardening · **21.8** DuckDB backend behind `DataStore` ·
  **21.9** optional raw-ZMQ kernel client · **21.10** `docs/decisions/centaur_data_scientist.md` + README config snippet.

## Wiring it in (21.2)

The SQL EDA slice is delivered as the `newt-mcp-data` binary. Add one
`[[mcp_servers]]` entry to `~/.newt/config.toml`:

```toml
[[mcp_servers]]
name = "data"
command = "newt-mcp-data"
```

The agent then discovers and routes to `data__sql_ingest_csv`,
`data__sql_query`, `data__sql_summarize`, and `data__sql_list_tables` through
the existing MCP discovery→connect→route chain — **zero** newt-core / newt-tui /
agentic-loop changes. The data database lives at
`<workspace>/.newt-data/data.db` (override with the `NEWT_DATA_DB` environment
variable), separate from the conversation store. The server routes all tracing
to stderr because stdout is the JSON-RPC wire.

## Long-term: gilabot → family migration

MCP is the interchange bus; agent-mesh the multi-machine transport. gilabot's
Python utilities migrate by becoming MCP servers (consumable by newt /
gilamonster / hermes unchanged), each following this shape: **engine (logic) +
MCP adapter (interchange) + optional PyO3 (in-notebook speed)**. A future ADR
codifies this as the family plugin contract.
