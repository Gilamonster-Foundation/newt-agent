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
