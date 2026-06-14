//! MCP protocol handlers for the Centaur SQL EDA tools (Phase 21).
//!
//! Registers the JSON-RPC methods that `newt-mcp-data` exposes — `initialize`,
//! `tools/list`, and `tools/call` — for the four SQL tools defined in §4.1 of
//! [`docs/design/centaur-data-scientist.md`](../../../docs/design/centaur-data-scientist.md):
//! `sql_ingest_csv`, `sql_query`, `sql_summarize`, and `sql_list_tables`.
//!
//! Mirrors `newt-mcp-server/src/handlers.rs`, but every tool here is a pure
//! call into the headless [`newt_data`] engine — there is no inference, no
//! confined shell, and no capability leash. The single piece of runtime state
//! is the shared [`SqliteBackend`], wired into the `tools/call` closure with the
//! same `Arc`-clone-into-async-block pattern the code server uses.
//!
//! ## In-band error discipline (the load-bearing Centaur contract)
//!
//! Every data tool returns the MCP content envelope. On **any** failure — bad
//! SQL, a missing table, a missing or mistyped argument — the handler returns an
//! in-band MCP *tool error* `{ content: [{ type: text, text: reason }], isError:
//! true }`, **not** a `-32603` transport fault. The rationale is the same one
//! `newt-mcp-server` applies to `shell_run`: the model (and the human watching
//! the chat) must *see* the error and recover from it, rather than have the call
//! collapse into an opaque transport error. Each handler therefore returns a
//! [`serde_json::Value`] directly (the `tools/call` arm wraps it in `Ok`) and
//! never bubbles a [`newt_data::DataError`] up the transport.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use newt_data::kernel::rest::RestKernelClient;
use newt_data::kernel::KernelClient;
use newt_data::{DataStore, SqliteBackend};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::server::McpServer;

/// The default `row_cap` for `sql_query` when the caller omits one — mirrors the
/// engine's documented small-result default. An absent, negative, or *mistyped*
/// cap (a float, a stringified number, a bool) falls back to this; a genuine
/// huge integer is accepted as-is (the engine reads at most that many rows and
/// sets the honest `truncated` flag). See [`parse_row_cap`].
const DEFAULT_ROW_CAP: usize = 1000;

/// The live-kernel session state (Phase 21.3): the currently-attached
/// [`KernelClient`], if any.
///
/// `None` until `kernel_attach` succeeds. Behind a `tokio::Mutex` (not a
/// `std::Mutex`) because `run_cell` awaits the kernel websocket while holding it,
/// serializing concurrent cell runs over one kernel — which is exactly the
/// semantics a single Jupyter kernel has (one execution at a time). `Arc` so the
/// `tools/call` closure (an `Fn`) can clone it per invocation, mirroring the
/// shared-store pattern.
pub type KernelSession = Arc<Mutex<Option<Box<dyn KernelClient>>>>;

/// A fresh, empty kernel session (no kernel attached yet).
pub fn new_kernel_session() -> KernelSession {
    Arc::new(Mutex::new(None))
}

/// Register every MCP handler on `server`: the four SQL EDA tools plus the two
/// Phase 21.3 live-kernel tools (`kernel_attach`, `run_cell`).
///
/// `store` is the shared [`SqliteBackend`]; `session` is the (initially empty)
/// kernel session a successful `kernel_attach` fills; `plots_dir` is where
/// `run_cell` writes decoded PNG plots (`<data-dir>/plots`). All three are shared
/// (via `Arc`) into the `tools/call` closure so each invocation gets its own
/// clone (the outer closure is `Fn`, not `FnOnce`).
pub fn register_handlers(
    server: &mut McpServer,
    store: Arc<SqliteBackend>,
    session: KernelSession,
    plots_dir: PathBuf,
) {
    register_initialize(server);
    register_tools_list(server);
    register_tools_call(server, store, session, plots_dir);
}

// ── initialize ─────────────────────────────────────────────────────────────

fn register_initialize(server: &mut McpServer) {
    server.register("initialize", |_params| async move {
        Ok(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "newt-mcp-data",
                "version": env!("CARGO_PKG_VERSION")
            }
        }))
    });
}

// ── tools/list ─────────────────────────────────────────────────────────────

fn register_tools_list(server: &mut McpServer) {
    server.register("tools/list", |_params| async move {
        Ok(serde_json::json!({
            "tools": tool_definitions()
        }))
    });
}

