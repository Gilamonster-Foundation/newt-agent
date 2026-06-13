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

use std::path::Path;
use std::sync::Arc;

use newt_data::{DataStore, SqliteBackend};
use serde_json::Value;

use crate::server::McpServer;

/// The default `row_cap` for `sql_query` when the caller omits one — mirrors the
/// engine's documented small-result default. An absent, negative, or *mistyped*
/// cap (a float, a stringified number, a bool) falls back to this; a genuine
/// huge integer is accepted as-is (the engine reads at most that many rows and
/// sets the honest `truncated` flag). See [`parse_row_cap`].
const DEFAULT_ROW_CAP: usize = 1000;

/// Register the SQL EDA MCP handlers on `server`, wiring in the shared data
/// `store`.
///
/// `store` is the single piece of runtime state — an [`SqliteBackend`] opened
/// over the workspace's `.newt-data/data.db` (or an in-memory one in tests). It
/// is shared (via `Arc`) into the `tools/call` closure so each invocation gets
/// its own clone (the outer closure is `Fn`, not `FnOnce`).
pub fn register_handlers(server: &mut McpServer, store: Arc<SqliteBackend>) {
    register_initialize(server);
    register_tools_list(server);
    register_tools_call(server, store);
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
    ];

    Value::Array(tools)
}

// ── tools/call ─────────────────────────────────────────────────────────────

fn register_tools_call(server: &mut McpServer, store: Arc<SqliteBackend>) {
    server.register("tools/call", move |params| {
        // Move a clone into the async block so each invocation owns its own
        // Arc (the outer closure is `Fn`, not `FnOnce`).
        let store = store.clone();
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
                // An unknown tool name is a transport-level `-32603`, matching
                // `newt-mcp-server`'s `other =>` arm. A *known* tool failing for
                // its own reasons (bad SQL, missing arg) stays in-band, above.
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
    /// (ingest → query → summarize → list) can share one database.
    async fn rpc_with(store: Arc<SqliteBackend>, request: &Value) -> Value {
        let mut server = McpServer::new();
        register_handlers(&mut server, store);

        let input = format!("{}\n", serde_json::to_string(request).unwrap());
        let mut output: Vec<u8> = Vec::new();
        server.run(input.as_bytes(), &mut output).await.unwrap();
        let text = String::from_utf8(output).unwrap();
        serde_json::from_str(text.trim()).unwrap()
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
    async fn tools_list_returns_exactly_the_four_sql_tools() {
        let resp = rpc(&serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
        }))
        .await;

        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(
            tools.len(),
            4,
            "expected exactly 4 tools, got {}",
            tools.len()
        );

        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"sql_ingest_csv"));
        assert!(names.contains(&"sql_query"));
        assert!(names.contains(&"sql_summarize"));
        assert!(names.contains(&"sql_list_tables"));

        // Every tool carries an inputSchema object.
        for tool in tools {
            assert!(
                tool["inputSchema"].is_object(),
                "tool {} missing inputSchema",
                tool["name"]
            );
        }
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
}
