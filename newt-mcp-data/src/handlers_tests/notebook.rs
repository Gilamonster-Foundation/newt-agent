use super::*;

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