/// Return the JSON array of the four SQL tool definitions (§4.1).
///
/// The names are bare (`sql_query`, …); the MCP client namespaces them as
/// `data__*` when the server is configured under the name `"data"`.
fn tool_definitions() -> Value {
    let tools = vec![
        serde_json::json!({
            "name": "sql_ingest_csv",
            "description": "Ingest a CSV file into a SQLite table (dtype-inferred). Returns the table name, row count, and per-column dtype + null counts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Filesystem path to the CSV file to ingest"
                    },
                    "table": {
                        "type": "string",
                        "description": "Table name to (re)create from the CSV (dropped + recreated)"
                    }
                },
                "required": ["path", "table"]
            }
        }),
        serde_json::json!({
            "name": "sql_query",
            "description": "Run a read-or-write SQL statement against the data store. Returns columns, rows (capped), and a truthful `truncated` flag. The exact SQL is shown in chat before it runs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "The SQL statement to execute"
                    },
                    "row_cap": {
                        "type": "integer",
                        "description": "Maximum rows to return (default 1000); the engine reads one past the cap to set the honest `truncated` flag"
                    }
                },
                "required": ["sql"]
            }
        }),
        serde_json::json!({
            "name": "sql_summarize",
            "description": "Summarize a table: schema, dtypes, null and distinct counts, and a pandas-style numeric describe (count/mean/std/min/quartiles/max).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "table": {
                        "type": "string",
                        "description": "Table name to summarize"
                    }
                },
                "required": ["table"]
            }
        }),
        serde_json::json!({
            "name": "sql_list_tables",
            "description": "List ingested tables with row counts and source CSV paths.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        serde_json::json!({
            "name": "kernel_attach",
            "description": "Attach to the human's already-running Jupyter server so `run_cell` can execute code on a live kernel. Reuses a running kernel (or starts one) and returns the kernel id + server URL. Call this once before `run_cell`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Jupyter Server base URL (e.g. http://127.0.0.1:8888)"
                    },
                    "token": {
                        "type": "string",
                        "description": "Jupyter token, if the server requires one"
                    },
                    "kernel_id": {
                        "type": "string",
                        "description": "Adopt a specific kernel id; omit to reuse the first running kernel (or start one)"
                    }
                },
                "required": ["url"]
            }
        }),
        serde_json::json!({
            "name": "run_cell",
            "description": "Run a code cell on the attached Jupyter kernel (call kernel_attach first). Returns stdout/stderr, rich text results, and any error. PNG plots are written to <data-dir>/.newt-data/plots/ and reported as a file path + honest size summary — never inlined. The exact code is shown in chat before it runs. Pass `persist_to` to also append this executed cell (source + outputs, plots inlined so the notebook renders) to a .ipynb on disk — a faithful, reviewable, git-diffable record.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "The Python (or kernel-language) code to execute in one cell"
                    },
                    "persist_to": {
                        "type": "string",
                        "description": "Optional path to a .ipynb notebook; after a successful run, append this cell (source + outputs) to it (created if missing). The run result is returned regardless — a persist failure is reported but does not discard it."
                    }
                },
                "required": ["code"]
            }
        }),
        serde_json::json!({
            "name": "notebook_read",
            "description": "Read an .ipynb notebook and return a reviewable summary of every cell: index, cell_type (code/markdown/raw), source (joined), and whether a code cell has outputs. A missing, corrupt, or non-nbformat-4 file is an in-band error.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Filesystem path to the .ipynb notebook to read"
                    }
                },
                "required": ["path"]
            }
        }),
        serde_json::json!({
            "name": "notebook_insert_cell",
            "description": "PROPOSE a cell in an .ipynb notebook without executing it (a code cell goes in with execution_count:null and no outputs). Inserts at `index` (or appends when omitted); creates the notebook if it does not exist. Returns the inserted index. To run AND record a cell, use run_cell with persist_to instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Filesystem path to the .ipynb notebook (created if missing)"
                    },
                    "source": {
                        "type": "string",
                        "description": "The cell source (code or markdown text)"
                    },
                    "index": {
                        "type": "integer",
                        "description": "Zero-based position to insert at; omit to append. An out-of-range index appends."
                    },
                    "cell_type": {
                        "type": "string",
                        "description": "Cell kind: code (default), markdown, or raw"
                    }
                },
                "required": ["path", "source"]
            }
        }),
        serde_json::json!({
            "name": "notebook_persist_executed_cell",
            "description": "Append a CODE cell carrying `source` and caller-supplied nbformat `outputs` (already nbformat-shaped Values) to an .ipynb notebook, atomically; creates the notebook if missing. Returns the appended index. This is the low-level primitive run_cell(persist_to) uses; prefer run_cell(persist_to) to run and record in one step.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Filesystem path to the .ipynb notebook (created if missing)"
                    },
                    "source": {
                        "type": "string",
                        "description": "The executed cell's source"
                    },
                    "outputs": {
                        "type": "array",
                        "description": "nbformat-shaped output Values (stream / execute_result / display_data / error) to attach to the cell",
                        "items": { "type": "object" }
                    }
                },
                "required": ["path", "source", "outputs"]
            }
        }),
    ];

    Value::Array(tools)
}

// ── tools/call ─────────────────────────────────────────────────────────────

fn register_tools_call(
    server: &mut McpServer,
    store: Arc<SqliteBackend>,
    session: KernelSession,
    plots_dir: PathBuf,
) {
    server.register("tools/call", move |params| {
        // Move clones into the async block so each invocation owns its own
        // Arc / PathBuf (the outer closure is `Fn`, not `FnOnce`).
        let store = store.clone();
        let session = session.clone();
        let plots_dir = plots_dir.clone();
        async move {
            let name = params
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));

            match name.as_str() {
                "sql_ingest_csv" => Ok(handle_sql_ingest_csv(&arguments, &store)),
                "sql_query" => Ok(handle_sql_query(&arguments, &store)),
                "sql_summarize" => Ok(handle_sql_summarize(&arguments, &store)),
                "sql_list_tables" => Ok(handle_sql_list_tables(&store)),
                "kernel_attach" => Ok(handle_kernel_attach(&arguments, &session, &plots_dir).await),
                "run_cell" => Ok(handle_run_cell(&arguments, &session).await),
                "notebook_read" => Ok(handle_notebook_read(&arguments)),
                "notebook_insert_cell" => Ok(handle_notebook_insert_cell(&arguments)),
                "notebook_persist_executed_cell" => {
                    Ok(handle_notebook_persist_executed_cell(&arguments))
                }
                // An unknown tool name is a transport-level `-32603`, matching
                // `newt-mcp-server`'s `other =>` arm. A *known* tool failing for
                // its own reasons (bad SQL, missing arg, no kernel attached)
                // stays in-band, above.
                other => anyhow::bail!("unknown tool: {other}"),
            }
        }
    });
}

// ── Tool implementations ───────────────────────────────────────────────────
//
// Each returns a `Value` directly — never an `anyhow::Error`. A `DataError` or a
// missing argument becomes an in-band MCP tool error so the model can read and
// recover from it (the Centaur in-band-error discipline; see module docs).

