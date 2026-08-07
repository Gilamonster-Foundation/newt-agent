//! Integration tests for the ACP worker.
//!
//! Each test wires a `MockBackend` (from `tests-common`) into an
//! `AcpServer`, drives one or more JSON-RPC requests through an
//! in-memory reader/writer, and asserts on the parsed responses.

use std::path::Path;
use std::sync::Arc;

use newt_acp_worker::{AcpServer, WorkerIdentity};
use serde_json::Value;
use tests_common::MockBackend;

/// `git init` + identity config so `git diff` works inside the tempdir.
///
/// We clear inherited `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE`
/// so the test stays scoped to `path` when run from inside a git hook
/// (e.g. the pre-push hook that invokes `cargo test --workspace`).
fn init_git_repo(path: &Path) {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_PREFIX")
            .output()
            .expect("git command failed")
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@test"]);
    run(&["config", "user.name", "test"]);
}

/// Commit `path/file` with the given content so subsequent edits show
/// up in `git diff`. See `init_git_repo` for the env-clearing rationale.
fn commit_initial(path: &Path, file: &str, content: &str) {
    std::fs::write(path.join(file), content).unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_PREFIX")
            .output()
            .expect("git command failed")
    };
    run(&["add", file]);
    run(&["commit", "-q", "-m", "init"]);
}

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
async fn prompt_returns_model_id() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = mock_backend("hello from mock");

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
                "method": "prompt",
                "params": { "session_id": sid, "prompt": "do something" },
            })
        },
    )
    .await;

    let result = &responses[1]["result"];
    assert_eq!(result["model_id"], "mock-model");
    assert_eq!(result["content"], "hello from mock");
    assert_eq!(result["diff_applied"], false);
}

#[tokio::test]
async fn prompt_applies_diff_when_present() {
    let tmp = tempfile::tempdir().unwrap();
    // git repo + committed baseline so the post-turn diff is non-empty.
    init_git_repo(tmp.path());
    commit_initial(tmp.path(), "hello.txt", "line1\nline2\nline3\n");

    let diff = "\
--- a/hello.txt
+++ b/hello.txt
@@ -1,3 +1,3 @@
 line1
-line2
+EDITED
 line3
";
    let backend = mock_backend(diff);

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
                "method": "prompt",
                "params": { "session_id": sid, "prompt": "edit line2" },
            })
        },
    )
    .await;

    let result = &responses[1]["result"];
    assert_eq!(result["diff_applied"], true);
    assert_eq!(result["empty_diff"], false);
    assert!(result["diff"].as_str().unwrap().contains("-line2"));
    assert!(result["diff"].as_str().unwrap().contains("+EDITED"));

    // The file on disk should now contain the patched content.
    let after = std::fs::read_to_string(tmp.path().join("hello.txt")).unwrap();
    assert_eq!(after, "line1\nEDITED\nline3\n");
}

#[tokio::test]
async fn prompt_captures_empty_diff_on_no_changes() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_initial(tmp.path(), "hello.txt", "unchanged\n");

    // Model returns prose with no diff — nothing to apply, no edits.
    let backend = mock_backend("I thought about it and decided not to change anything.");

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
                "method": "prompt",
                "params": { "session_id": sid, "prompt": "do nothing" },
            })
        },
    )
    .await;

    let result = &responses[1]["result"];
    assert_eq!(result["diff_applied"], false);
    assert_eq!(result["empty_diff"], true);
    assert_eq!(result["diff"], "");
}

#[tokio::test]
async fn prompt_non_git_workspace_reports_empty_diff() {
    // Non-git workspace: capture_diff returns "" with a tracing warn.
    // The server still completes the turn, just with empty_diff=true.
    let tmp = tempfile::tempdir().unwrap();
    let backend = mock_backend("just prose, no diff");

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
                "method": "prompt",
                "params": { "session_id": sid, "prompt": "hi" },
            })
        },
    )
    .await;

    let result = &responses[1]["result"];
    assert_eq!(result["empty_diff"], true);
    assert_eq!(result["diff"], "");
}

