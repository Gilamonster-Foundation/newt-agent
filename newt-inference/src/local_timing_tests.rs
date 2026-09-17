use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use super::{ChatRequest, InferenceBackend, LocalVllmBackend, RetryPolicy, RESPONSE_BYTES_READ};

#[path = "../tests/support/local_stream.rs"]
mod support;
use support::request_body;

/// Grounds the idle-read timeout in real HTTP consumption: a total generation
/// may outlast the deadline while each gap between received chunks stays below it.
#[tokio::test]
async fn progressing_local_stream_resets_its_idle_timeout() {
    const FRAMES: [&[u8]; 3] = [
        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n",
        b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"still progressing\"},\"finish_reason\":\"stop\"}]}\n\n",
        b"data: [DONE]\n\n",
    ];
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (advance_tx, mut advance_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = request_body(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        socket.write_all(FRAMES[0]).await.unwrap();
        started_tx.send(request).unwrap();
        for frame in &FRAMES[1..] {
            advance_rx.recv().await.expect("test advances the stream");
            socket.write_all(frame).await.unwrap();
        }
        socket.shutdown().await.unwrap();
    });
    let backend = LocalVllmBackend::new(endpoint, "stream-fixture")
        .with_client(reqwest::Client::new())
        .with_timeout(Duration::from_secs(5))
        .with_retry_policy(RetryPolicy::immediate(0));
    let consumed = Arc::new(AtomicUsize::new(0));
    let mut completion = Box::pin(RESPONSE_BYTES_READ.scope(
        consumed.clone(),
        backend.complete(ChatRequest::new().user("keep generating")),
    ));
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            request = started_rx => request.unwrap(),
            result = &mut completion => panic!("stream ended before its first frame: {result:?}"),
        }
    })
    .await
    .expect("the generation must begin");
    assert_eq!(request["stream"], true);

    tokio::time::pause();
    let mut expected = 0;
    let mut completed = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    for (index, frame) in FRAMES.iter().enumerate() {
        if index > 0 {
            tokio::time::advance(Duration::from_secs(3)).await;
            advance_tx.send(()).unwrap();
        }
        expected += frame.len();
        // The initial frame proves response headers reached the client before
        // any clock advance. Each later frame proves the idle timer reset.
        // Yield stays runnable so paused time cannot advance automatically
        // while the real socket's readiness notification catches up.
        while consumed.load(Ordering::SeqCst) < expected || index == FRAMES.len() - 1 {
            assert!(
                std::time::Instant::now() < deadline,
                "client did not consume stream frame {index}"
            );
            tokio::select! {
                biased;
                result = &mut completion => {
                    assert_eq!(index, FRAMES.len() - 1,
                        "stream ended before its terminal frame: {result:?}");
                    completed = Some(result);
                    break;
                }
                _ = tokio::task::yield_now() => {}
            }
        }
        assert_eq!(consumed.load(Ordering::SeqCst), expected);
    }
    tokio::time::resume();
    let reply = completed
        .expect("the completed stream must return")
        .expect("progress must reset the idle read timeout");
    server.await.unwrap();
    assert_eq!(reply.content, "still progressing");
}

/// A stream that keeps every gap under the idle timeout but never finishes
/// must still be cut off — otherwise a model-swap proxy that drip-feeds
/// heartbeat bytes just under the idle window can wedge a turn forever, as
/// diagnosed from a `newt` session stuck ~95 minutes past its stated "120s
/// idle timeout". The per-attempt total read deadline
/// (`STREAM_TOTAL_TIMEOUT_MULTIPLIER * idle_timeout`) is the backstop.
///
/// Uses paused virtual time (never a real sleep) per the unit-tier rule: a
/// real-clock version of this test flaked by tripping the idle timer instead
/// of the total deadline under scheduler jitter.
#[tokio::test]
async fn never_ending_local_stream_hits_its_total_read_deadline() {
    const HEARTBEAT: &[u8] = b": heartbeat\n\n";
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (advance_tx, mut advance_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let _request = request_body(&mut socket).await;
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        // Keep trickling a comment frame (no `data:` payload, never `[DONE]`)
        // every time the test asks, well within the idle window, forever.
        while advance_rx.recv().await.is_some() {
            if socket.write_all(HEARTBEAT).await.is_err() {
                break;
            }
        }
    });

    let idle_timeout = Duration::from_secs(2);
    let backend = LocalVllmBackend::new(endpoint, "never-ending-fixture")
        .with_client(reqwest::Client::new())
        .with_timeout(idle_timeout)
        .with_retry_policy(RetryPolicy::immediate(0));
    let consumed = Arc::new(AtomicUsize::new(0));
    let mut completion = Box::pin(RESPONSE_BYTES_READ.scope(
        consumed.clone(),
        backend.complete(ChatRequest::new().user("keep generating forever")),
    ));

    tokio::time::pause();
    let mut expected = 0usize;
    let mut completed = None;
    // Total deadline is 5 * idle_timeout = 10s. Advance 1s per heartbeat (well
    // under the 2s idle window) for 12 heartbeats — 12s of stream time,
    // crossing the total deadline without a single gap tripping the idle timer.
    for _ in 0..12 {
        advance_tx
            .send(())
            .expect("server task must still be running");
        tokio::time::advance(Duration::from_secs(1)).await;
        expected += HEARTBEAT.len();
        loop {
            tokio::select! {
                biased;
                result = &mut completion => {
                    completed = Some(result);
                    break;
                }
                _ = tokio::task::yield_now() => {
                    if consumed.load(Ordering::SeqCst) >= expected {
                        break;
                    }
                }
            }
        }
        if completed.is_some() {
            break;
        }
    }
    tokio::time::resume();
    drop(advance_tx);
    let _ = server.await;

    let result = completed.expect(
        "the total read deadline must cut the stream off within 12 heartbeats, not hang forever",
    );
    let error = result.expect_err("a stream that never finishes must not hang forever");
    assert!(
        error.to_string().contains("total read deadline elapsed"),
        "expected a total-deadline error, got: {error}"
    );
}
