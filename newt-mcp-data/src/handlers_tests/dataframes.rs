use super::*;

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
    let noisy =
        "some incidental print\n\n[{\"name\":\"df\",\"rows\":1,\"cols\":1,\"memory_bytes\":8}]\n";
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
    let (session, seen) =
        session_with_stdout(r#"{"name":"df","shape":[0,0],"columns":[],"head":[],"describe":{}}"#);
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
