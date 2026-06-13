//! Transport integration tests for the Phase 21.3 live-kernel co-pilot
//! (`newt-data/src/kernel/rest.rs`), gated on the `kernel` feature.
//!
//! A live Jupyter kernel is NOT available in CI, so these stand up **mock**
//! Jupyter surfaces in-process and drive the real [`RestKernelClient`] against
//! them — covering `rest.rs` end to end without a kernel:
//!
//! - **REST discovery** ([`wiremock`]): a mock `GET /api/kernels` returns a
//!   running kernel id; [`RestKernelClient::connect`] must adopt it.
//! - **Kernel channels websocket** ([`tokio_tungstenite`] server side): a mock
//!   server accepts the channels websocket, expects one `execute_request`, and
//!   replays a canned iopub sequence (stream → execute_result → display_data with
//!   a PNG → status idle). [`RestKernelClient::run_cell`] must fold it into the
//!   right [`CellRun`] and write the PNG to disk.
//!
//! Run with: `cargo test -p newt-data --features kernel --test kernel_ws`.
#![cfg(feature = "kernel")]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use newt_data::kernel::rest::RestKernelClient;
use newt_data::kernel::KernelClient;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Base64-encode (RFC 4648) for the canned PNG fixture.
fn b64(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(A[(b0 >> 2) as usize] as char);
        out.push(A[(((b0 & 0b11) << 4) | (b1 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            A[(((b1 & 0b1111) << 2) | (b2 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            A[(b2 & 0b111111) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// The 4-byte PNG-signature prefix; the mock replays it as the plot bytes so the
/// test can assert the exact file content.
const FIXTURE_PNG: [u8; 4] = [0x89, b'P', b'N', b'G'];

/// Spin up a mock Jupyter kernel-channels websocket server on an ephemeral port.
///
/// The server accepts one connection, reads the `execute_request` (asserting its
/// shape), captures its `header.msg_id`, then replays a canned iopub sequence —
/// every reply carries `channel: "iopub"` and `parent_header.msg_id` set to the
/// request's id (so the client's parent-msg-id filter accepts them) — ending in
/// `status: idle`. Returns the bound port.
async fn spawn_mock_kernel_ws() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let png = b64(&FIXTURE_PNG);

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

        // Read the execute_request from the client.
        let first = loop {
            match ws.next().await {
                // tungstenite 0.24 `Message::Text` wraps a `String` directly.
                Some(Ok(Message::Text(t))) => break t,
                Some(Ok(_)) => continue,
                _ => return,
            }
        };
        let req: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(req["header"]["msg_type"], "execute_request");
        assert_eq!(req["channel"], "shell");
        let msg_id = req["header"]["msg_id"].as_str().unwrap().to_string();

        // A reply envelope with the right channel + parent msg_id.
        let reply = |msg_type: &str, content: Value| -> String {
            serde_json::json!({
                "channel": "iopub",
                "header": { "msg_type": msg_type },
                "parent_header": { "msg_id": msg_id },
                "content": content
            })
            .to_string()
        };

        let sequence = vec![
            reply("status", serde_json::json!({ "execution_state": "busy" })),
            reply(
                "stream",
                serde_json::json!({ "name": "stdout", "text": "hello from kernel\n" }),
            ),
            reply(
                "execute_result",
                serde_json::json!({
                    "execution_count": 5,
                    "data": { "text/plain": "42" }
                }),
            ),
            reply(
                "display_data",
                serde_json::json!({
                    "data": { "image/png": png, "text/plain": "<Figure size 640x480>" },
                    "metadata": { "image/png": { "width": 640, "height": 480 } }
                }),
            ),
            reply("status", serde_json::json!({ "execution_state": "idle" })),
        ];
        for frame in sequence {
            ws.send(Message::text(frame)).await.unwrap();
        }
        // Give the client a moment to drain, then close.
        let _ = ws.close(None).await;
    });

    port
}

/// Spin up a mock Jupyter kernel-channels websocket server that replays a
/// **truncated** iopub sequence — `busy` + one `stream`, then closes the socket
/// **without** ever sending `status: idle`. This emulates a kernel that died (or
/// a socket that dropped) mid-cell. [`RestKernelClient::run_cell`] must treat
/// this as a protocol failure (`Err`), not present the partial output as a
/// finished run (Phase 21.3).
async fn spawn_mock_kernel_ws_no_idle() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

        let first = loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => break t,
                Some(Ok(_)) => continue,
                _ => return,
            }
        };
        let req: Value = serde_json::from_str(&first).unwrap();
        let msg_id = req["header"]["msg_id"].as_str().unwrap().to_string();

        let reply = |msg_type: &str, content: Value| -> String {
            serde_json::json!({
                "channel": "iopub",
                "header": { "msg_type": msg_type },
                "parent_header": { "msg_id": msg_id },
                "content": content
            })
            .to_string()
        };

        // Note: NO terminating `status: idle`.
        let sequence = vec![
            reply("status", serde_json::json!({ "execution_state": "busy" })),
            reply(
                "stream",
                serde_json::json!({ "name": "stdout", "text": "partial output\n" }),
            ),
        ];
        for frame in sequence {
            ws.send(Message::text(frame)).await.unwrap();
        }
        // Close the socket mid-cell, before any idle.
        let _ = ws.close(None).await;
    });

    port
}

#[tokio::test]
async fn run_cell_folds_canned_iopub_sequence_and_writes_png() {
    let port = spawn_mock_kernel_ws().await;
    let plots = tempfile::tempdir().unwrap();

    // kernel_id supplied → connect() skips REST and builds the ws URL straight
    // from the base URL (which points at the mock ws server).
    let client = RestKernelClient::connect(
        &format!("http://127.0.0.1:{port}"),
        None,
        Some("kernel-xyz"),
        plots.path().join("plots"),
    )
    .await
    .expect("connect");

    let client = client.with_timeout(Duration::from_secs(10));
    let run = client
        .run_cell("print('hi'); plt.show()")
        .await
        .expect("run_cell");

    // stdout folded from the stream message.
    assert_eq!(run.stdout, "hello from kernel\n");
    assert!(run.stderr.is_empty());
    // execution_count from the execute_result.
    assert_eq!(run.execution_count, Some(5));
    // Two text/plain DisplayItems (execute_result + display_data fallback);
    // never the image/png.
    assert!(run.results.iter().all(|d| d.mime != "image/png"));
    assert!(run.results.iter().any(|d| d.text == "42"));
    // The PNG was decoded to disk with the exact fixture bytes, recorded as an
    // ImageOutput with size from metadata, never inlined.
    assert_eq!(run.images.len(), 1);
    let img = &run.images[0];
    assert_eq!(img.mime, "image/png");
    assert_eq!(img.width, Some(640));
    assert_eq!(img.height, Some(480));
    assert!(img.path.exists(), "PNG should be written to disk");
    assert_eq!(std::fs::read(&img.path).unwrap(), FIXTURE_PNG);
    // The cell did not raise.
    assert!(!run.failed());
}

#[tokio::test]
async fn run_cell_errors_when_socket_closes_before_idle() {
    // A kernel that drops the channels socket mid-cell (no terminating
    // `status: idle`) is a protocol failure: run_cell must return Err so the
    // MCP handler maps it to an in-band isError, rather than presenting the
    // truncated output as a finished run (Phase 21.3 in-band-error contract).
    let port = spawn_mock_kernel_ws_no_idle().await;
    let plots = tempfile::tempdir().unwrap();

    let client = RestKernelClient::connect(
        &format!("http://127.0.0.1:{port}"),
        None,
        Some("kernel-xyz"),
        plots.path().join("plots"),
    )
    .await
    .expect("connect")
    .with_timeout(Duration::from_secs(10));

    let result = client.run_cell("print('hi')").await;
    let err = match result {
        Ok(run) => panic!("expected Err on a socket that closed before idle, got {run:?}"),
        Err(e) => e,
    };
    // The error must name the truncation cause, not look like a timeout.
    let msg = err.to_string();
    assert!(
        msg.contains("closed before") || msg.contains("truncated"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn connect_discovers_running_kernel_over_rest() {
    // wiremock stands in for the Jupyter Server REST API: GET /api/kernels
    // returns one running kernel. connect() must adopt its id.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/kernels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "id": "running-kernel-1", "name": "python3" }
        ])))
        .mount(&server)
        .await;

    let client = RestKernelClient::connect(
        &server.uri(),
        Some("a-token"),
        None,
        std::env::temp_dir().join("newt-data-test-plots"),
    )
    .await
    .expect("connect via REST discovery");

    assert_eq!(client.kernel_id(), "running-kernel-1");
    assert_eq!(client.base_url(), server.uri().trim_end_matches('/'));
}