fn handle_sql_ingest_csv(args: &Value, store: &SqliteBackend) -> Value {
    let path = match required_str(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let table = match required_str(args, "table") {
        Ok(t) => t,
        Err(e) => return e,
    };

    match store.ingest_csv(Path::new(path), table) {
        Ok(report) => pretty_or_error(serde_json::to_string_pretty(&report)),
        Err(e) => mcp_error_content(&e.to_string()),
    }
}

fn handle_sql_query(args: &Value, store: &SqliteBackend) -> Value {
    let sql = match required_str(args, "sql") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let row_cap = parse_row_cap(args.get("row_cap"));

    match store.query(sql, row_cap) {
        Ok(result) => pretty_or_error(serde_json::to_string_pretty(&result)),
        Err(e) => mcp_error_content(&e.to_string()),
    }
}

fn handle_sql_summarize(args: &Value, store: &SqliteBackend) -> Value {
    let table = match required_str(args, "table") {
        Ok(t) => t,
        Err(e) => return e,
    };

    match store.summarize(table) {
        Ok(summary) => pretty_or_error(serde_json::to_string_pretty(&summary)),
        Err(e) => mcp_error_content(&e.to_string()),
    }
}

fn handle_sql_list_tables(store: &SqliteBackend) -> Value {
    match store.list_tables() {
        Ok(tables) => pretty_or_error(serde_json::to_string_pretty(&tables)),
        Err(e) => mcp_error_content(&e.to_string()),
    }
}

// ── Live-kernel tools (Phase 21.3) ──────────────────────────────────────────
//
// Like the SQL tools, every failure is an in-band MCP tool error (isError:true),
// never a -32603 transport fault: an unreachable Jupyter server, a wrong token,
// or "no kernel attached" must all be readable by the model so it can recover
// (the Centaur in-band-error discipline). A cell that *raises* a Python
// exception is NOT a failure here — it is a successful run whose `error` field
// is populated, surfaced as normal content.

/// `kernel_attach`: connect to the human's running Jupyter server and store the
/// resulting [`KernelClient`] in the session. Returns the kernel id + server URL
/// as text on success; an in-band error (server unreachable, bad token, missing
/// `url`) on failure.
async fn handle_kernel_attach(args: &Value, session: &KernelSession, plots_dir: &Path) -> Value {
    let url = match required_str(args, "url") {
        Ok(u) => u,
        Err(e) => return e,
    };
    let token = args.get("token").and_then(Value::as_str);
    let kernel_id = args.get("kernel_id").and_then(Value::as_str);

    match RestKernelClient::connect(url, token, kernel_id, plots_dir.to_path_buf()).await {
        Ok(client) => {
            let summary = serde_json::json!({
                "status": "attached",
                "kernel_id": client.kernel_id(),
                "server_url": client.base_url(),
            });
            // Store the live client for subsequent `run_cell` calls.
            *session.lock().await = Some(Box::new(client));
            pretty_or_error(serde_json::to_string_pretty(&summary))
        }
        Err(e) => mcp_error_content(&format!("kernel_attach failed: {e}")),
    }
}

/// `run_cell`: execute `code` on the attached kernel and summarize the run.
///
/// In-band error if no kernel is attached (tells the model to call
/// `kernel_attach` first) or if the transport fails. A successful run — even one
/// where the cell raised — returns a [`CellRun`](newt_data::kernel::CellRun)
/// summary as pretty JSON: stdout/stderr, text results, image **paths** + honest
/// size summaries (never inlined bytes), and any `ename`/`evalue`.
///
/// ## `persist_to` (Phase 21.4)
///
/// When the caller passes `persist_to: <notebook.ipynb>`, the executed cell is
/// appended to that notebook after a successful run: the [`CellRun`] is converted
/// to nbformat outputs (PNG plots **re-read and inlined** so the notebook
/// renders) and appended via [`newt_data::notebook::persist_cell`], leaving a
/// faithful, git-diffable record. The persist is reported in the summary's
/// `persisted` field. Crucially, **a persist failure does not discard the run
/// result** — the cell already ran; the failure is reported (`persisted.error`)
/// alongside the run so the model still sees the output and can retry the write.
async fn handle_run_cell(args: &Value, session: &KernelSession) -> Value {
    let code = match required_str(args, "code") {
        Ok(c) => c,
        Err(e) => return e,
    };
    // `persist_to` is optional; absent → no persistence.
    let persist_to = args.get("persist_to").and_then(Value::as_str);

    let guard = session.lock().await;
    let client = match guard.as_ref() {
        Some(c) => c,
        None => {
            return mcp_error_content(
                "no kernel attached — call kernel_attach with your Jupyter server url first",
            )
        }
    };

    let run = match client.run_cell(code).await {
        Ok(run) => run,
        Err(e) => return mcp_error_content(&format!("run_cell failed: {e}")),
    };

    // After a successful run, optionally persist the executed cell. A persist
    // failure is reported in the summary but never discards the run result.
    let persisted = persist_to.map(|path| persist_run(path, code, &run));

    pretty_or_error(serde_json::to_string(&run_summary(&run, persisted)))
}

/// Append a just-executed cell to a notebook, returning the model-facing
/// `persisted` summary object (`{ path, index }` on success, `{ path, error }`
/// on failure). Converts the [`CellRun`] to nbformat outputs (Phase 21.4 bridge)
/// and calls [`newt_data::notebook::persist_cell`]; the run result is reported by
/// the caller regardless of this outcome.
fn persist_run(path: &str, code: &str, run: &newt_data::kernel::CellRun) -> Value {
    let outputs = newt_data::kernel::cell_run_to_nb_outputs(run);
    match newt_data::notebook::persist_cell(Path::new(path), code, outputs) {
        Ok(index) => serde_json::json!({ "path": path, "index": index }),
        Err(e) => serde_json::json!({ "path": path, "error": e.to_string() }),
    }
}

/// Build the honest, model-facing JSON summary of a [`CellRun`](newt_data::kernel::CellRun).
///
/// Images are reported as `{ path, summary }` where `summary` is a human string
/// like `"640x480 PNG saved: <path>"` (size omitted when the kernel did not
/// report dimensions) — the bytes are **never** inlined (Centaur principle; rich
/// render is gilamonster's job). stdout/stderr/results/error/execution_count are
/// passed through faithfully.
///
/// `persisted` (Phase 21.4) carries the `run_cell(persist_to=…)` outcome when the
/// caller asked to record the cell: `{ path, index }` on success, `{ path, error
/// }` if the write failed (the run result is reported either way). It is `null`
/// when no `persist_to` was given.
fn run_summary(run: &newt_data::kernel::CellRun, persisted: Option<Value>) -> Value {
    let images: Vec<Value> = run
        .images
        .iter()
        .map(|img| {
            let path = img.path.display().to_string();
            let summary = match (img.width, img.height) {
                (Some(w), Some(h)) => format!("{w}x{h} PNG saved: {path}"),
                _ => format!("PNG saved: {path}"),
            };
            serde_json::json!({ "path": path, "summary": summary })
        })
        .collect();

    let results: Vec<Value> = run
        .results
        .iter()
        .map(|d| serde_json::json!({ "mime": d.mime, "text": d.text }))
        .collect();

    let error = run.error.as_ref().map(|e| {
        serde_json::json!({
            "ename": e.ename,
            "evalue": e.evalue,
            "traceback": e.traceback,
        })
    });

    serde_json::json!({
        "stdout": run.stdout,
        "stderr": run.stderr,
        "results": results,
        "images": images,
        "error": error,
        "execution_count": run.execution_count,
        "persisted": persisted,
    })
}

// ── Notebook tools (Phase 21.4) ─────────────────────────────────────────────
//
// The on-disk counterpart to the live-kernel tools: read / propose / persist
// cells in a human-reviewable .ipynb artifact (docs/design/centaur-data-scientist.md
// §4.1, the notebook-artifact bullet). Same in-band-error discipline — a missing
// file, a corrupt notebook, or a missing argument is an in-band MCP tool error
// the model can read, never a -32603 transport fault. These are pure calls into
// the headless `newt_data::notebook` engine (no kernel needed), so they work
// even before `kernel_attach`.

/// `notebook_read`: summarize every cell of an `.ipynb` as pretty JSON. A
/// missing, corrupt, or non-nbformat-4 file is an in-band error.
fn handle_notebook_read(args: &Value) -> Value {
    let path = match required_str(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    match newt_data::notebook::read_notebook(Path::new(path)) {
        Ok(cells) => pretty_or_error(serde_json::to_string_pretty(&cells)),
        Err(e) => mcp_error_content(&e.to_string()),
    }
}

/// `notebook_insert_cell`: PROPOSE a cell (does not execute it). Inserts at
/// `index` (or appends), creating the notebook if missing; returns the inserted
/// index. `cell_type` defaults to `code`.
fn handle_notebook_insert_cell(args: &Value) -> Value {
    let path = match required_str(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let source = match required_str(args, "source") {
        Ok(s) => s,
        Err(e) => return e,
    };
    // `index` is optional; a non-integer or absent value → append (None).
    let index = args
        .get("index")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    // `cell_type` defaults to code; an unrecognized value also folds to code.
    let cell_type = newt_data::notebook::CellType::from_str_lenient(
        args.get("cell_type").and_then(Value::as_str).unwrap_or(""),
    );

    match newt_data::notebook::insert_cell(Path::new(path), source, index, cell_type) {
        Ok(at) => mcp_text_content(&serde_json::json!({ "inserted_index": at }).to_string()),
        Err(e) => mcp_error_content(&e.to_string()),
    }
}

/// `notebook_persist_executed_cell`: append a code cell with caller-supplied
/// nbformat `outputs`. The low-level primitive `run_cell(persist_to)` uses;
/// returns the appended index. A missing/non-array `outputs` is an in-band error.
fn handle_notebook_persist_executed_cell(args: &Value) -> Value {
    let path = match required_str(args, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let source = match required_str(args, "source") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let outputs = match args.get("outputs").and_then(Value::as_array) {
        Some(arr) => arr.clone(),
        None => return mcp_error_content("missing required argument: outputs (must be an array)"),
    };

    match newt_data::notebook::persist_cell(Path::new(path), source, outputs) {
        Ok(at) => mcp_text_content(&serde_json::json!({ "appended_index": at }).to_string()),
        Err(e) => mcp_error_content(&e.to_string()),
    }
}

// ── Argument + result helpers ──────────────────────────────────────────────

/// Extract a required string argument, or return the in-band tool error a
/// missing/mistyped argument should produce (`missing required argument: …`).
fn required_str<'a>(args: &'a Value, name: &str) -> std::result::Result<&'a str, Value> {
    args.get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| mcp_error_content(&format!("missing required argument: {name}")))
}

/// Parse the optional `row_cap` argument into a [`usize`].
///
/// Anything that is not a representable non-negative integer → [`DEFAULT_ROW_CAP`].
/// This covers a missing/null cap, a negative integer (meaningless), and any
/// *mistyped* cap — a JSON float (`2.5`), a stringified number (`"100"`), or a
/// bool — all of which a model occasionally emits. Falling back to the safe
/// default (rather than an unbounded read) keeps the honest `truncated` contract
/// intact even when the cap is garbage: an unbounded query is the dangerous case,
/// so a meaningless cap is treated as "use the default", never "read everything".
///
/// A genuine non-negative integer is taken as-is — even an absurdly large one
/// (a huge `u64` saturates to [`usize::MAX`]); the engine reads at most that many
/// rows and sets the honest `truncated` flag, so a large value is accepted rather
/// than rejected. See §4.1 of `docs/design/centaur-data-scientist.md`.
fn parse_row_cap(value: Option<&Value>) -> usize {
    // `as_u64()` is `Some` only for a non-negative integer within `u64` range;
    // it is `None` for a missing/null value, a negative integer, a float, a
    // string, or a bool — every one of which we fold into the safe default.
    match value.and_then(Value::as_u64) {
        Some(n) => n as usize,
        None => DEFAULT_ROW_CAP,
    }
}

/// Wrap an already-attempted pretty serialization in the MCP envelope: the
/// pretty JSON text on success, or — for the practically impossible
/// serialization failure of an engine result type — an in-band tool error
/// rather than a transport fault.
///
/// Takes the [`serde_json::Result`] (not a `serde::Serialize` value) so this
/// crate never has to name the `serde` trait directly — `serde` is only a
/// transitive dependency through `serde_json`, and the dependency budget here is
/// deliberately minimal (Phase 21).
fn pretty_or_error(serialized: serde_json::Result<String>) -> Value {
    match serialized {
        Ok(text) => mcp_text_content(&text),
        Err(e) => mcp_error_content(&format!("failed to serialize result: {e}")),
    }
}

/// Wrap a string in the MCP content envelope: `{ "content": [{ "type": "text", "text": ... }] }`.
fn mcp_text_content(text: &str) -> Value {
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": text
        }]
    })
}

