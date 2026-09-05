use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use newt_core::BackendKind;
use newt_scheduler::{ChatRequest, Dispatcher, LocalDispatcher, PoolBackend};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

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
            "message": { "role": "assistant", "content": "done" }
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

async fn next_arrival(arrived: &mut tokio::sync::mpsc::UnboundedReceiver<usize>) -> usize {
    tokio::time::timeout(Duration::from_secs(1), arrived.recv())
        .await
        .expect("request should reach wiremock")
        .expect("arrival channel should remain open")
}

#[tokio::test]
async fn one_slot_prevents_two_requests_from_overlapping() {
    let (_server, backend, mut arrived) =
        mock_backend(1, vec![success(Duration::from_millis(100))]).await;
    let start = Arc::new(tokio::sync::Barrier::new(3));
    let first = spawn_dispatch(backend.clone(), start.clone());
    let second = spawn_dispatch(backend, start.clone());
    start.wait().await;

    assert_eq!(next_arrival(&mut arrived).await, 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), arrived.recv())
            .await
            .is_err(),
        "the second request reached wiremock before the first response completed"
    );
    assert_eq!(next_arrival(&mut arrived).await, 1);
    assert!(first.await.unwrap().is_ok());
    assert!(second.await.unwrap().is_ok());
}

#[tokio::test]
async fn configured_slots_allow_n_requests_but_make_n_plus_one_wait() {
    const SLOTS: usize = 3;
    let (_server, backend, mut arrived) =
        mock_backend(SLOTS, vec![success(Duration::from_millis(100))]).await;
    let start = Arc::new(tokio::sync::Barrier::new(SLOTS + 2));
    let tasks: Vec<_> = (0..=SLOTS)
        .map(|_| spawn_dispatch(backend.clone(), start.clone()))
        .collect();
    start.wait().await;

    for expected in 0..SLOTS {
        assert_eq!(next_arrival(&mut arrived).await, expected);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(25), arrived.recv())
            .await
            .is_err(),
        "request N+1 reached wiremock while all N slots were occupied"
    );
    assert_eq!(next_arrival(&mut arrived).await, SLOTS);
    for task in tasks {
        assert!(task.await.unwrap().is_ok());
    }
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
        Duration::from_secs(1),
        LocalDispatcher.dispatch(&backend, "test-model", ChatRequest::new().user("succeeds")),
    )
    .await
    .expect("a failed request must not leak its permit");
    assert!(recovered.is_ok());
    assert_eq!(next_arrival(&mut arrived).await, 1);
}

#[tokio::test]
async fn timed_out_request_releases_its_slot() {
    let (_server, backend, mut arrived) = mock_backend(
        1,
        vec![success(Duration::from_secs(60)), success(Duration::ZERO)],
    )
    .await;

    let timed_out = tokio::spawn({
        let backend = backend.clone();
        async move {
            tokio::time::timeout(
                Duration::from_secs(1),
                LocalDispatcher.dispatch(
                    &backend,
                    "test-model",
                    ChatRequest::new().user("times out"),
                ),
            )
            .await
        }
    });
    assert_eq!(next_arrival(&mut arrived).await, 0);
    assert!(timed_out.await.unwrap().is_err());
    let recovered = tokio::time::timeout(
        Duration::from_secs(1),
        LocalDispatcher.dispatch(&backend, "test-model", ChatRequest::new().user("succeeds")),
    )
    .await
    .expect("a timed-out request must not leak its permit");
    assert!(recovered.is_ok());
    assert_eq!(next_arrival(&mut arrived).await, 1);
}
