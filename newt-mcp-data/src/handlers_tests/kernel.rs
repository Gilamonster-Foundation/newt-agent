use super::*;

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