#[tokio::test]
async fn prompt_unknown_session_errors() {
    let backend = mock_backend("");
    let resp = one(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 20,
            "method": "prompt",
            "params": {
                "session_id": "00000000-0000-0000-0000-000000000000",
                "prompt": "hi",
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
async fn prompt_requires_session_id() {
    let backend = mock_backend("");
    let resp = one(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 21,
            "method": "prompt",
            "params": { "prompt": "hi" },
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
async fn prompt_requires_prompt() {
    let backend = mock_backend("");
    let resp = one(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 22,
            "method": "prompt",
            "params": { "session_id": "00000000-0000-0000-0000-000000000000" },
        }),
    )
    .await;

    assert_eq!(resp["error"]["code"], -32603);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("prompt required"));
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

// ── newt-coder plugin integration ──────────────────────────────────────
//
// These tests drive the `coder: true` opt-in through the full ACP loop
// and assert that the response carries the wire-stable `emission_shape`
// label so the foreman's scorecard can distinguish T0a / T0b / T0c.

#[tokio::test]
async fn new_session_with_coder_param_echoes_opt_in() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = mock_backend("");
    let resp = one(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "new_session",
            "params": {
                "workspace_path": tmp.path().to_str().unwrap(),
                "coder": true,
            },
        }),
    )
    .await;
    assert_eq!(resp["result"]["coder"], true);
    assert!(resp["result"]["session_id"].is_string());
}

#[tokio::test]
async fn coder_prompt_writes_whole_file_and_reports_shape() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_initial(tmp.path(), "lib.rs", "pub fn greet() {}\n");

    // Canned reply in the S5 shape: rename greet -> hello.
    let canned = "FILE: lib.rs\npub fn hello() {}\nEND-FILE\n";
    let backend = mock_backend(canned);

    let responses = drive_dependent(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "new_session",
            "params": {
                "workspace_path": tmp.path().to_str().unwrap(),
                "coder": true,
            },
        }),
        |first| {
            let sid = first["result"]["session_id"].as_str().unwrap().to_string();
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "prompt",
                "params": {
                    "session_id": sid,
                    "prompt": "Rename greet to hello in lib.rs",
                },
            })
        },
    )
    .await;

    let result = &responses[1]["result"];
    assert_eq!(result["emission_shape"], "whole_files");
    assert_eq!(result["model_id"], "mock-model");
    assert_eq!(result["empty_diff"], false);
    assert!(result["diff"].as_str().unwrap().contains("-pub fn greet"));
    assert!(result["diff"].as_str().unwrap().contains("+pub fn hello"));

    // The on-disk file should now carry the renamed function.
    let after = std::fs::read_to_string(tmp.path().join("lib.rs")).unwrap();
    assert!(after.contains("pub fn hello()"));
}

#[tokio::test]
async fn coder_prompt_prose_only_reports_t0a_shape() {
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_initial(tmp.path(), "lib.rs", "pub fn greet() {}\n");

    // T0a-style reply: pure prose. The workspace must stay unchanged.
    let canned = "I've updated src/lib.rs as requested.";
    let backend = mock_backend(canned);

    let responses = drive_dependent(
        backend,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "new_session",
            "params": {
                "workspace_path": tmp.path().to_str().unwrap(),
                "coder": true,
            },
        }),
        |first| {
            let sid = first["result"]["session_id"].as_str().unwrap().to_string();
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "prompt",
                "params": {
                    "session_id": sid,
                    "prompt": "Rename greet to hello in lib.rs",
                },
            })
        },
    )
    .await;

    let result = &responses[1]["result"];
    assert_eq!(result["emission_shape"], "prose");
    assert_eq!(result["empty_diff"], true);
    let after = std::fs::read_to_string(tmp.path().join("lib.rs")).unwrap();
    assert_eq!(after, "pub fn greet() {}\n");
}

// ── #94: headless dispatch derives caveats from a signed operator key ────

