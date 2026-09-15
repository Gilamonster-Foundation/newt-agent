use super::*;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn scope(ledger: &Mutex<AttemptLedger>) -> AttemptScope<'_> {
    AttemptScope {
        ledger,
        turn: "prompt:turn",
        model: "test-model",
        backend: "test-backend",
        cancel: None,
    }
}

/// Precision (1): a request whose body is not in memory cannot be keyed from
/// its bytes, so it is refused before anything is sent or recorded.
#[tokio::test]
async fn a_request_without_an_in_memory_body_is_refused_and_not_recorded() {
    let ledger = Mutex::new(AttemptLedger::default());
    let request = reqwest::Client::new().post("http://127.0.0.1:9/api/chat");
    let error = send(Some(scope(&ledger)), "primary", request, "request failed")
        .await
        .expect_err("no body bytes, no attempt");
    assert!(
        error.to_string().contains("without an in-memory body"),
        "{error}"
    );
    assert_eq!(ledger.lock().unwrap().totals().attempts, 0);
}

/// Precision (3): each try of a retried request is one attempt. A 500 then a
/// 200 is two requests and two attempts — the first `failed` with no usage,
/// the second `ok` with the usage it reported — through the same
/// `with_backoff_notify_error` shape as `dispatch_with_decoder`, whose
/// "inference endpoint" status prefix classifies a 5xx as retryable.
#[tokio::test]
async fn each_retry_of_a_failing_request_is_one_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"content": "ok"}, "prompt_eval_count": 40, "eval_count": 2
        })))
        .mount(&server)
        .await;
    let ledger = Mutex::new(AttemptLedger::default());
    let client = reqwest::Client::new();
    let url = format!("{}/api/chat", server.uri());
    let body = serde_json::json!({"model": "test-model", "messages": [], "stream": false});
    let policy = crate::retry::RetryPolicy {
        max_retries: 2,
        base: std::time::Duration::ZERO,
        max: std::time::Duration::ZERO,
        jitter: false,
    };
    crate::retry::with_backoff_notify_error(
        &policy,
        || async {
            let (response, key) = send(
                Some(scope(&ledger)),
                "primary",
                client.post(&url).json(&body),
                "request failed",
            )
            .await?;
            let json =
                super::super::smart_harness::response(response, None, "inference endpoint").await?;
            complete(key.as_ref(), super::super::trim::ollama_usage(&json));
            Ok(json)
        },
        |_, _, _| {},
    )
    .await
    .unwrap_or_else(|error| panic!("the retry succeeds: {error:#}"));

    let received = server.received_requests().await.expect("journal");
    assert_eq!(received.len(), 2);
    let ledger = ledger.lock().unwrap();
    let totals = ledger.totals();
    assert_eq!((totals.attempts, totals.usage_missing), (2, 1));
    let mut records: Vec<_> = ledger.records().collect();
    records.sort_by_key(|record| record.key.ordinal);
    let wire = content_addressable::RawContentId::from_content(&received[0].body);
    assert!(records.iter().all(|record| record.key.request == wire));
    assert_eq!(records[0].state, AttemptState::Failed);
    assert_eq!(records[0].usage, None);
    assert_eq!(records[1].state, AttemptState::Ok);
    assert_eq!(
        records[1].usage,
        Some(TokenUsage {
            input_tokens: 40,
            output_tokens: 2
        })
    );
}

/// #2313 (c): the attempt handle's drop. Unsettled under an interrupt it is
/// cancelled with no usage; unsettled with the flag clear its send-time
/// `failed` stands; settled first, a later interrupt changes nothing.
#[tokio::test]
async fn a_dropped_attempt_is_cancelled_only_when_unsettled_under_an_interrupt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let url = format!("{}/api/chat", server.uri());
    let used = Some(TokenUsage {
        input_tokens: 9,
        output_tokens: 1,
    });
    for (name, settle, interrupt, expected) in [
        (
            "unsettled, interrupted",
            false,
            true,
            (AttemptState::Cancelled, None),
        ),
        (
            "unsettled, not interrupted",
            false,
            false,
            (AttemptState::Failed, None),
        ),
        (
            "settled, then interrupted",
            true,
            true,
            (AttemptState::Ok, used),
        ),
    ] {
        let ledger = Mutex::new(AttemptLedger::default());
        let flag = std::sync::atomic::AtomicBool::new(false);
        let scope = AttemptScope {
            cancel: Some(&flag),
            ..scope(&ledger)
        };
        let body = serde_json::json!({"case": name});
        let (_response, attempt) = send(
            Some(scope),
            "primary",
            reqwest::Client::new().post(&url).json(&body),
            "request failed",
        )
        .await
        .expect("sent");
        if settle {
            complete(attempt.as_ref(), used);
        }
        flag.store(interrupt, std::sync::atomic::Ordering::Relaxed);
        drop(attempt);
        let ledger = ledger.lock().unwrap();
        let records: Vec<_> = ledger.records().collect();
        assert_eq!(records.len(), 1, "{name}");
        assert_eq!((records[0].state, records[0].usage), expected, "{name}");
    }
}
