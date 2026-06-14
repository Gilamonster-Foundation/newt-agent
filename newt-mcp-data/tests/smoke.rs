//! Stdio smoke test for the real `newt-mcp-data` binary (Phase 21).
//!
//! Spawns the compiled server with `NEWT_DATA_DB` pointed at a throwaway
//! tempfile (so it never touches the real `~/.newt-data` home), drives two
//! newline-delimited JSON-RPC requests (`initialize`, then `tools/list`) over
//! stdin, closes stdin, and parses the two newline-delimited response lines from
//! stdout. Asserts the `serverInfo.name` is `newt-mcp-data` and that all six
//! tool names (four SQL EDA + the two Phase 21.3 live-kernel tools) are
//! advertised — the end-to-end wiring the agent relies on.
//!
//! Mirrors the newt CLI test conventions (`assert_cmd` + `predicates`).

use assert_cmd::cargo::cargo_bin;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn initialize_and_tools_list_over_stdio() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("smoke.db");

    let mut child = Command::new(cargo_bin("newt-mcp-data"))
        .env("NEWT_DATA_DB", &db)
        // Keep tracing quiet; it goes to stderr regardless, but silence it.
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn newt-mcp-data");

    // Send both requests, then close stdin so the server's read loop ends.
    {
        let mut stdin = child.stdin.take().expect("child stdin");
        let initialize = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
        });
        let tools_list = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
        });
        writeln!(stdin, "{}", serde_json::to_string(&initialize).unwrap()).unwrap();
        writeln!(stdin, "{}", serde_json::to_string(&tools_list).unwrap()).unwrap();
        // stdin dropped here → EOF.
    }

    // Read the two newline-delimited response lines from stdout.
    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);

    let mut init_line = String::new();
    reader
        .read_line(&mut init_line)
        .expect("read initialize response");
    let init: Value = serde_json::from_str(init_line.trim()).expect("parse initialize response");

    let mut list_line = String::new();
    reader
        .read_line(&mut list_line)
        .expect("read tools/list response");
    let list: Value = serde_json::from_str(list_line.trim()).expect("parse tools/list response");

    let status = child.wait().expect("wait for child");
    assert!(status.success(), "server exited with failure: {status:?}");

    // initialize → serverInfo.name = newt-mcp-data.
    assert_eq!(init["id"], 1);
    assert_eq!(
        init["result"]["serverInfo"]["name"], "newt-mcp-data",
        "unexpected serverInfo: {init}"
    );
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");

    // tools/list → the four SQL tools (21.2), the two live-kernel tools (21.3),
    // the three notebook tools (21.4), and the two dataframe-introspection tools
    // (21.5).
    assert_eq!(list["id"], 2);
    let tools = list["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
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
        assert!(
            names.contains(&expected),
            "tools/list missing {expected}: {names:?}"
        );
    }
    assert_eq!(names.len(), 11, "expected exactly 11 tools, got {names:?}");
}
