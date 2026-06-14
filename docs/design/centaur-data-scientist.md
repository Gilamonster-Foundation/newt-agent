# Phase 21 — Centaur Data Scientist

**Status:** Steps 21.1 and 21.2 done — the headless `newt-data` engine ships, and
`newt-mcp-data`, the thin stdio MCP server over it, is the **first usable Centaur
slice**: net-new SQL EDA reachable from newt chat via one `[[mcp_servers]]` line
(see [Wiring it in](#wiring-it-in-21.2) below). Step 21.3 done — the live-kernel
co-pilot: the agent now attaches to the human's running Jupyter server and runs
cells (`kernel_attach` + `run_cell`), reading back stdout/stderr, rich results,
and PNG plots written to disk (see [Live-kernel co-pilot](#live-kernel-co-pilot-21.3)
below). Step 21.4 done — the notebook artifact: `notebook_read` /
`notebook_insert_cell` / `notebook_persist_executed_cell`, and
`run_cell(persist_to=…)` now appends each executed cell (source + outputs, plots
inlined so the notebook renders) to a real `.ipynb`, leaving a faithful,
reviewable, git-diffable record (see [Notebook artifact](#notebook-artifact-21.4)
below). Step 21.5 done — read-only dataframe introspection over the attached live
kernel: `list_dataframes` enumerates the human's live pandas DataFrames (name /
rows / cols / in-memory bytes) and `inspect_dataframe` returns one frame's shape,
per-column dtype + null count, head rows, and numeric `describe()` — the Centaur
*sees* the human's working DataFrames without ever mutating them (see
[Dataframe introspection](#dataframe-introspection-21.5) below). Step 21.6 done —
the same SQLite primitives are importable into a human notebook as the
`newt_agent.data` PyO3 submodule of the umbrella wheel (see
[In the notebook](#in-the-notebook-21.6) below).

**With 21.1–21.6 all shipped, the Phase 21 Centaur MVP is complete:** the headless
engine, the SQL EDA slice, the live-kernel co-pilot, the notebook artifact, the
read-only dataframe introspection, and the in-notebook PyO3 submodule. Remaining
steps (21.7 interrupt/restart hardening, 21.8 DuckDB backend, 21.9 raw-ZMQ client,
21.10 the decision record) are post-MVP hardening and extensions.

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
   cells (the thesis). Wired in step 21.6 — *shipped*: `load_csv_to_sqlite`,
   `query` (returns `list[dict]`, honest int/float/None typing), and `summarize`
   (returns a dict with the pandas-faithful `describe`), plus a `DataError`
   exception. The wrappers are thin; all logic stays in `newt-data` (21.1). See
   [In the notebook](#in-the-notebook-21.6).

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
- **Kernel co-pilot (21.3, *shipped*):** `kernel_attach` (attach to a running
  Jupyter server, reuse-or-start a kernel, report kernel id + server url),
  `run_cell` (proposed code visible before it runs; stdout/stderr + rich
  `execute_result`/`display_data` text + any `ename`/`evalue`; PNG written to
  `<data-dir>/.newt-data/plots/cell-<n>-<uuid>.png` and reported as path +
  honest size summary — never inlined; rich render deferred to gilamonster).
  Pure-Rust transport (Option A1): Jupyter Server REST + kernel channels
  websocket, no embedded libpython. Every failure (unreachable server, wrong
  token, no kernel attached, transport drop) is an in-band MCP tool error the
  model can read; a cell that *raises* is a successful run with `error` set.
  `interrupt_kernel` / `restart_kernel` arrive in 21.7.
- **Notebook artifact (21.4, *shipped*):** `notebook_read` (a reviewable
  per-cell summary of an `.ipynb`), `notebook_insert_cell` (**proposes** a cell;
  does not execute it — a code cell goes in with `execution_count: null`,
  `outputs: []`), `notebook_persist_executed_cell` (appends a code cell with
  caller-supplied nbformat outputs). `run_cell` gains an optional
  `persist_to: <notebook.ipynb>`: after a successful run it converts the
  `CellRun` to nbformat outputs (PNG plots **re-read from disk and base64-inlined**
  so the notebook actually renders the plot) and appends the executed cell, so the
  human notebook stays a faithful, reviewable, git-diffable record. All writes are
  atomic (temp file in the same dir + rename, like the conversation store); a
  persist failure is reported but never discards an already-completed run. nbformat
  is pure `serde_json` JSON manipulation (no new dependency); a missing/corrupt/
  non-nbformat-4 file is an honest in-band error. See
  [Notebook artifact](#notebook-artifact-21.4).
- **Dataframe introspection (21.5, *shipped*):** `list_dataframes` (enumerate the
  live pandas DataFrames in the attached kernel's `globals()` → name / rows / cols
  / in-memory bytes), `inspect_dataframe` (one named frame → shape, per-column
  dtype + null count, head rows (default 5), and a numeric `describe()`). **Strictly
  read-only** — the Centaur sees the human's working DataFrames without mutating
  them. Each tool runs a defensive Python snippet over the session `run_cell` that
  imports json + pandas inside itself, never touches the namespace, and PRINTS one
  JSON line the Rust side parses robustly (no fragile text scraping); on a problem
  the snippet emits `{"error": ...}` rather than raising. The DataFrame `name` is
  validated as a plain Python identifier (`[A-Za-z_][A-Za-z0-9_]*`) **before** it is
  interpolated into the snippet, so a hostile name can never inject code — a bad
  name is rejected in-band before the kernel is ever touched. Requires
  `kernel_attach`; every failure (no kernel, undefined name, kernel error,
  unparseable output) is an in-band MCP tool error. See
  [Dataframe introspection](#dataframe-introspection-21.5).

## Roadmap (Drake-flight-sized; one PR each; full acceptance contract)

- **21.1** `newt-data` skeleton + `DataStore` trait + SQLite ingest/query/summarize. *(done)*
- **21.2** `newt-mcp-data` server with the SQL tools (first shippable Centaur slice). *(done)*
- **21.3** `KernelClient` trait + REST/websocket client + `kernel_attach`/`run_cell`. *(done)*
- **21.4** notebook read/insert/persist-executed-cell + `run_cell(persist_to=…)`. *(done)*
- **21.5** dataframe introspection *(done)* · **21.6** PyO3 `newt_data` submodule + umbrella/release wiring *(done)* ·
  **21.7** interrupt/restart + reconnect hardening · **21.8** DuckDB backend behind `DataStore` ·
  **21.9** optional raw-ZMQ kernel client · **21.10** `docs/decisions/centaur_data_scientist.md` + README config snippet.

With **21.1–21.6** done the Phase 21 Centaur MVP is complete; 21.7–21.10 are
post-MVP hardening + extensions.

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

## Live-kernel co-pilot (21.3)

The same `newt-mcp-data` server now also exposes `data__kernel_attach` and
`data__run_cell`, so the agent can run cells on the human's **already-running**
JupyterLab and read the outputs back into chat.

```text
1. data__kernel_attach { "url": "http://127.0.0.1:8888", "token": "<tok>" }
   → { "status": "attached", "kernel_id": "…", "server_url": "…" }
2. data__run_cell { "code": "import matplotlib.pyplot as plt; plt.plot([1,2,3]); plt.show()" }
   → { "stdout": "", "stderr": "", "results": [...],
       "images": [{ "path": "…/.newt-data/plots/cell-3-<uuid>.png",
                    "summary": "640x480 PNG saved: …/cell-3-<uuid>.png" }],
       "error": null, "execution_count": 3 }
```

**Transport (Option A1, pure-Rust — no libpython).** `kernel_attach` uses
`reqwest` to hit the Jupyter Server REST API (`GET /api/kernels`, reusing the
first running kernel or `POST`ing to start one; the token rides an
`Authorization: token <tok>` header). `run_cell` opens the per-kernel **channels
websocket** (`tokio-tungstenite`, `ws(s)://…/api/kernels/<id>/channels?token=…`),
sends one Jupyter `execute_request`, and folds the reply iopub stream — filtered
to replies whose `parent_header.msg_id` matches our request — into a `CellRun`,
stopping at the terminating `status: idle`. No ZMQ, no HMAC, no embedded
interpreter: the server stays a lean Rust binary (the rejected Option B linked
libpython at runtime). The websocket/HTTP stack is gated behind the `newt-data`
`kernel` cargo feature, so the default engine build and the PyO3 wheel stay lean.

**PNG to disk, never inlined.** An `image/png` output bundle is base64-decoded
and written to `<data-dir>/.newt-data/plots/cell-<n>-<uuid>.png`; only the path
and an honest size summary (`"640x480 PNG saved: …"`) travel back to the model.
Rich rendering is gilamonster's job.

**In-band errors (the Centaur contract).** Every failure — an unreachable
server, a wrong token, "no kernel attached", a dropped socket — is an in-band MCP
tool error (`isError: true`) the model can read and recover from, never a
`-32603` transport fault. A cell that *raises* a Python exception is **not** a
failure: it is a successful run whose `error` field (`ename`/`evalue`/`traceback`)
is populated.

**The testable heart.** All the output-folding logic lives in a pure
`Accumulator` (in `newt-data/src/kernel/mod.rs`) that folds a sequence of iopub
`serde_json::Value`s into a `CellRun` with a single injected PNG sink — unit-tested
against captured message fixtures with no live kernel. The REST + websocket
client (`rest.rs`) is covered by an in-process mock Jupyter (wiremock for REST,
a `tokio-tungstenite` server replaying a canned iopub sequence); a real-kernel
integration test is `#[ignore]` by default (set `JUPYTER_URL`) so CI never needs
a live kernel and the coverage gate never depends on one.

## Notebook artifact (21.4)

Running a cell on a live kernel (21.3) shows the output *in chat*; 21.4 leaves a
durable record *on disk*, so the human notebook stays a **faithful, reviewable,
git-diffable artifact** of what the agent did. Three tools manipulate an
`.ipynb`, plus a `persist_to` knob on `run_cell`:

```text
1. data__notebook_insert_cell { "path": "eda.ipynb", "source": "import pandas as pd" }
   → { "inserted_index": 0 }          # PROPOSES a cell; does NOT execute it
2. data__notebook_read { "path": "eda.ipynb" }
   → [ { "index": 0, "cell_type": "code", "source": "import pandas as pd",
         "has_output": false } ]
3. data__run_cell { "code": "df.describe().plot()", "persist_to": "eda.ipynb" }
   → { …run summary…,
       "persisted": { "path": "eda.ipynb", "index": 1 } }   # cell + outputs appended
```

- **`notebook_read`** returns a per-cell summary (`index`, `cell_type`,
  joined `source`, `has_output`) — a human-scannable view of the notebook.
- **`notebook_insert_cell`** *proposes* a cell without running it: a code cell
  goes in with `execution_count: null` and `outputs: []`, so a reviewer can tell
  at a glance it has not executed. `index` inserts at a position (out-of-range
  appends); `cell_type` defaults to `code`.
- **`notebook_persist_executed_cell`** appends a code cell carrying `source` and
  caller-supplied nbformat `outputs`. It is the low-level primitive
  `run_cell(persist_to)` calls; callers normally use `run_cell(persist_to)`.
- **`run_cell(persist_to=…)`** runs the cell, then converts the `CellRun` to
  nbformat outputs and appends it. `stdout`/`stderr` → `stream`; text results →
  `execute_result`; a raised exception → `error`; and each PNG plot is
  **re-read from `<plots>/cell-<n>-<uuid>.png` and base64-inlined** into a
  `display_data` output so the persisted notebook *renders the plot* (the chat
  summary still only carries the path — the inline lives in the artifact, not the
  conversation). The `persisted` field reports `{ path, index }` on success or
  `{ path, error }` on failure; **a persist failure never discards the run
  result** — the cell already ran.

**Atomic, dependency-light, honest errors.** Every write goes through a temp file
in the *same directory* + `rename` (intra-filesystem, so atomic) — the same
durable-write discipline as the conversation store; a reader (JupyterLab, a `git
diff`) never sees a half-written `.ipynb`, and a corrupt existing target is never
clobbered. nbformat is just `serde_json` JSON manipulation, so this needs **no new
dependency**. A missing, corrupt, or non-nbformat-4 file is a clean in-band MCP
tool error the model can read (never a panic). The pure read/insert/persist
engine lives in `newt-data/src/notebook.rs` (a *normal* module — no kernel/HTTP
deps), unit-tested over tempfile `.ipynb` fixtures; the `CellRun` → nbformat-output
bridge (`cell_run_to_nb_outputs`) lives beside the accumulator in
`newt-data/src/kernel/mod.rs`.

## Dataframe introspection (21.5)

The live-kernel co-pilot (21.3) runs the agent's *own* cells; 21.5 lets the agent
**look at the human's** pandas DataFrames — the ones already sitting in the
notebook's namespace — **without mutating them**. Two read-only tools, both over
the attached kernel:

```text
1. data__list_dataframes { }
   → [ { "name": "df",    "rows": 1000, "cols": 8, "memory_bytes": 412345 },
       { "name": "sales", "rows": 42,   "cols": 5, "memory_bytes":   3360 } ]

2. data__inspect_dataframe { "name": "sales", "head": 5 }
   → { "name": "sales",
       "shape": [42, 5],
       "columns": [ { "name": "region", "dtype": "object",  "null_count": 0 },
                    { "name": "amount", "dtype": "float64", "null_count": 2 }, … ],
       "head": [ { "region": "west", "amount": 100.0 }, … up to N rows … ],
       "describe": { "amount": { "count": 40.0, "mean": …, "std": …, "min": …,
                                 "25%": …, "50%": …, "75%": …, "max": … } } }
```

- **`list_dataframes`** enumerates `globals()` in the attached kernel for
  `pandas.DataFrame` instances and reports each one's variable name, row/column
  counts, and deep in-memory size in bytes.
- **`inspect_dataframe`** returns one named frame's `shape`, per-column
  `dtype` + `null_count`, the first `head` rows (default 5) as records, and a
  pandas `describe()` over the numeric columns (`{}` when the frame has none).

**How it works — a crafted snippet, robust JSON, no scraping.** Each tool runs a
defensive Python snippet through the session `run_cell` and parses **one JSON
line** out of `CellRun.stdout` (it takes the last non-empty line, so an incidental
`print` above it never breaks the parse). The snippet imports `json` + `pandas`
*inside itself* (a kernel without pandas yields a clean `{"error": ...}`, not a
raised `NameError`), iterates a *snapshot* of `globals()` so it **never mutates
the namespace**, and wraps everything in a `try/except` that prints
`{"error": ...}` rather than raising. The tool surfaces that `error` (or a kernel
fault, or unparseable output, or "no kernel attached") as an in-band MCP tool
error the model can read and recover from.

**Injection-proof name interpolation.** `inspect_dataframe` interpolates the
caller-supplied DataFrame `name` into the snippet, so the name is validated as a
plain Python identifier (`[A-Za-z_][A-Za-z0-9_]*`) **before** any kernel call — a
hostile name like `"df; import os"` is rejected in-band and the kernel is never
touched (the test asserts `run_cell` was not invoked). `list_dataframes` takes no
arguments and carries no interpolation at all.

**Read-only, by design.** Nothing here writes to the human's session: the snippets
only *read* `globals()` and call non-mutating DataFrame methods (`shape`,
`dtypes`, `isnull`, `head`, `describe`, `memory_usage`). This is the Centaur
contract — the agent sees what the human is working with and makes them faster,
without surprising them by changing their data underfoot.

## In the notebook (21.6)

The same SQLite primitives are importable into the human's own notebook through
the `newt-agent-py` umbrella wheel (the "use PyO3 to create data-science tools"
thesis). Build it with `maturin develop` (or `pip install newt-agent-py`), then:

```python
import newt_agent.data as nd

# Load a CSV into a SQLite DB and inspect the inferred schema.
report = nd.load_csv_to_sqlite("sales.csv", "data.db", "sales")
print(report.row_count, [(c.name, c.dtype) for c in report.columns])

# Query it back — rows come as list[dict] with honest int/float/None typing.
rows = nd.query("data.db", "SELECT region, amount FROM sales WHERE amount > 100")

# pandas-faithful describe (sample std, linear-interpolation quartiles).
summary = nd.summarize("data.db", "sales")
print(summary["columns"][1]["describe"]["mean"])
```

Errors surface as `nd.DataError` (bad SQL, no such table, …). The submodule is a
thin PyO3 wrapper over `newt-data`; the engine logic and its tests live in that
crate (21.1).

## Long-term: gilabot → family migration

MCP is the interchange bus; agent-mesh the multi-machine transport. gilabot's
Python utilities migrate by becoming MCP servers (consumable by newt /
gilamonster / hermes unchanged), each following this shape: **engine (logic) +
MCP adapter (interchange) + optional PyO3 (in-notebook speed)**. A future ADR
codifies this as the family plugin contract.
