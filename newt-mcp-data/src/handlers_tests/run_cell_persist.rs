use super::*;

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
