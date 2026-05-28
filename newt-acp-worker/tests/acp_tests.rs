//! Integration tests for the ACP worker.
//!
//! Each test wires a `MockBackend` (from `tests-common`) into an
//! `AcpServer`, drives one or more JSON-RPC requests through an
//! in-memory reader/writer, and asserts on the parsed responses.

use std::sync::Arc;

use newt_acp_worker::AcpServer;
use serde_json::Value;
use tests_common::MockBackend;

/// Send a batch of JSON-RPC requests through a fresh server and return
/// the parsed responses (one per request, in order).
async fn roundtrip(
    backend: Arc<dyn newt_inference::InferenceBackend>,
    requests: &[Value],
) -> Vec<Value> {
    let server = AcpServer::new(backend);
    let mut input = String::new();
    for req in requests {
        input.push_str(&serde_json::to_string(req).unwrap());
        input.push('\n');
    }
    let mut output: Vec<u8> = Vec::new();
    server.run(input.as_bytes(), &mut output).await.unwrap();

    let response_str = String::from_utf8(output).unwrap();
    response_str
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Single-request convenience wrapper around `roundtrip`.
async fn one(backend: Arc<dyn newt_inference::InferenceBackend>, request: Value) -> Value {
    let mut responses = roundtrip(backend, &[request]).await;
    responses.pop().expect("expected one response")
}

/// Send raw bytes (used to exercise the parse-error path).
async fn roundtrip_raw(backend: Arc<dyn newt_inference::InferenceBackend>, raw: &str) -> Value {
    let server = AcpServer::new(backend);
    let mut output: Vec<u8> = Vec::new();
    server.run(raw.as_bytes(), &mut output).await.unwrap();
    let response_str = String::from_utf8(output).unwrap();
    serde_json::from_str(response_str.trim()).unwrap()
}

fn mock_backend(reply: &str) -> Arc<dyn newt_inference::InferenceBackend> {
    Arc::new(MockBackend::all_tiers("mock", reply))
}

#[tokio::test]
async fn initialize_returns_capabilities() {
    let backend = mock_backend("");
    let resp = one(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
        }),
    )
    .await;

    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["protocolVersion"], "v0.1");
    assert_eq!(resp["result"]["serverInfo"]["name"], "newt-acp-worker");
    assert_eq!(resp["result"]["capabilities"]["prompting"], true);
    assert_eq!(resp["result"]["capabilities"]["diff_capture"], true);
}

#[tokio::test]
async fn new_session_returns_session_id() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = mock_backend("");
    let resp = one(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "new_session",
            "params": { "workspace_path": tmp.path().to_str().unwrap() },
        }),
    )
    .await;

    let session_id = resp["result"]["session_id"]
        .as_str()
        .expect("session_id missing");
    // UUIDs are 36 chars: 8-4-4-4-12.
    assert_eq!(session_id.len(), 36);
    assert_eq!(session_id.as_bytes()[8], b'-');
}

#[tokio::test]
async fn new_session_rejects_nonexistent_path() {
    let backend = mock_backend("");
    let resp = one(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "new_session",
            "params": { "workspace_path": "/nonexistent/path/should/never/exist/xyz" },
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"], -32603);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("does not exist"));
}

#[tokio::test]
async fn new_session_requires_workspace_path() {
    let backend = mock_backend("");
    let resp = one(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "new_session",
            "params": {},
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"], -32603);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("workspace_path required"));
}

#[tokio::test]
async fn unknown_method_returns_error() {
    let backend = mock_backend("");
    let resp = one(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "bogus_method",
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"], -32603);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("bogus_method"));
}

#[tokio::test]
async fn malformed_json_returns_parse_error() {
    let backend = mock_backend("");
    let resp = roundtrip_raw(backend, "{{{{not json}}}}\n").await;
    assert_eq!(resp["error"]["code"], -32700);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Parse error"));
}
