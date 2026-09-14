use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use newt_core::BackendKind;
use newt_scheduler::{ChatRequest, Dispatcher, LocalDispatcher, PoolBackend};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

// A real-HTTP fixture watchdog, not a bound on semaphore release latency.
const FIXTURE_WATCHDOG: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct ScriptedResponder {
    arrived: tokio::sync::mpsc::UnboundedSender<usize>,
    next: Arc<AtomicUsize>,
    responses: Arc<Vec<ResponseTemplate>>,
}

impl Respond for ScriptedResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let index = self.next.fetch_add(1, Ordering::SeqCst);
        self.arrived.send(index).expect("arrival observer is alive");
        self.responses
            .get(index)
            .or_else(|| self.responses.last())
            .expect("at least one response is scripted")
            .clone()
    }
}

fn success(delay: Duration) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .set_delay(delay)
        .set_body_json(serde_json::json!({
            "model": "test-model",
            "message": { "role": "assistant", "content": "done" },
            "done": true
        }))
}

async fn mock_backend(
    slots: usize,
    responses: Vec<ResponseTemplate>,
) -> (
    MockServer,
    PoolBackend,
    tokio::sync::mpsc::UnboundedReceiver<usize>,
) {
    let server = MockServer::start().await;
    let (arrived_tx, arrived_rx) = tokio::sync::mpsc::unbounded_channel();
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ScriptedResponder {
            arrived: arrived_tx,
            next: Arc::new(AtomicUsize::new(0)),
            responses: Arc::new(responses),
        })
        .mount(&server)
        .await;
    let backend =
        PoolBackend::new("one-endpoint", server.uri(), BackendKind::Ollama).with_slots(slots);
    (server, backend, arrived_rx)
}

fn spawn_dispatch(
    backend: PoolBackend,
    start: Arc<tokio::sync::Barrier>,
) -> tokio::task::JoinHandle<anyhow::Result<newt_scheduler::ChatReply>> {
    tokio::spawn(async move {
        start.wait().await;
        LocalDispatcher
            .dispatch(&backend, "test-model", ChatRequest::new().user("hello"))
            .await
    })
}

async fn next_arrival<T>(arrived: &mut tokio::sync::mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(FIXTURE_WATCHDOG, arrived.recv())
        .await
        .expect("request should reach the HTTP fixture")
        .expect("arrival channel should remain open")
}

// Adapt the socket fixtures in newt-core's openai_stream_loop tests: wiremock's
// synchronous Respond cannot hold one response without blocking other requests.
// Each connection here waits independently for its own explicit release signal.
struct HeldResponseServer(tokio::task::JoinHandle<()>);

impl Drop for HeldResponseServer {
    fn drop(&mut self) {
        // Aborting the owner also drops its JoinSet and all connection tasks.
        self.0.abort();
    }
}

async fn held_response_backend(
    slots: usize,
) -> (
    HeldResponseServer,
    PoolBackend,
    tokio::sync::mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<()>>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (arrived_tx, arrived_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        for _ in 0..=slots {
            let (socket, _) = listener.accept().await.unwrap();
            let arrived = arrived_tx.clone();
            connections.spawn(async move {
                let mut request = BufReader::new(socket);
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    assert!(request.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse().unwrap();
                        }
                    }
                }
                request
                    .read_exact(&mut vec![0; content_length])
                    .await
                    .unwrap();
                let (release, released) = tokio::sync::oneshot::channel();
                arrived.send(release).unwrap();
                released.await.unwrap();
                let body = r#"{"model":"test-model","message":{"role":"assistant","content":"done"},"done":true}"#;
                request
                    .get_mut()
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            });
        }
        while let Some(result) = connections.join_next().await {
            result.unwrap();
        }
    });
    let backend =
        PoolBackend::new("held-endpoint", endpoint, BackendKind::Ollama).with_slots(slots);
    (HeldResponseServer(server), backend, arrived_rx)
}