/// Wrap a reason in the MCP **tool error** envelope: the content shape plus
/// `isError: true`. This is what a data failure looks like across the MCP
/// boundary — an in-band tool error the model can read, not a transport fault.
fn mcp_error_content(reason: &str) -> Value {
    serde_json::json!({
        "content": [{
            "type": "text",
            "text": reason
        }],
        "isError": true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Fixture mirroring the 21.1 engine test CSV: id (Integer), label (Text),
    /// score (Real) with one empty score cell (row 5) so the null/describe
    /// numbers are checkable. Non-null scores [1,2,3,4] give the pandas
    /// describe below (mean 2.5, std 1.290994…, quartiles 1.75/2.5/3.25).
    const FIXTURE_CSV: &str = "id,label,score\n\
        1,alpha,1.0\n\
        2,bravo,2.0\n\
        3,charlie,3.0\n\
        4,delta,4.0\n\
        5,echo,\n";

    /// Write `contents` to a NamedTempFile and return it (kept alive by the
    /// caller so the path stays valid).
    fn csv_file(contents: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    /// Build a fully-wired McpServer over a fresh in-memory store and send a
    /// single request through it via in-memory byte buffers. Hermetic: the
    /// store is `open_in_memory`, never a real `~/.newt-data` path.
    async fn rpc(request: &Value) -> Value {
        rpc_with(Arc::new(SqliteBackend::open_in_memory().unwrap()), request).await
    }

    /// Like [`rpc`], but with a caller-supplied store so a multi-step flow
    /// (ingest → query → summarize → list) can share one database. The kernel
    /// session starts empty (no kernel attached).
    async fn rpc_with(store: Arc<SqliteBackend>, request: &Value) -> Value {
        rpc_full(store, new_kernel_session(), request).await
    }

    /// The fully-parameterized harness: a caller-supplied store **and** kernel
    /// session, so the live-kernel tests can pre-attach a [`MockKernel`] (or
    /// assert the empty-session error) and the SQL tests can share a database.
    async fn rpc_full(store: Arc<SqliteBackend>, session: KernelSession, request: &Value) -> Value {
        // A throwaway plots dir; the MockKernel never writes there (it returns a
        // canned CellRun), so the path only needs to be syntactically valid.
        let plots_dir = std::env::temp_dir().join("newt-mcp-data-test-plots");
        let mut server = McpServer::new();
        register_handlers(&mut server, store, session, plots_dir);

        let input = format!("{}\n", serde_json::to_string(request).unwrap());
        let mut output: Vec<u8> = Vec::new();
        server.run(input.as_bytes(), &mut output).await.unwrap();
        let text = String::from_utf8(output).unwrap();
        serde_json::from_str(text.trim()).unwrap()
    }

    /// A canned [`KernelClient`] for the tool-logic tests: it returns a fixed
    /// [`CellRun`] (or a fixed transport error) without any websocket — so the
    /// `run_cell` / `kernel_attach` MCP envelope logic (PNG path reporting, the
    /// no-kernel error, the in-band error discipline) is exercised hermetically,
    /// no live Jupyter kernel required.
    struct MockKernel {
        run: newt_data::kernel::CellRun,
        fail: Option<String>,
    }

    #[async_trait::async_trait]
    impl KernelClient for MockKernel {
        async fn run_cell(&self, _code: &str) -> anyhow::Result<newt_data::kernel::CellRun> {
            match &self.fail {
                Some(msg) => anyhow::bail!("{msg}"),
                None => Ok(self.run.clone()),
            }
        }
    }

    /// A session with a pre-attached [`MockKernel`] returning `run`.
    fn session_with(run: newt_data::kernel::CellRun) -> KernelSession {
        Arc::new(Mutex::new(Some(
            Box::new(MockKernel { run, fail: None }) as Box<dyn KernelClient>
        )))
    }

    /// A session with a pre-attached [`MockKernel`] whose `run_cell` errors.
    fn session_failing(msg: &str) -> KernelSession {
        Arc::new(Mutex::new(Some(Box::new(MockKernel {
            run: newt_data::kernel::CellRun::default(),
            fail: Some(msg.to_string()),
        }) as Box<dyn KernelClient>)))
    }

    /// Helper: a `tools/call` request body.
    fn call(id: i64, name: &str, arguments: Value) -> Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        })
    }

    // ── initialize ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn initialize_returns_protocol_version_and_name() {
        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
        }))
        .await;

        let result = &resp["result"];
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["serverInfo"]["name"], "newt-mcp-data");
        assert!(result["capabilities"]["tools"].is_object());
    }

    // ── tools/list ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn tools_list_returns_the_sql_and_kernel_tools() {
        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
        }))
        .await;

        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // The four SQL tools (21.2), the two live-kernel tools (21.3), and the
        // three notebook tools (21.4).
        for expected in [
            "sql_ingest_csv",
            "sql_query",
            "sql_summarize",
            "sql_list_tables",
            "kernel_attach",
            "run_cell",
            "notebook_read",
            "notebook_insert_cell",
            "notebook_persist_executed_cell",
        ] {
            assert!(names.contains(&expected), "tools/list missing {expected}");
        }
        assert_eq!(tools.len(), 9, "expected exactly 9 tools, got {names:?}");

        // Every tool carries an inputSchema object.
        for tool in tools {
            assert!(
                tool["inputSchema"].is_object(),
                "tool {} missing inputSchema",
                tool["name"]
            );
        }
        // kernel_attach requires `url`; run_cell requires `code` (persist_to is
        // optional, so it must NOT be in `required`).
        let attach = tools.iter().find(|t| t["name"] == "kernel_attach").unwrap();
        assert_eq!(
            attach["inputSchema"]["required"],
            serde_json::json!(["url"])
        );
        let run = tools.iter().find(|t| t["name"] == "run_cell").unwrap();
        assert_eq!(run["inputSchema"]["required"], serde_json::json!(["code"]));
        // run_cell advertises the optional persist_to property.
        assert!(
            run["inputSchema"]["properties"]["persist_to"].is_object(),
            "run_cell must advertise the optional persist_to argument"
        );
        // notebook_insert_cell requires path + source; persist requires outputs too.
        let insert = tools
            .iter()
            .find(|t| t["name"] == "notebook_insert_cell")
            .unwrap();
        assert_eq!(
            insert["inputSchema"]["required"],
            serde_json::json!(["path", "source"])
        );
        let persist = tools
            .iter()
            .find(|t| t["name"] == "notebook_persist_executed_cell")
            .unwrap();
        assert_eq!(
            persist["inputSchema"]["required"],
            serde_json::json!(["path", "source", "outputs"])
        );
    }

    // ── happy-path flow: ingest → query → summarize → list ──────────────────

    #[tokio::test]
    async fn happy_path_flow_over_shared_store() {
        let store = Arc::new(SqliteBackend::open_in_memory().unwrap());
        let f = csv_file(FIXTURE_CSV);

        // 1. Ingest the fixture CSV.
        let resp = rpc_with(
            store.clone(),
            &call(
                10,
                "sql_ingest_csv",
                serde_json::json!({ "path": f.path().to_str().unwrap(), "table": "metrics" }),
            ),
        )
        .await;
        assert!(resp["error"].is_null(), "ingest must be in-band: {resp}");
        assert!(
            resp["result"]["isError"].is_null(),
            "ingest must succeed: {resp}"
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let report: Value = serde_json::from_str(text).unwrap();
        assert_eq!(report["table"], "metrics");
        assert_eq!(report["row_count"], 5);
        assert_eq!(report["columns"].as_array().unwrap().len(), 3);

        // 2. Query with a small row_cap → truncated flag set honestly.
        let resp = rpc_with(
            store.clone(),
            &call(
                11,
                "sql_query",
                serde_json::json!({ "sql": "SELECT * FROM metrics", "row_cap": 2 }),
            ),
        )
        .await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let result: Value = serde_json::from_str(text).unwrap();
        assert_eq!(
            result["columns"],
            serde_json::json!(["id", "label", "score"])
        );
        assert_eq!(result["returned"], 2);
        assert_eq!(result["rows"].as_array().unwrap().len(), 2);
        assert_eq!(result["truncated"], true, "5 rows, cap 2 → truncated");

        // 3. Summarize → the pandas describe numbers appear.
        let resp = rpc_with(
            store.clone(),
            &call(
                12,
                "sql_summarize",
                serde_json::json!({ "table": "metrics" }),
            ),
        )
        .await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let summary: Value = serde_json::from_str(text).unwrap();
        assert_eq!(summary["table"], "metrics");
        assert_eq!(summary["row_count"], 5);
        let score = summary["columns"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "score")
            .unwrap();
        assert_eq!(score["null_count"], 1);
        assert_eq!(score["distinct_count"], 4);
        let d = &score["numeric"];
        assert_eq!(d["count"], 4);
        assert_eq!(d["mean"], 2.5);
        assert_eq!(d["min"], 1.0);
        assert_eq!(d["q25"], 1.75);
        assert_eq!(d["q50"], 2.5);
        assert_eq!(d["q75"], 3.25);
        assert_eq!(d["max"], 4.0);

        // 4. List tables → the table + row_count appear.
        let resp = rpc_with(
            store.clone(),
            &call(13, "sql_list_tables", serde_json::json!({})),
        )
        .await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let tables: Value = serde_json::from_str(text).unwrap();
        let arr = tables.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["table"], "metrics");
        assert_eq!(arr[0]["row_count"], 5);
        assert_eq!(
            arr[0]["source"].as_str().unwrap(),
            f.path().display().to_string()
        );
    }

    /// A full-result query (cap above the row count) is NOT truncated, and the
    /// default cap is applied when `row_cap` is omitted.
    #[tokio::test]
    async fn sql_query_default_cap_and_untruncated() {
        let store = Arc::new(SqliteBackend::open_in_memory().unwrap());
        let f = csv_file(FIXTURE_CSV);
        rpc_with(
            store.clone(),
            &call(
                20,
                "sql_ingest_csv",
                serde_json::json!({ "path": f.path().to_str().unwrap(), "table": "metrics" }),
            ),
        )
        .await;

        // No row_cap → default 1000, well above 5 rows → not truncated.
        let resp = rpc_with(
            store,
            &call(
                21,
                "sql_query",
                serde_json::json!({ "sql": "SELECT * FROM metrics" }),
            ),
        )
        .await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let result: Value = serde_json::from_str(text).unwrap();
        assert_eq!(result["returned"], 5);
        assert_eq!(result["truncated"], false);
    }

    // ── error cases: all in-band (isError), resp["error"] null ──────────────

    #[tokio::test]
    async fn sql_query_bad_sql_is_in_band_error() {
        let resp = rpc(&call(
            30,
            "sql_query",
            serde_json::json!({ "sql": "SELECT FROM WHERE oops" }),
        ))
        .await;
        assert!(
            resp["error"].is_null(),
            "must be in-band, not transport: {resp}"
        );
        assert_eq!(resp["result"]["isError"], true);
        assert!(!resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn sql_summarize_missing_table_is_in_band_error() {
        let resp = rpc(&call(
            31,
            "sql_summarize",
            serde_json::json!({ "table": "no_such_table" }),
        ))
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no such table"));
    }

    #[tokio::test]
    async fn sql_ingest_csv_missing_table_arg_is_in_band_error() {
        let f = csv_file(FIXTURE_CSV);
        let resp = rpc(&call(
            32,
            "sql_ingest_csv",
            serde_json::json!({ "path": f.path().to_str().unwrap() }),
        ))
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("missing required argument: table"));
    }

    #[tokio::test]
    async fn sql_query_missing_sql_arg_is_in_band_error() {
        let resp = rpc(&call(33, "sql_query", serde_json::json!({}))).await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("missing required argument: sql"));
    }

    #[tokio::test]
    async fn sql_ingest_csv_missing_path_arg_is_in_band_error() {
        let resp = rpc(&call(
            34,
            "sql_ingest_csv",
            serde_json::json!({ "table": "metrics" }),
        ))
        .await;
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("missing required argument: path"));
    }

    #[tokio::test]
    async fn sql_summarize_missing_table_arg_is_in_band_error() {
        let resp = rpc(&call(35, "sql_summarize", serde_json::json!({}))).await;
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("missing required argument: table"));
    }

    /// An unknown tool name stays a `-32603` transport fault, matching the
    /// `other =>` arm of `newt-mcp-server`'s `tools/call` dispatch.
    #[tokio::test]
    async fn unknown_tool_returns_transport_error() {
        let resp = rpc(&call(40, "nonexistent_tool", serde_json::json!({}))).await;
        assert!(
            resp["error"].is_object(),
            "expected transport error: {resp}"
        );
        assert_eq!(resp["error"]["code"], -32603);
        assert!(resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }

    // ── row_cap parsing — unit tests ────────────────────────────────────────

    #[test]
    fn parse_row_cap_defaults_and_clamps() {
        assert_eq!(parse_row_cap(None), DEFAULT_ROW_CAP);
        assert_eq!(
            parse_row_cap(Some(&serde_json::json!(null))),
            DEFAULT_ROW_CAP
        );
        assert_eq!(parse_row_cap(Some(&serde_json::json!(0))), 0);
        assert_eq!(parse_row_cap(Some(&serde_json::json!(50))), 50);
        // Negative → treated as the default (a negative cap is meaningless).
        assert_eq!(parse_row_cap(Some(&serde_json::json!(-5))), DEFAULT_ROW_CAP);
        // Mistyped caps a model occasionally emits — a float, a stringified
        // number, or a bool — must fall back to the safe default, NOT to an
        // unbounded read. This is the load-bearing case: returning usize::MAX
        // here would silently defeat the honest `truncated` contract.
        assert_eq!(
            parse_row_cap(Some(&serde_json::json!(2.5))),
            DEFAULT_ROW_CAP
        );
        assert_eq!(
            parse_row_cap(Some(&serde_json::json!("100"))),
            DEFAULT_ROW_CAP
        );
        assert_eq!(
            parse_row_cap(Some(&serde_json::json!(true))),
            DEFAULT_ROW_CAP
        );
        // A huge u64 beyond i64 range is still accepted as-is (saturates to
        // usize::MAX on a 64-bit target); the engine caps the actual read and
        // sets the honest `truncated` flag.
        assert_eq!(
            parse_row_cap(Some(&serde_json::json!(u64::MAX))),
            usize::MAX
        );
    }

    #[test]
    fn required_str_present_and_absent() {
        let args = serde_json::json!({ "a": "x" });
        assert_eq!(required_str(&args, "a").unwrap(), "x");
        let err = required_str(&args, "b").unwrap_err();
        assert_eq!(err["isError"], true);
        assert!(err["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("missing required argument: b"));
    }

    // ── live-kernel tools (21.3) — driven by a MockKernel ────────────────────

    use newt_data::kernel::{CellRun, DisplayItem, ImageOutput, KernelError};

    /// A canned CellRun with stdout, a text result, a PNG image, and an
    /// execution_count — the happy path `run_cell` must summarize.
    fn canned_run() -> CellRun {
        CellRun {
            stdout: "hello from kernel\n".into(),
            stderr: String::new(),
            results: vec![DisplayItem {
                mime: "text/plain".into(),
                text: "42".into(),
            }],
            images: vec![ImageOutput {
                path: std::path::PathBuf::from("/ws/.newt-data/plots/cell-5-abc.png"),
                mime: "image/png".into(),
                width: Some(640),
                height: Some(480),
            }],
            error: None,
            execution_count: Some(5),
        }
    }

    /// `run_cell` with no kernel attached is an in-band error telling the model
    /// to call `kernel_attach` first — never a transport fault.
    #[tokio::test]
    async fn run_cell_without_attach_is_in_band_error() {
        let resp = rpc(&call(50, "run_cell", serde_json::json!({ "code": "1+1" }))).await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("kernel_attach"));
    }

    /// `run_cell` against an attached MockKernel returns the CellRun summary:
    /// stdout, text results, and the PNG reported as a path + honest size string
    /// — never the image bytes inlined.
    #[tokio::test]
    async fn run_cell_summarizes_run_with_png_path_not_bytes() {
        let store = Arc::new(SqliteBackend::open_in_memory().unwrap());
        let session = session_with(canned_run());
        let resp = rpc_full(
            store,
            session,
            &call(51, "run_cell", serde_json::json!({ "code": "plt.show()" })),
        )
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert!(
            resp["result"]["isError"].is_null(),
            "should succeed: {resp}"
        );

        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let summary: Value = serde_json::from_str(text).unwrap();
        assert_eq!(summary["stdout"], "hello from kernel\n");
        assert_eq!(summary["execution_count"], 5);
        assert_eq!(summary["results"][0]["mime"], "text/plain");
        assert_eq!(summary["results"][0]["text"], "42");
        // The image is a path + size summary — and the raw bytes never appear.
        let img = &summary["images"][0];
        assert_eq!(img["path"], "/ws/.newt-data/plots/cell-5-abc.png");
        assert_eq!(
            img["summary"],
            "640x480 PNG saved: /ws/.newt-data/plots/cell-5-abc.png"
        );
        assert!(
            !text.contains("image/png"),
            "must not inline the PNG bytes/mime blob"
        );
        assert!(summary["error"].is_null());
    }

    /// A cell that *raised* is a successful run whose `error` field is populated
    /// — NOT an in-band tool error (the exception is data the model reads).
    #[tokio::test]
    async fn run_cell_with_cell_exception_is_success_with_error_field() {
        let run = CellRun {
            error: Some(KernelError {
                ename: "NameError".into(),
                evalue: "name 'foo' is not defined".into(),
                traceback: vec!["Traceback...".into()],
            }),
            ..Default::default()
        };
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session_with(run),
            &call(52, "run_cell", serde_json::json!({ "code": "foo" })),
        )
        .await;
        // The MCP call itself succeeded (no isError); the exception is in the body.
        assert!(
            resp["result"]["isError"].is_null(),
            "cell raise is not a tool error: {resp}"
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let summary: Value = serde_json::from_str(text).unwrap();
        assert_eq!(summary["error"]["ename"], "NameError");
        assert_eq!(summary["error"]["evalue"], "name 'foo' is not defined");
    }

    /// A transport failure inside `run_cell` (kernel died, socket dropped) is an
    /// in-band tool error, never a -32603.
    #[tokio::test]
    async fn run_cell_transport_failure_is_in_band_error() {
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session_failing("websocket closed unexpectedly"),
            &call(53, "run_cell", serde_json::json!({ "code": "1+1" })),
        )
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("run_cell failed"));
    }

    /// `run_cell` with a missing `code` argument is an in-band error.
    #[tokio::test]
    async fn run_cell_missing_code_arg_is_in_band_error() {
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session_with(canned_run()),
            &call(54, "run_cell", serde_json::json!({})),
        )
        .await;
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("missing required argument: code"));
    }

    /// `kernel_attach` with a missing `url` argument is an in-band error.
    #[tokio::test]
    async fn kernel_attach_missing_url_is_in_band_error() {
        let resp = rpc(&call(55, "kernel_attach", serde_json::json!({}))).await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("missing required argument: url"));
    }

    /// `kernel_attach` to an unreachable server is an in-band error (not a
    /// transport fault) the model can read and recover from.
    #[tokio::test]
    async fn kernel_attach_unreachable_server_is_in_band_error() {
        // Port 1 on localhost: nothing listens → connect() fails.
        let resp = rpc(&call(
            56,
            "kernel_attach",
            serde_json::json!({ "url": "http://127.0.0.1:1" }),
        ))
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("kernel_attach failed"));
    }

    /// The honest run summary: dimensions absent → no "WxH" prefix; the bytes are
    /// never present. A direct unit test of [`run_summary`].
    #[test]
    fn run_summary_reports_paths_and_honest_sizes() {
        let run = CellRun {
            images: vec![
                ImageOutput {
                    path: std::path::PathBuf::from("/p/a.png"),
                    mime: "image/png".into(),
                    width: Some(800),
                    height: Some(600),
                },
                ImageOutput {
                    path: std::path::PathBuf::from("/p/b.png"),
                    mime: "image/png".into(),
                    width: None,
                    height: None,
                },
            ],
            ..Default::default()
        };
        let summary = run_summary(&run, None);
        assert_eq!(
            summary["images"][0]["summary"],
            "800x600 PNG saved: /p/a.png"
        );
        // No dimensions → no size prefix, just the path.
        assert_eq!(summary["images"][1]["summary"], "PNG saved: /p/b.png");
        // No persist requested → persisted is null.
        assert!(summary["persisted"].is_null());
    }

    // ── notebook tools (Phase 21.4) — over a tempfile .ipynb ─────────────────

    /// A throwaway `.ipynb` path under a fresh tempdir. Returns the dir (kept
    /// alive by the caller) and the path string.
    fn nb_path() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.ipynb").display().to_string();
        (dir, path)
    }

    /// notebook_insert_cell proposes a cell, notebook_read shows it, and
    /// notebook_persist_executed_cell appends a cell with outputs — the three
    /// 21.4 tools over one tempfile notebook.
    #[tokio::test]
    async fn notebook_insert_read_persist_round_trip() {
        let (_dir, path) = nb_path();

        // 1. Insert a code cell (creates the notebook). Returns inserted_index 0.
        let resp = rpc(&call(
            60,
            "notebook_insert_cell",
            serde_json::json!({ "path": path, "source": "import pandas as pd" }),
        ))
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert!(
            resp["result"]["isError"].is_null(),
            "should succeed: {resp}"
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let out: Value = serde_json::from_str(text).unwrap();
        assert_eq!(out["inserted_index"], 0);

        // 2. Read it back — one cell, code, the source we inserted, no outputs.
        let resp = rpc(&call(
            61,
            "notebook_read",
            serde_json::json!({ "path": path }),
        ))
        .await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let cells: Value = serde_json::from_str(text).unwrap();
        let cells = cells.as_array().unwrap();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0]["cell_type"], "code");
        assert_eq!(cells[0]["source"], "import pandas as pd");
        assert_eq!(cells[0]["has_output"], false);

        // 3. Persist an executed cell with nbformat outputs → appended at index 1.
        let resp = rpc(&call(
            62,
            "notebook_persist_executed_cell",
            serde_json::json!({
                "path": path,
                "source": "df.head()",
                "outputs": [
                    { "output_type": "stream", "name": "stdout", "text": "ok\n" }
                ]
            }),
        ))
        .await;
        assert!(
            resp["result"]["isError"].is_null(),
            "should succeed: {resp}"
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let out: Value = serde_json::from_str(text).unwrap();
        assert_eq!(out["appended_index"], 1);

        // 4. Read again — two cells; the appended one carries an output.
        let resp = rpc(&call(
            63,
            "notebook_read",
            serde_json::json!({ "path": path }),
        ))
        .await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let cells: Value = serde_json::from_str(text).unwrap();
        let cells = cells.as_array().unwrap();
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[1]["source"], "df.head()");
        assert_eq!(cells[1]["has_output"], true);
    }

    /// notebook_read on a missing file is an in-band error (not a transport fault).
    #[tokio::test]
    async fn notebook_read_missing_file_is_in_band_error() {
        let (_dir, path) = nb_path(); // path does not exist yet
        let resp = rpc(&call(
            64,
            "notebook_read",
            serde_json::json!({ "path": path }),
        ))
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert_eq!(resp["result"]["isError"], true);
    }

    /// notebook_read on a corrupt notebook is an in-band "invalid notebook" error.
    #[tokio::test]
    async fn notebook_read_corrupt_file_is_in_band_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.ipynb");
        std::fs::write(&path, "{ not json").unwrap();
        let resp = rpc(&call(
            65,
            "notebook_read",
            serde_json::json!({ "path": path.display().to_string() }),
        ))
        .await;
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("invalid notebook"));
    }

    /// notebook_insert_cell with a missing `source` is an in-band error.
    #[tokio::test]
    async fn notebook_insert_missing_source_is_in_band_error() {
        let (_dir, path) = nb_path();
        let resp = rpc(&call(
            66,
            "notebook_insert_cell",
            serde_json::json!({ "path": path }),
        ))
        .await;
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("missing required argument: source"));
    }

    /// notebook_persist_executed_cell with a missing `outputs` array is an
    /// in-band error.
    #[tokio::test]
    async fn notebook_persist_missing_outputs_is_in_band_error() {
        let (_dir, path) = nb_path();
        let resp = rpc(&call(
            67,
            "notebook_persist_executed_cell",
            serde_json::json!({ "path": path, "source": "x = 1" }),
        ))
        .await;
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("missing required argument: outputs"));
    }

    /// run_cell(persist_to=…) against an attached MockKernel appends the executed
    /// cell — source + converted nbformat outputs, INCLUDING an image
    /// `display_data` whose base64 is re-read from the on-disk PNG — to the
    /// `.ipynb`, and reports the persisted path + index in the run summary.
    #[tokio::test]
    async fn run_cell_persist_to_appends_executed_cell_with_image_to_notebook() {
        let dir = tempfile::tempdir().unwrap();
        // A real PNG on disk that the ImageOutput points at, so the conversion
        // re-reads and base64-encodes its exact bytes into the notebook.
        let png_path = dir.path().join("cell-5-plot.png");
        let raw_png = vec![0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        std::fs::write(&png_path, &raw_png).unwrap();
        let nb = dir.path().join("session.ipynb").display().to_string();

        let run = CellRun {
            stdout: "plotting\n".into(),
            stderr: String::new(),
            results: vec![DisplayItem {
                mime: "text/plain".into(),
                text: "<Figure>".into(),
            }],
            images: vec![ImageOutput {
                path: png_path,
                mime: "image/png".into(),
                width: Some(640),
                height: Some(480),
            }],
            error: None,
            execution_count: Some(5),
        };

        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session_with(run),
            &call(
                70,
                "run_cell",
                serde_json::json!({
                    "code": "plt.plot([1,2,3]); plt.show()",
                    "persist_to": nb,
                }),
            ),
        )
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert!(
            resp["result"]["isError"].is_null(),
            "should succeed: {resp}"
        );

        // The run summary reports the persist: path + appended index 0.
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let summary: Value = serde_json::from_str(text).unwrap();
        assert_eq!(summary["persisted"]["path"], nb);
        assert_eq!(summary["persisted"]["index"], 0);
        assert!(
            summary["persisted"]["error"].is_null(),
            "persist must have succeeded: {summary}"
        );

        // The notebook on disk now holds the executed cell: source + outputs.
        let raw: Value = serde_json::from_slice(&std::fs::read(&nb).unwrap()).unwrap();
        let cell = &raw["cells"][0];
        assert_eq!(cell["cell_type"], "code");
        assert_eq!(cell["source"], "plt.plot([1,2,3]); plt.show()");
        let outputs = cell["outputs"].as_array().unwrap();
        // stream(stdout), execute_result(text/plain), display_data(image/png).
        assert_eq!(outputs.len(), 3);
        assert_eq!(outputs[0]["output_type"], "stream");
        assert_eq!(outputs[0]["text"], "plotting\n");
        assert_eq!(outputs[1]["output_type"], "execute_result");
        assert_eq!(outputs[1]["data"]["text/plain"], "<Figure>");
        let display = &outputs[2];
        assert_eq!(display["output_type"], "display_data");
        // The PNG was re-read and base64-encoded so the notebook RENDERS it.
        let b64 = display["data"]["image/png"].as_str().unwrap();
        // Decode the stored base64 and confirm it is the exact on-disk PNG bytes.
        let decoded = base64_decode_for_test(b64);
        assert_eq!(
            decoded, raw_png,
            "the persisted notebook embeds the real plot bytes"
        );
        assert_eq!(display["metadata"]["image/png"]["width"], 640);
        assert_eq!(display["metadata"]["image/png"]["height"], 480);
    }

    /// A persist failure (an unwritable notebook path) is REPORTED in the summary
    /// but does NOT discard the run result — the cell already ran.
    #[tokio::test]
    async fn run_cell_persist_failure_does_not_discard_run() {
        // Point persist_to at a path whose parent is a FILE, so the write fails.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"i am a file").unwrap();
        let bad_nb = blocker.join("nested.ipynb").display().to_string();

        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session_with(canned_run()),
            &call(
                71,
                "run_cell",
                serde_json::json!({ "code": "1+1", "persist_to": bad_nb }),
            ),
        )
        .await;
        // The MCP call itself succeeded — the run result is intact.
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert!(
            resp["result"]["isError"].is_null(),
            "a persist failure must not turn the successful run into a tool error: {resp}"
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let summary: Value = serde_json::from_str(text).unwrap();
        // The run output is still reported …
        assert_eq!(summary["stdout"], "hello from kernel\n");
        assert_eq!(summary["execution_count"], 5);
        // … and the persist failure is reported alongside it.
        assert_eq!(summary["persisted"]["path"], bad_nb);
        assert!(
            summary["persisted"]["error"].is_string(),
            "the persist error must be reported: {summary}"
        );
        assert!(summary["persisted"]["index"].is_null());
    }

    /// run_cell WITHOUT persist_to leaves `persisted` null (no notebook touched).
    #[tokio::test]
    async fn run_cell_without_persist_to_has_null_persisted() {
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session_with(canned_run()),
            &call(72, "run_cell", serde_json::json!({ "code": "1+1" })),
        )
        .await;
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let summary: Value = serde_json::from_str(text).unwrap();
        assert!(summary["persisted"].is_null());
    }

    /// A tiny RFC 4648 base64 decoder for the test assertion above (the prod
    /// decoder lives in newt-data behind the kernel feature; this keeps the
    /// adapter test self-contained).
    fn base64_decode_for_test(s: &str) -> Vec<u8> {
        fn val(c: u8) -> u8 {
            match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => 0,
            }
        }
        let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        let mut out = Vec::new();
        for chunk in bytes.chunks(4) {
            let pad = chunk.iter().rev().take_while(|&&b| b == b'=').count();
            let b0 = val(chunk[0]);
            let b1 = val(chunk[1]);
            let b2 = if pad >= 2 { 0 } else { val(chunk[2]) };
            let b3 = if pad >= 1 { 0 } else { val(chunk[3]) };
            out.push((b0 << 2) | (b1 >> 4));
            if pad < 2 {
                out.push((b1 << 4) | (b2 >> 2));
            }
            if pad < 1 {
                out.push((b2 << 6) | b3);
            }
        }
        out
    }
}
