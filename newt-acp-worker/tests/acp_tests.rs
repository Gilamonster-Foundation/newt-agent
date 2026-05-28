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

/// Drive two sequential requests through one `run` invocation, where
/// the second request is built from the first response.
///
/// The session map only persists for the lifetime of a single `run`
/// call — so any test that needs session continuity must pipeline its
/// requests through the same loop. A duplex stream lets us write the
/// second request after observing the first reply on stdout, then
/// close the write half so `run` returns.
async fn drive_dependent<F>(
    backend: Arc<dyn newt_inference::InferenceBackend>,
    first: Value,
    build_second: F,
) -> Vec<Value>
where
    F: FnOnce(&Value) -> Value + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // server reads from server_rx, writes to server_tx
    let (server_rx, mut client_tx) = tokio::io::duplex(8 * 1024);
    let (mut server_tx, client_rx) = tokio::io::duplex(8 * 1024);

    let server = AcpServer::new(backend);
    let server_task = tokio::spawn(async move { server.run(server_rx, &mut server_tx).await });

    // Write the first request.
    let mut first_line = serde_json::to_string(&first).unwrap();
    first_line.push('\n');
    client_tx.write_all(first_line.as_bytes()).await.unwrap();
    client_tx.flush().await.unwrap();

    // Read the first response.
    let mut reader = BufReader::new(client_rx);
    let mut first_resp_line = String::new();
    reader.read_line(&mut first_resp_line).await.unwrap();
    let first_resp: Value = serde_json::from_str(first_resp_line.trim()).unwrap();

    // Build + send the second request.
    let second = build_second(&first_resp);
    let mut second_line = serde_json::to_string(&second).unwrap();
    second_line.push('\n');
    client_tx.write_all(second_line.as_bytes()).await.unwrap();
    client_tx.flush().await.unwrap();

    // Read the second response.
    let mut second_resp_line = String::new();
    reader.read_line(&mut second_resp_line).await.unwrap();
    let second_resp: Value = serde_json::from_str(second_resp_line.trim()).unwrap();

    // Close the write half so `run` returns cleanly.
    drop(client_tx);
    server_task.await.unwrap().unwrap();

    vec![first_resp, second_resp]
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
async fn set_session_model_unknown_session_errors() {
    // No prior new_session — random UUID should be rejected.
    let backend = mock_backend("");
    let resp = one(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "set_session_model",
            "params": {
                "session_id": "00000000-0000-0000-0000-000000000000",
                "model": "qwen2.5-coder:32b",
            },
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"], -32603);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown session"));
}

#[tokio::test]
async fn set_session_model_happy_path() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = mock_backend("");

    // Use the duplex driver so we can build the second request from the
    // first response (set_session_model needs the freshly-issued
    // session_id). The session map only lives for the duration of one
    // `run` call, so both requests must travel through the same loop.
    let responses = drive_dependent(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "new_session",
            "params": { "workspace_path": tmp.path().to_str().unwrap() },
        }),
        |first| {
            let sid = first["result"]["session_id"].as_str().unwrap().to_string();
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "set_session_model",
                "params": { "session_id": sid, "model": "qwen2.5-coder:32b" },
            })
        },
    )
    .await;

    assert!(responses[0]["result"]["session_id"].is_string());
    assert_eq!(responses[1]["result"]["ok"], true);
}

#[tokio::test]
async fn set_session_model_requires_session_id() {
    let backend = mock_backend("");
    let resp = one(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "set_session_model",
            "params": { "model": "qwen2.5-coder:32b" },
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"], -32603);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("session_id required"));
}

#[tokio::test]
async fn set_session_model_requires_model() {
    let backend = mock_backend("");
    let resp = one(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "set_session_model",
            "params": { "session_id": "00000000-0000-0000-0000-000000000000" },
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"], -32603);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("model required"));
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
