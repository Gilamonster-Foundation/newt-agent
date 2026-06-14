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
    ///
    /// `seen_code` records the code of the last `run_cell` so a test can assert a
    /// tool DID (or did NOT) reach the kernel — load-bearing for the
    /// dataframe-introspection injection guard, which must reject a hostile name
    /// *before* any kernel call (Phase 21.5).
    struct MockKernel {
        run: newt_data::kernel::CellRun,
        fail: Option<String>,
        seen_code: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl KernelClient for MockKernel {
        async fn run_cell(&self, code: &str) -> anyhow::Result<newt_data::kernel::CellRun> {
            self.seen_code.lock().unwrap().push(code.to_string());
            match &self.fail {
                Some(msg) => anyhow::bail!("{msg}"),
                None => Ok(self.run.clone()),
            }
        }
    }

    /// A session with a pre-attached [`MockKernel`] returning `run`.
    fn session_with(run: newt_data::kernel::CellRun) -> KernelSession {
        Arc::new(Mutex::new(Some(Box::new(MockKernel {
            run,
            fail: None,
            seen_code: Arc::new(std::sync::Mutex::new(Vec::new())),
        }) as Box<dyn KernelClient>)))
    }

    /// A session with a pre-attached [`MockKernel`] whose `run_cell` errors.
    fn session_failing(msg: &str) -> KernelSession {
        Arc::new(Mutex::new(Some(Box::new(MockKernel {
            run: newt_data::kernel::CellRun::default(),
            fail: Some(msg.to_string()),
            seen_code: Arc::new(std::sync::Mutex::new(Vec::new())),
        }) as Box<dyn KernelClient>)))
    }

    /// A session pre-attached to a [`MockKernel`] that returns `stdout` verbatim
    /// (no error), plus the shared `seen_code` log so a test can assert what code
    /// — if any — actually reached the kernel. Used by the Phase 21.5
    /// dataframe-introspection tests: program the canned JSON the snippet would
    /// print, then assert the tool parses it (and, for the injection guard, that
    /// the kernel was never invoked at all).
    fn session_with_stdout(stdout: &str) -> (KernelSession, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen_code = Arc::new(std::sync::Mutex::new(Vec::new()));
        let run = newt_data::kernel::CellRun {
            stdout: stdout.to_string(),
            ..Default::default()
        };
        let session = Arc::new(Mutex::new(Some(Box::new(MockKernel {
            run,
            fail: None,
            seen_code: seen_code.clone(),
        }) as Box<dyn KernelClient>)));
        (session, seen_code)
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
        // The four SQL tools (21.2), the two live-kernel tools (21.3), the three
        // notebook tools (21.4), and the two dataframe-introspection tools (21.5).
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
            "list_dataframes",
            "inspect_dataframe",
        ] {
            assert!(names.contains(&expected), "tools/list missing {expected}");
        }
        assert_eq!(tools.len(), 11, "expected exactly 11 tools, got {names:?}");

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
        // list_dataframes requires nothing; inspect_dataframe requires `name`
        // (head is optional, so it must NOT be in `required`).
        let list_df = tools
            .iter()
            .find(|t| t["name"] == "list_dataframes")
            .unwrap();
        assert!(
            list_df["inputSchema"]["required"].is_null(),
            "list_dataframes must have no required args"
        );
        let inspect = tools
            .iter()
            .find(|t| t["name"] == "inspect_dataframe")
            .unwrap();
        assert_eq!(
            inspect["inputSchema"]["required"],
            serde_json::json!(["name"])
        );
        assert!(
            inspect["inputSchema"]["properties"]["head"].is_object(),
            "inspect_dataframe must advertise the optional head argument"
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

    // ── dataframe introspection (Phase 21.5) — driven by a MockKernel ────────
    //
    // No live kernel: the MockKernel returns the JSON the snippet *would* print,
    // so the tool's parse + in-band-error logic is exercised hermetically.

    /// `is_python_identifier` accepts plain identifiers and rejects anything that
    /// could break out of the `globals()[...]` lookup — the injection guard.
    #[test]
    fn is_python_identifier_accepts_and_rejects() {
        for ok in ["df", "_df", "df1", "my_frame", "A", "_", "X9_y"] {
            assert!(
                is_python_identifier(ok),
                "{ok} should be a valid identifier"
            );
        }
        for bad in [
            "",
            "1df",
            "df df",
            "df;import os",
            "df.attr",
            "df-1",
            "df()",
            "\"df\"",
            "df\n",
            "globals()['x']",
        ] {
            assert!(!is_python_identifier(bad), "{bad:?} must be rejected");
        }
    }

    /// `parse_head` defaults to 5 and folds every mistyped value to the default,
    /// taking a genuine non-negative integer as-is (mirrors `parse_row_cap`).
    #[test]
    fn parse_head_defaults_and_folds_mistypes() {
        assert_eq!(parse_head(None), DEFAULT_HEAD);
        assert_eq!(parse_head(Some(&serde_json::json!(null))), DEFAULT_HEAD);
        assert_eq!(parse_head(Some(&serde_json::json!(0))), 0);
        assert_eq!(parse_head(Some(&serde_json::json!(10))), 10);
        assert_eq!(parse_head(Some(&serde_json::json!(-3))), DEFAULT_HEAD);
        assert_eq!(parse_head(Some(&serde_json::json!(2.5))), DEFAULT_HEAD);
        assert_eq!(parse_head(Some(&serde_json::json!("7"))), DEFAULT_HEAD);
        assert_eq!(parse_head(Some(&serde_json::json!(true))), DEFAULT_HEAD);
    }

    /// `inspect_snippet` interpolates only a validated identifier and a decimal
    /// head, and references neither outside `globals()` — a quick guard that the
    /// crafted snippet has the shape the kernel test relies on.
    #[test]
    fn inspect_snippet_interpolates_name_and_head() {
        let snippet = inspect_snippet("sales", 12);
        assert!(snippet.contains(r#"globals().get("sales")"#));
        assert!(snippet.contains(".head(12)"));
        assert!(snippet.contains(r#""error": "no DataFrame named sales""#));
        // It imports pandas + json inside itself (defensive) and never mutates.
        assert!(snippet.contains("import pandas as _pd"));
        assert!(snippet.contains("import json as _json"));
        // It runs the payload through the strict-JSON sanitizer so a NaN in a
        // numeric column (null → NaN) cannot emit the invalid `NaN` token.
        assert!(snippet.contains("def _clean("));
        assert!(snippet.contains("_json.dumps(_clean("));
    }

    /// `list_dataframes` parses a canned 2-DataFrame JSON stdout into the tool
    /// result (the happy path) and reaches the kernel exactly once.
    #[tokio::test]
    async fn list_dataframes_parses_canned_json() {
        let canned = r#"[{"name":"df","rows":100,"cols":4,"memory_bytes":3200},{"name":"sales","rows":7,"cols":3,"memory_bytes":840}]"#;
        let (session, seen) = session_with_stdout(canned);
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session,
            &call(80, "list_dataframes", serde_json::json!({})),
        )
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert!(
            resp["result"]["isError"].is_null(),
            "should succeed: {resp}"
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let frames: Value = serde_json::from_str(text).unwrap();
        let arr = frames.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["name"], "df");
        assert_eq!(arr[0]["rows"], 100);
        assert_eq!(arr[1]["name"], "sales");
        assert_eq!(arr[1]["memory_bytes"], 840);
        // The list snippet ran once on the kernel.
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    /// stdout with incidental prints above the JSON line still parses — the
    /// parser takes the LAST non-empty line.
    #[tokio::test]
    async fn list_dataframes_takes_last_non_empty_stdout_line() {
        let noisy = "some incidental print\n\n[{\"name\":\"df\",\"rows\":1,\"cols\":1,\"memory_bytes\":8}]\n";
        let (session, _seen) = session_with_stdout(noisy);
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session,
            &call(81, "list_dataframes", serde_json::json!({})),
        )
        .await;
        assert!(
            resp["result"]["isError"].is_null(),
            "should succeed: {resp}"
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let frames: Value = serde_json::from_str(text).unwrap();
        assert_eq!(frames.as_array().unwrap()[0]["name"], "df");
    }

    /// `inspect_dataframe` parses a canned columns/dtypes/describe/head JSON.
    #[tokio::test]
    async fn inspect_dataframe_parses_canned_json() {
        let canned = r#"{"name":"df","shape":[5,2],"columns":[{"name":"id","dtype":"int64","null_count":0},{"name":"score","dtype":"float64","null_count":1}],"head":[{"id":1,"score":1.0},{"id":2,"score":2.0}],"describe":{"id":{"count":5.0,"mean":3.0},"score":{"count":4.0,"mean":2.5}}}"#;
        let (session, seen) = session_with_stdout(canned);
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session,
            &call(82, "inspect_dataframe", serde_json::json!({ "name": "df" })),
        )
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert!(
            resp["result"]["isError"].is_null(),
            "should succeed: {resp}"
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let inspect: Value = serde_json::from_str(text).unwrap();
        assert_eq!(inspect["name"], "df");
        assert_eq!(inspect["shape"], serde_json::json!([5, 2]));
        assert_eq!(inspect["columns"][1]["name"], "score");
        assert_eq!(inspect["columns"][1]["dtype"], "float64");
        assert_eq!(inspect["columns"][1]["null_count"], 1);
        assert_eq!(inspect["head"].as_array().unwrap().len(), 2);
        assert_eq!(inspect["describe"]["score"]["mean"], 2.5);
        // The inspect snippet ran once, with the default head of 5.
        let code = &seen.lock().unwrap()[0];
        assert!(code.contains(".head(5)"), "default head must be 5: {code}");
        assert!(code.contains(r#"globals().get("df")"#));
    }

    /// `inspect_dataframe` with an explicit `head` interpolates that N into the
    /// snippet the kernel runs.
    #[tokio::test]
    async fn inspect_dataframe_honors_explicit_head() {
        let (session, seen) = session_with_stdout(
            r#"{"name":"df","shape":[0,0],"columns":[],"head":[],"describe":{}}"#,
        );
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session,
            &call(
                83,
                "inspect_dataframe",
                serde_json::json!({ "name": "df", "head": 20 }),
            ),
        )
        .await;
        assert!(
            resp["result"]["isError"].is_null(),
            "should succeed: {resp}"
        );
        assert!(seen.lock().unwrap()[0].contains(".head(20)"));
    }

    /// No kernel attached → an in-band error telling the model to attach first
    /// (resp["error"] null, result isError true). For both tools.
    #[tokio::test]
    async fn dataframe_tools_without_attach_are_in_band_errors() {
        for (id, name, args) in [
            (84, "list_dataframes", serde_json::json!({})),
            (85, "inspect_dataframe", serde_json::json!({ "name": "df" })),
        ] {
            let resp = rpc(&call(id, name, args)).await;
            assert!(resp["error"].is_null(), "{name} must be in-band: {resp}");
            assert_eq!(resp["result"]["isError"], true, "{name}: {resp}");
            assert!(resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("attach a kernel first"));
        }
    }

    /// A snippet that prints `{"error": ...}` (e.g. pandas not importable, or an
    /// undefined DataFrame name) is surfaced as an in-band tool error carrying
    /// that message.
    #[tokio::test]
    async fn dataframe_tool_snippet_error_is_in_band() {
        let (session, _seen) = session_with_stdout(r#"{"error": "no DataFrame named ghost"}"#);
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session,
            &call(
                86,
                "inspect_dataframe",
                serde_json::json!({ "name": "ghost" }),
            ),
        )
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no DataFrame named ghost"));
    }

    /// A CellRun carrying a kernel error (the snippet itself somehow raised, or
    /// the kernel faulted) is surfaced in-band, ename:evalue included.
    #[tokio::test]
    async fn dataframe_tool_kernel_error_is_in_band() {
        let run = CellRun {
            error: Some(KernelError {
                ename: "RuntimeError".into(),
                evalue: "kernel exploded".into(),
                traceback: vec!["Traceback...".into()],
            }),
            ..Default::default()
        };
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session_with(run),
            &call(87, "list_dataframes", serde_json::json!({})),
        )
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert_eq!(resp["result"]["isError"], true);
        let msg = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(msg.contains("kernel error"));
        assert!(msg.contains("RuntimeError"));
        assert!(msg.contains("kernel exploded"));
    }

    /// A transport failure inside the snippet run is an in-band error.
    #[tokio::test]
    async fn dataframe_tool_transport_failure_is_in_band() {
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session_failing("websocket closed unexpectedly"),
            &call(88, "list_dataframes", serde_json::json!({})),
        )
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("kernel run failed"));
    }

    /// stdout with NO parseable JSON line is an honest in-band parse error.
    #[tokio::test]
    async fn dataframe_tool_unparseable_stdout_is_in_band() {
        let (session, _seen) = session_with_stdout("not json at all\nstill not json\n");
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session,
            &call(89, "list_dataframes", serde_json::json!({})),
        )
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("could not parse JSON"));
    }

    /// Empty stdout is an honest in-band "no JSON output" error.
    #[tokio::test]
    async fn dataframe_tool_empty_stdout_is_in_band() {
        let (session, _seen) = session_with_stdout("\n\n   \n");
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session,
            &call(90, "list_dataframes", serde_json::json!({})),
        )
        .await;
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("no JSON output"));
    }

    /// `inspect_dataframe` with an INVALID name (`"df; import os"`) is an in-band
    /// error BEFORE any kernel call — the injection guard. Assert the kernel was
    /// never invoked.
    #[tokio::test]
    async fn inspect_dataframe_invalid_name_rejected_before_kernel_call() {
        let (session, seen) = session_with_stdout(r#"{"name":"x"}"#);
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session,
            &call(
                91,
                "inspect_dataframe",
                serde_json::json!({ "name": "df; import os" }),
            ),
        )
        .await;
        assert!(resp["error"].is_null(), "must be in-band: {resp}");
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("must be a plain Python identifier"));
        // The load-bearing assertion: the kernel was NEVER touched — the hostile
        // name was rejected before any run_cell, so no code injection is possible.
        assert!(
            seen.lock().unwrap().is_empty(),
            "a hostile name must be rejected BEFORE any kernel call"
        );
    }

    /// `inspect_dataframe` with a missing `name` argument is an in-band error.
    #[tokio::test]
    async fn inspect_dataframe_missing_name_is_in_band_error() {
        let (session, _seen) = session_with_stdout(r#"{"name":"x"}"#);
        let resp = rpc_full(
            Arc::new(SqliteBackend::open_in_memory().unwrap()),
            session,
            &call(92, "inspect_dataframe", serde_json::json!({})),
        )
        .await;
        assert_eq!(resp["result"]["isError"], true);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("missing required argument: name"));
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