#[tokio::test]
async fn connect_starts_kernel_when_none_running() {
    // GET /api/kernels returns an empty list → connect() POSTs to start one and
    // adopts the returned id.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/kernels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/kernels"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": "freshly-started", "name": "python3"
        })))
        .mount(&server)
        .await;

    let client = RestKernelClient::connect(
        &server.uri(),
        None,
        None,
        std::env::temp_dir().join("newt-data-test-plots"),
    )
    .await
    .expect("connect should start a kernel");

    assert_eq!(client.kernel_id(), "freshly-started");
}

#[tokio::test]
async fn connect_reports_rest_auth_failure() {
    // A 403 on GET /api/kernels (wrong token) must surface as an Err connect()
    // can report in-band.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/kernels"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;

    // `RestKernelClient` is intentionally not `Debug` (it would leak the token),
    // so match on the Result rather than `unwrap_err`.
    let result =
        RestKernelClient::connect(&server.uri(), Some("bad"), None, std::env::temp_dir()).await;
    let err = match result {
        Ok(_) => panic!("expected a 403 to fail connect()"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("403") || err.to_string().contains("GET"),
        "unexpected error: {err}"
    );
}

/// A real-kernel smoke test, `#[ignore]` by default so CI never needs a live
/// Jupyter server and the coverage gate never depends on one. Run it by hand
/// against a running JupyterLab:
///
/// ```text
/// JUPYTER_URL=http://127.0.0.1:8888 JUPYTER_TOKEN=<tok> \
///   cargo test -p newt-data --features kernel --test kernel_ws \
///   -- --ignored run_cell_against_real_kernel
/// ```
///
/// It runs a trivial arithmetic cell and asserts the kernel echoed `3`.
#[tokio::test]
#[ignore = "requires a live Jupyter server; set JUPYTER_URL (and optionally JUPYTER_TOKEN)"]
async fn run_cell_against_real_kernel() {
    let url = std::env::var("JUPYTER_URL").expect("set JUPYTER_URL for the live-kernel test");
    let token = std::env::var("JUPYTER_TOKEN").ok();
    let plots = tempfile::tempdir().unwrap();

    let client =
        RestKernelClient::connect(&url, token.as_deref(), None, plots.path().to_path_buf())
            .await
            .expect("connect to live Jupyter");
    let run = client.run_cell("print(1 + 2)").await.expect("run_cell");
    assert_eq!(run.stdout.trim(), "3", "unexpected kernel output: {run:?}");
    assert!(!run.failed());
}