/// Variant of [`drive_dependent`] that lets the test inject an explicit
/// [`WorkerIdentity`] into the server. Used by the #94 regression
/// tests to assert that an operator-rooted identity dispatches without
/// falling back to `Caveats::top()`.
async fn drive_dependent_with_identity<F>(
    backend: Arc<dyn newt_inference::InferenceBackend>,
    identity: WorkerIdentity,
    first: Value,
    build_second: F,
) -> Vec<Value>
where
    F: FnOnce(&Value) -> Value + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (server_rx, mut client_tx) = tokio::io::duplex(8 * 1024);
    let (mut server_tx, client_rx) = tokio::io::duplex(8 * 1024);

    let server = AcpServer::new(backend).with_identity(identity);
    let server_task = tokio::spawn(async move { server.run(server_rx, &mut server_tx).await });

    let mut first_line = serde_json::to_string(&first).unwrap();
    first_line.push('\n');
    client_tx.write_all(first_line.as_bytes()).await.unwrap();
    client_tx.flush().await.unwrap();

    let mut reader = BufReader::new(client_rx);
    let mut first_resp_line = String::new();
    reader.read_line(&mut first_resp_line).await.unwrap();
    let first_resp: Value = serde_json::from_str(first_resp_line.trim()).unwrap();

    let second = build_second(&first_resp);
    let mut second_line = serde_json::to_string(&second).unwrap();
    second_line.push('\n');
    client_tx.write_all(second_line.as_bytes()).await.unwrap();
    client_tx.flush().await.unwrap();

    let mut second_resp_line = String::new();
    reader.read_line(&mut second_resp_line).await.unwrap();
    let second_resp: Value = serde_json::from_str(second_resp_line.trim()).unwrap();

    drop(client_tx);
    server_task.await.unwrap().unwrap();

    vec![first_resp, second_resp]
}

#[tokio::test]
async fn coder_dispatch_with_operator_identity_carries_non_top_caveats() {
    // #94 acceptance: a coder dispatch under an operator-rooted
    // `WorkerIdentity` must succeed end-to-end. The identity attenuates
    // the user's top() authority to the conservative worker policy
    // (`worker_session_caveats`) — exec=None, max_calls=AtMost(32),
    // net=Only([backend_host]). The mock backend has no endpoint, so
    // the coder's net check is vacuously satisfied; fs_read / fs_write
    // are both `All`; max_calls budget is 32. The dispatch therefore
    // lands without a `CapabilityDenied`, proving the wiring without
    // hard-coding the caveats into the wire.
    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_initial(tmp.path(), "lib.rs", "pub fn greet() {}\n");

    // Per-test operator key under a tempdir (never touches ~/.newt).
    let key_dir = tempfile::tempdir().unwrap();
    let key_path = key_dir.path().join("identity.pem");
    let identity = WorkerIdentity::from_operator_key(&key_path).unwrap();
    assert!(identity.is_operator(), "must be operator-rooted");

    // And the verified caveats are strictly narrower than `top()` —
    // pin the property at the dispatch layer.
    let resolved = identity.caveats_for_dispatch(None, None).unwrap();
    assert_ne!(
        resolved,
        newt_core::Caveats::top(),
        "operator dispatch must not pass top() (regression for #94)"
    );

    let canned = "FILE: lib.rs\npub fn hello() {}\nEND-FILE\n";
    let backend = mock_backend(canned);

    let responses = drive_dependent_with_identity(
        backend,
        identity,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "new_session",
            "params": {
                "workspace_path": tmp.path().to_str().unwrap(),
                "coder": true,
            },
        }),
        |first| {
            let sid = first["result"]["session_id"].as_str().unwrap().to_string();
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "prompt",
                "params": {
                    "session_id": sid,
                    "prompt": "Rename greet to hello in lib.rs",
                },
            })
        },
    )
    .await;

    let result = &responses[1]["result"];
    // The coder dispatched successfully under the attenuated caveats:
    // the whole-file emission applied, the diff is non-empty.
    assert_eq!(result["emission_shape"], "whole_files");
    assert_eq!(result["empty_diff"], false);
    assert!(result["diff"].as_str().unwrap().contains("+pub fn hello"));
}