/// Grounds the deterministic permit lifetime test against real HTTP while the
/// first response is held, then verifies normal completion releases the slot.
#[tokio::test]
async fn one_slot_prevents_two_requests_from_overlapping() {
    let (mut server, backend, mut arrived) = held_response_backend(1).await;
    let start = Arc::new(tokio::sync::Barrier::new(3));
    let first = spawn_dispatch(backend.clone(), start.clone());
    let second = spawn_dispatch(backend, start.clone());
    start.wait().await;

    let release_first = next_arrival(&mut arrived).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(25), arrived.recv())
            .await
            .is_err(),
        "the second request arrived while the first response was still held"
    );
    release_first.send(()).unwrap();
    next_arrival(&mut arrived).await.send(()).unwrap();
    tokio::time::timeout(FIXTURE_WATCHDOG, async {
        assert!(first.await.unwrap().is_ok());
        assert!(second.await.unwrap().is_ok());
        (&mut server.0).await.unwrap();
    })
    .await
    .expect("released HTTP requests should finish");
}

/// Grounds the same permit contract at capacity N: the server can hold N
/// independent responses, but sees N+1 only after one is explicitly released.
#[tokio::test]
async fn configured_slots_allow_n_requests_but_make_n_plus_one_wait() {
    const SLOTS: usize = 3;
    let (mut server, backend, mut arrived) = held_response_backend(SLOTS).await;
    let start = Arc::new(tokio::sync::Barrier::new(SLOTS + 2));
    let tasks: Vec<_> = (0..=SLOTS)
        .map(|_| spawn_dispatch(backend.clone(), start.clone()))
        .collect();
    start.wait().await;

    let mut held = Vec::new();
    for _ in 0..SLOTS {
        held.push(next_arrival(&mut arrived).await);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(25), arrived.recv())
            .await
            .is_err(),
        "request N+1 arrived while all N responses were still held"
    );
    held.pop().unwrap().send(()).unwrap();
    next_arrival(&mut arrived).await.send(()).unwrap();
    for release in held {
        release.send(()).unwrap();
    }
    tokio::time::timeout(FIXTURE_WATCHDOG, async {
        for task in tasks {
            assert!(task.await.unwrap().is_ok());
        }
        (&mut server.0).await.unwrap();
    })
    .await
    .expect("released HTTP requests should finish");
}

#[tokio::test]
async fn failed_request_releases_its_slot() {
    let (_server, backend, mut arrived) = mock_backend(
        1,
        vec![
            ResponseTemplate::new(400).set_body_string("fatal request"),
            success(Duration::ZERO),
        ],
    )
    .await;

    assert!(LocalDispatcher
        .dispatch(&backend, "test-model", ChatRequest::new().user("fails"),)
        .await
        .is_err());
    assert_eq!(next_arrival(&mut arrived).await, 0);
    let recovered = tokio::time::timeout(
        FIXTURE_WATCHDOG,
        LocalDispatcher.dispatch(&backend, "test-model", ChatRequest::new().user("succeeds")),
    )
    .await
    .expect("the successor HTTP request should complete after the failure");
    assert!(recovered.is_ok());
    assert_eq!(next_arrival(&mut arrived).await, 1);
}

/// Grounds `cancelled_permit_owner_releases_its_slot_immediately` against real
/// HTTP: cancel only after dispatch owns a slot and reaches the server, then
/// require a successful successor through the same one-slot backend.
#[tokio::test]
async fn timed_out_request_releases_its_slot() {
    let (_server, backend, mut arrived) = mock_backend(
        1,
        vec![success(Duration::from_secs(60)), success(Duration::ZERO)],
    )
    .await;

    let mut active = Box::pin(LocalDispatcher.dispatch(
        &backend,
        "test-model",
        ChatRequest::new().user("times out"),
    ));
    tokio::select! {
        arrival = next_arrival(&mut arrived) => assert_eq!(arrival, 0),
        reply = &mut active => panic!("delayed request completed before cancellation: {reply:?}"),
    }
    // Own the future: timing out a borrow would leave its permit alive.
    assert!(tokio::time::timeout(Duration::ZERO, active).await.is_err());
    let recovered = tokio::time::timeout(
        FIXTURE_WATCHDOG,
        LocalDispatcher.dispatch(&backend, "test-model", ChatRequest::new().user("succeeds")),
    )
    .await
    .expect("the successor HTTP request should complete after cancellation");
    assert!(recovered.is_ok());
    assert_eq!(next_arrival(&mut arrived).await, 1);
}
