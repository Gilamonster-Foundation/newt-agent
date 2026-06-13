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
            "description": "Run a code cell on the attached Jupyter kernel (call kernel_attach first). Returns stdout/stderr, rich text results, and any error. PNG plots are written to <data-dir>/.newt-data/plots/ and reported as a file path + honest size summary — never inlined. The exact code is shown in chat before it runs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "code": {
                        "type": "string",
                        "description": "The Python (or kernel-language) code to execute in one cell"
                    }
                },
                "required": ["code"]
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
async fn handle_run_cell(args: &Value, session: &KernelSession) -> Value {
    let code = match required_str(args, "code") {
        Ok(c) => c,
        Err(e) => return e,
    };

    let guard = session.lock().await;
    let client = match guard.as_ref() {
        Some(c) => c,
        None => {
            return mcp_error_content(
                "no kernel attached — call kernel_attach with your Jupyter server url first",
            )
        }
    };

    match client.run_cell(code).await {
        Ok(run) => pretty_or_error(serde_json::to_string(&run_summary(&run))),
        Err(e) => mcp_error_content(&format!("run_cell failed: {e}")),
    }
}

/// Build the honest, model-facing JSON summary of a [`CellRun`](newt_data::kernel::CellRun).
///
/// Images are reported as `{ path, summary }` where `summary` is a human string
/// like `"640x480 PNG saved: <path>"` (size omitted when the kernel did not
/// report dimensions) — the bytes are **never** inlined (Centaur principle; rich
/// render is gilamonster's job). stdout/stderr/results/error/execution_count are
/// passed through faithfully.
fn run_summary(run: &newt_data::kernel::CellRun) -> Value {
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
    })
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
        // The four SQL tools (21.2) plus the two live-kernel tools (21.3).
        for expected in [
            "sql_ingest_csv",
            "sql_query",
            "sql_summarize",
            "sql_list_tables",
            "kernel_attach",
            "run_cell",
        ] {
            assert!(names.contains(&expected), "tools/list missing {expected}");
        }
        assert_eq!(tools.len(), 6, "expected exactly 6 tools, got {names:?}");

        // Every tool carries an inputSchema object.
        for tool in tools {
            assert!(
                tool["inputSchema"].is_object(),
                "tool {} missing inputSchema",
                tool["name"]
            );
        }
        // kernel_attach requires `url`; run_cell requires `code`.
        let attach = tools.iter().find(|t| t["name"] == "kernel_attach").unwrap();
        assert_eq!(
            attach["inputSchema"]["required"],
            serde_json::json!(["url"])
        );
        let run = tools.iter().find(|t| t["name"] == "run_cell").unwrap();
        assert_eq!(run["inputSchema"]["required"], serde_json::json!(["code"]));
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
        let summary = run_summary(&run);
        assert_eq!(
            summary["images"][0]["summary"],
            "800x600 PNG saved: /p/a.png"
        );
        // No dimensions → no size prefix, just the path.
        assert_eq!(summary["images"][1]["summary"], "PNG saved: /p/b.png");
    }
}