#[tokio::test]
async fn worker_identity_allow_no_key_falls_back_to_top() {
    // Debug-only fallback: `--allow-no-key` (modeled by
    // `WorkerIdentity::AllowNoKey`) restores the pre-#94 `Caveats::top()`
    // dispatch. Behavior must match the legacy `AcpServer::new(...)`
    // default so developer iteration without a provisioned key keeps
    // working.
    let identity = WorkerIdentity::AllowNoKey;
    assert!(!identity.is_operator());
    assert_eq!(
        identity.caveats_for_dispatch(None, None).unwrap(),
        newt_core::Caveats::top(),
        "AllowNoKey must preserve pre-#94 top() behavior"
    );

    let tmp = tempfile::tempdir().unwrap();
    init_git_repo(tmp.path());
    commit_initial(tmp.path(), "lib.rs", "pub fn greet() {}\n");

    let canned = "FILE: lib.rs\npub fn hello() {}\nEND-FILE\n";
    let backend = mock_backend(canned);

    // Drive a coder turn under AllowNoKey and confirm it still dispatches.
    let responses = drive_dependent_with_identity(
        backend,
        identity,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "new_session",
            "params": {
                "workspace_path": tmp.path().to_str().unwrap(),
                "coder": true,
            },
        }),
        |first| {
            let sid = first["result"]["session_id"].as_str().unwrap().to_string();
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "prompt",
                "params": {
                    "session_id": sid,
                    "prompt": "Rename greet to hello in lib.rs",
                },
            })
        },
    )
    .await;
    let result = &responses[1]["result"];
    assert_eq!(result["emission_shape"], "whole_files");
}

#[tokio::test]
async fn resolve_refuses_when_path_unresolved_without_allow_no_key() {
    // The headless worker's `--allow-no-key`-less path must REFUSE to
    // start when no key can be loaded. We force the refusal by giving
    // `resolve` a bad-PEM file (a deterministic, env-free way to make
    // `load_or_generate` fail). Without the flag → Err; with it → the
    // debug AllowNoKey fallback.
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.pem");
    std::fs::write(&bad, b"not a real PEM").unwrap();

    let refused = WorkerIdentity::resolve(Some(&bad), /*allow_no_key=*/ false);
    assert!(
        refused.is_err(),
        "operator key load failure must refuse without --allow-no-key, got: {refused:?}"
    );

    let fallback =
        WorkerIdentity::resolve(Some(&bad), /*allow_no_key=*/ true).expect("must fall back");
    assert!(
        !fallback.is_operator(),
        "--allow-no-key must produce AllowNoKey on key load failure"
    );
}

#[tokio::test]
async fn flat_path_omits_emission_shape_field() {
    // The legacy newt-flat path (no `coder: true`) must not carry an
    // `emission_shape` key on the wire — downstream consumers can
    // pre-date the field.
    let tmp = tempfile::tempdir().unwrap();
    let backend = mock_backend("a plain prose reply");

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
                "method": "prompt",
                "params": { "session_id": sid, "prompt": "do a thing" },
            })
        },
    )
    .await;

    let result = &responses[1]["result"];
    assert!(
        result.get("emission_shape").is_none(),
        "newt-flat path leaked emission_shape: {result}"
    );
}

#[tokio::test]
async fn operator_identity_exposes_parent_key_for_plugin_spawn() {
    // Issue #93: `WorkerIdentity::Operator { root }` must surface the
    // operator-rooted `Arc<AgentKey>` so the ACP server can thread it
    // into `Coder::with_parent_key`. Without that exposure, subprocess
    // plugin spawn would fall back to a synthetic-key path.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("identity.pem");
    let identity = WorkerIdentity::from_operator_key(&path).unwrap();
    assert!(identity.is_operator());
    assert!(
        identity.parent_key().is_some(),
        "Operator identity MUST expose its parent_key for plugin spawn (#93)"
    );

    // And the parent_key roots at the same user key on disk.
    let user = newt_identity::load_or_generate(&path).unwrap();
    let parent = identity.parent_key().unwrap();
    let cert = parent.cert();
    cert.verify().unwrap();
    assert_eq!(cert.user_fingerprint(), user.fingerprint());
}

#[tokio::test]
async fn allow_no_key_identity_has_no_parent_key() {
    // The debug fallback has no operator key on disk, so there is no
    // parent_key to root subprocess plugins at. The Coder threading
    // path must see `None` and the consequence (per #93 design) is:
    // subprocess plugin spawn from this path also runs without an
    // envelope, NOT with a freshly-minted synthetic key. The
    // companion `no_synthetic_keys.rs` source-text scanner verifies
    // the dispatch chain doesn't reach for `UserKey::generate()` to
    // fill the gap.
    let identity = WorkerIdentity::AllowNoKey;
    assert!(!identity.is_operator());
    assert!(
        identity.parent_key().is_none(),
        "AllowNoKey has no operator key — parent_key MUST be None (#93)"
    );
}
