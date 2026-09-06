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
        serde_json::json!({
            "name": "list_dataframes",
            "description": "List the pandas DataFrames currently live in the attached Jupyter kernel's global namespace (READ-ONLY — never mutates the human's session). Requires kernel_attach first. Returns, for each DataFrame, its variable name, row and column counts, and in-memory size in bytes. The Centaur sees the human's working DataFrames without touching them.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
        serde_json::json!({
            "name": "inspect_dataframe",
            "description": "Inspect one named pandas DataFrame in the attached Jupyter kernel (READ-ONLY — never mutates it). Requires kernel_attach first. Returns the shape, per-column dtype + null count, the first N rows (head, default 5), and a pandas describe() over the numeric columns. The `name` must be a plain Python identifier; an undefined name or a non-DataFrame is an in-band error.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The variable name of the DataFrame in the kernel's global namespace (a plain Python identifier)"
                    },
                    "head": {
                        "type": "integer",
                        "description": "How many leading rows to return under `head` (default 5)"
                    }
                },
                "required": ["name"]
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
                "list_dataframes" => Ok(handle_list_dataframes(&session).await),
                "inspect_dataframe" => Ok(handle_inspect_dataframe(&arguments, &session).await),
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

// ── Dataframe introspection tools (Phase 21.5) ──────────────────────────────
//
// Read-only introspection of the human's live pandas DataFrames over the
// attached kernel (docs/design/centaur-data-scientist.md §"Tool surface", the
// dataframe-introspection bullet). The Centaur sees the human's working
// DataFrames *without mutating them*: each tool runs a defensive Python snippet
// via the session `KernelClient::run_cell` that imports json + pandas inside the
// snippet, never touches the namespace, and PRINTS exactly one JSON line we
// parse from `CellRun.stdout` — robust parsing, no fragile text scraping. On a
// problem the snippet emits `{"error": "..."}` rather than raising, so the tool
// surfaces it cleanly in-band (the Centaur in-band-error discipline). The
// DataFrame `name` is validated as a plain Python identifier BEFORE it is
// interpolated into the snippet, so a hostile name can never inject code.

/// `true` iff `name` is a plain Python identifier (`[A-Za-z_][A-Za-z0-9_]*`).
///
/// The gate that makes [`inspect_snippet`] injection-proof: only a validated
/// identifier is ever interpolated into the snippet (Phase 21.5). A leading
/// digit, an embedded space, a quote, a semicolon, or any operator — anything a
/// caller might use to break out of the `globals()[...]` lookup and run
/// arbitrary code — is rejected here, before the kernel is ever touched.
fn is_python_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// The read-only snippet that enumerates pandas DataFrames in the kernel's
/// `globals()` and prints one JSON line: a list of
/// `{ name, rows, cols, memory_bytes }` (Phase 21.5).
///
/// Defensive by construction: it imports json + pandas *inside* the snippet (so
/// a kernel without pandas yields a clean `{"error": ...}` rather than a raised
/// `NameError`), iterates a snapshot of `globals().items()` so it never mutates
/// the namespace, and wraps everything in a try/except that prints
/// `{"error": ...}` instead of raising. Nothing in this snippet is
/// caller-controlled, so it carries no interpolation.
fn list_snippet() -> &'static str {
    r#"
import json as _json
try:
    import pandas as _pd
    _frames = []
    for _name, _val in list(globals().items()):
        if isinstance(_val, _pd.DataFrame):
            _frames.append({
                "name": _name,
                "rows": int(_val.shape[0]),
                "cols": int(_val.shape[1]),
                "memory_bytes": int(_val.memory_usage(deep=True).sum()),
            })
    print(_json.dumps(_frames))
except Exception as _e:
    print(_json.dumps({"error": "list_dataframes failed: " + repr(_e)}))
"#
}

/// Build the read-only snippet that inspects the DataFrame named `name`, printing
/// one JSON line: `{ name, shape:[rows,cols], columns:[{name,dtype,null_count}],
/// head:[...records...], describe:{...} }` (Phase 21.5).
///
/// `name` MUST already be a validated Python identifier ([`is_python_identifier`]);
/// the caller checks before calling, so the interpolation here is injection-safe.
/// The snippet is defensive: it imports json + pandas inside itself, looks the
/// name up in `globals()` without mutating anything, emits
/// `{"error": "no DataFrame named <name>"}` when the name is undefined or not a
/// DataFrame, guards `describe()` for an all-non-numeric frame (→ `{}`), and
/// falls back to a printed `{"error": ...}` on any other exception rather than
/// raising — `head(N)` uses `to_dict(orient="records")` so each row is a dict.
///
/// **Strict-JSON discipline.** A null in a numeric column makes `head()` /
/// `describe()` carry `NaN` (and `±inf`), which Python's `json.dumps` emits as
/// the bare tokens `NaN` / `Infinity` — **not** valid JSON, which the strict
/// `serde_json` parser on the Rust side rejects. The snippet therefore runs the
/// assembled payload through a small recursive `_clean` that maps every
/// non-finite float to `None` (→ JSON `null`) before dumping, so the printed line
/// is always strict, parseable JSON.
fn inspect_snippet(name: &str, head: usize) -> String {
    // `name` is a validated identifier and `head` is a plain usize, so the only
    // interpolated text is `[A-Za-z_][A-Za-z0-9_]*` and a decimal integer — no
    // quoting or escaping is required (and none would be safe to rely on).
    format!(
        r#"
import json as _json
import math as _math
def _clean(_o):
    if isinstance(_o, float):
        return _o if _math.isfinite(_o) else None
    if isinstance(_o, dict):
        return {{_k: _clean(_v) for _k, _v in _o.items()}}
    if isinstance(_o, (list, tuple)):
        return [_clean(_v) for _v in _o]
    return _o
try:
    import pandas as _pd
    _df = globals().get("{name}")
    if not isinstance(_df, _pd.DataFrame):
        print(_json.dumps({{"error": "no DataFrame named {name}"}}))
    else:
        _cols = [
            {{
                "name": str(_c),
                "dtype": str(_df[_c].dtype),
                "null_count": int(_df[_c].isnull().sum()),
            }}
            for _c in _df.columns
        ]
        _head = _df.head({head}).to_dict(orient="records")
        _num = _df.select_dtypes(include="number")
        _describe = {{}} if _num.shape[1] == 0 else _num.describe().to_dict()
        print(_json.dumps(_clean({{
            "name": "{name}",
            "shape": [int(_df.shape[0]), int(_df.shape[1])],
            "columns": _cols,
            "head": _head,
            "describe": _describe,
        }}), default=str))
except Exception as _e:
    print(_json.dumps({{"error": "inspect_dataframe failed: " + repr(_e)}}))
"#
    )
}

/// `list_dataframes`: run [`list_snippet`] on the attached kernel and return the
/// parsed list of live DataFrames as pretty JSON (Phase 21.5).
///
/// In-band error if no kernel is attached, if the kernel errors / the run fails,
/// or if the snippet printed an `{"error": ...}` object (e.g. pandas not
/// importable) — all surfaced via [`run_snippet_json`].
async fn handle_list_dataframes(session: &KernelSession) -> Value {
    match run_snippet_json(session, list_snippet()).await {
        Ok(value) => pretty_or_error(serde_json::to_string_pretty(&value)),
        Err(envelope) => envelope,
    }
}

/// `inspect_dataframe`: validate `name`, run [`inspect_snippet`] on the attached
/// kernel, and return the structured introspection as pretty JSON (Phase 21.5).
///
/// `name` is validated as a Python identifier *before any kernel call* — a
/// hostile name is an in-band error and the kernel is never touched. `head`
/// defaults to 5 (a missing, negative, float, or otherwise non-integer value
/// folds to the default). An undefined name, a non-DataFrame, a kernel error, or
/// a snippet `{"error": ...}` all surface in-band via [`run_snippet_json`].
async fn handle_inspect_dataframe(args: &Value, session: &KernelSession) -> Value {
    let name = match required_str(args, "name") {
        Ok(n) => n,
        Err(e) => return e,
    };
    if !is_python_identifier(name) {
        return mcp_error_content(&format!(
            "invalid DataFrame name {name:?}: must be a plain Python identifier ([A-Za-z_][A-Za-z0-9_]*)"
        ));
    }
    let head = parse_head(args.get("head"));

    match run_snippet_json(session, &inspect_snippet(name, head)).await {
        Ok(value) => pretty_or_error(serde_json::to_string_pretty(&value)),
        Err(envelope) => envelope,
    }
}

/// The default `head` for `inspect_dataframe` when the caller omits one.
const DEFAULT_HEAD: usize = 5;

/// Parse the optional `head` argument into a [`usize`], folding anything that is
/// not a representable non-negative integer (absent, null, negative, a float, a
/// string, a bool) to [`DEFAULT_HEAD`] — mirroring [`parse_row_cap`]'s defensive
/// posture for a model-supplied integer (Phase 21.5).
fn parse_head(value: Option<&Value>) -> usize {
    match value.and_then(Value::as_u64) {
        Some(n) => n as usize,
        None => DEFAULT_HEAD,
    }
}

/// Run `snippet` on the attached kernel and parse one JSON value out of its
/// stdout — the shared engine of the two dataframe-introspection tools
/// (Phase 21.5), factored out so it is unit-testable.
///
/// Returns `Ok(value)` with the parsed `serde_json::Value` on success, or
/// `Err(envelope)` where `envelope` is the ready-to-return in-band MCP tool
/// error for every failure mode:
///
/// - **no kernel attached** → "attach a kernel first" (tells the model to call
///   `kernel_attach`);
/// - **transport failure** (`run_cell` errored) → the wrapped error;
/// - **the cell raised** (`CellRun.error` set) → the `ename: evalue` surfaced;
/// - **unparseable stdout** (no JSON line) → an honest parse error;
/// - **the snippet printed `{"error": ...}`** → that message surfaced.
///
/// stdout may carry incidental prints, so the parser trims and takes the **last
/// non-empty line** (the snippet's `json.dumps` is the final thing it prints).
async fn run_snippet_json(session: &KernelSession, snippet: &str) -> Result<Value, Value> {
    let guard = session.lock().await;
    let client = match guard.as_ref() {
        Some(c) => c,
        None => {
            return Err(mcp_error_content(
                "attach a kernel first — call kernel_attach with your Jupyter server url before introspecting DataFrames",
            ))
        }
    };

    let run = match client.run_cell(snippet).await {
        Ok(run) => run,
        Err(e) => return Err(mcp_error_content(&format!("kernel run failed: {e}"))),
    };

    // A cell that raised is a kernel error here (unlike `run_cell`, where a raise
    // is data): the snippet is ours and is written never to raise, so a raise
    // means something went wrong below it — surface it in-band.
    if let Some(err) = &run.error {
        return Err(mcp_error_content(&format!(
            "kernel error: {}: {}",
            err.ename, err.evalue
        )));
    }

    // Take the last non-empty stdout line — the snippet's terminating
    // `print(json.dumps(...))` — so incidental prints above it don't break us.
    let last_line = run.stdout.lines().map(str::trim).rfind(|l| !l.is_empty());
    let json_line = match last_line {
        Some(line) => line,
        None => {
            return Err(mcp_error_content(
                "kernel produced no JSON output to parse (empty stdout)",
            ))
        }
    };

    let value: Value = match serde_json::from_str(json_line) {
        Ok(v) => v,
        Err(e) => {
            return Err(mcp_error_content(&format!(
                "could not parse JSON from kernel output: {e}"
            )))
        }
    };

    // The snippet reports its own problems (pandas missing, name undefined, …) as
    // a JSON object carrying an `error` field; surface that in-band too.
    if let Some(msg) = value.get("error").and_then(Value::as_str) {
        return Err(mcp_error_content(msg));
    }

    Ok(value)
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
#[path = "handlers_tests/mod.rs"]
mod tests;
