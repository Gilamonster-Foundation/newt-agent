//! #2449: corrective summarization shares primary admission and transport bounds.
use super::*;
use newt_core::agentic::{
    run_allowance::RunAllowance,
    turn_admission::{CorrectionCause, TurnAdmission, TurnPolicy},
};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};

fn pending(run: &RunAllowance, cause: CorrectionCause) -> Arc<TurnAdmission> {
    let owner = TurnAdmission::new(
        TurnPolicy {
            tenacity: newt_core::Tenacity::Grit,
            budgets: newt_core::tenacity::TenacityBudgets::default(),
            rounds: newt_core::tenacity::resolve_tool_round_limit(8, None, None),
            grace_rounds: 0,
        },
        Some(run.clone()),
    );
    owner.propose(cause, true, None).unwrap();
    owner
}

/// A real retry is two shared run units but just one admitted correction.
#[tokio::test]
async fn grit_2449_summarizer_transport_retry_spends_correction_once() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let served = calls.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &Request| {
            if served.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices":[{"message":{"content":"summary"}}]}))
            }
        })
        .mount(&server)
        .await;
    let run = Arc::new(RunAllowance::new(3));
    let owner = pending(&run, CorrectionCause::ToolFailure);
    let summary = make_loop_summarizer(
        server.uri(),
        "fixture".into(),
        newt_core::BackendKind::Openai,
        None,
        None,
        SummarizerOpts {
            retries: 1,
            run_allowance: Some(run.clone()),
            turn_admission: Some(owner.clone()),
            ..Default::default()
        },
    );
    assert_eq!(
        summary("summarize observed failure".into())
            .await
            .unwrap()
            .0,
        "summary"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        run.remaining(),
        1,
        "same run counter, exactly one debit per HTTP attempt"
    );
    assert_eq!(
        owner.execution_receipt()["grit_used"],
        1,
        "transport backoff neither defers nor duplicates the corrective admission"
    );
}

#[tokio::test]
async fn grit_2449_cancelled_summarizer_sends_no_pending_correction() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"choices":[{"message":{"content":"summary"}}]})),
        )
        .mount(&server)
        .await;
    let run = Arc::new(RunAllowance::new(3));
    let owner = pending(&run, CorrectionCause::ToolFailure);
    let summary = make_loop_summarizer(
        server.uri(),
        "fixture".into(),
        newt_core::BackendKind::Openai,
        None,
        None,
        SummarizerOpts {
            retries: 0,
            run_allowance: Some(run.clone()),
            turn_admission: Some(owner.clone()),
            cancel: Some(Arc::new(AtomicBool::new(true))),
            ..Default::default()
        },
    );
    let result = summary("summarize observed failure".into()).await;
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "cancellation stops actual HTTP admission"
    );
    assert!(result.is_err());
    assert_eq!(run.remaining(), 3);
    assert_eq!(owner.execution_receipt()["grit_used"], 0);
}

/// A queued import correction is not admitted by post-turn archival work.
/// Both callbacks still spend exactly one unit from the same global allowance.
#[tokio::test]
async fn grit_2449_archival_summary_does_not_spend_queued_correction() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"choices":[{"message":{"content":"summary"}}]})),
        )
        .mount(&server)
        .await;
    let run = Arc::new(RunAllowance::new(3));
    let owner = pending(&run, CorrectionCause::ImportRepair);
    let archival = make_loop_summarizer(
        server.uri(),
        "fixture".into(),
        newt_core::BackendKind::Openai,
        None,
        None,
        SummarizerOpts {
            retries: 0,
            run_allowance: Some(run.clone()),
            ..Default::default()
        },
    );
    archival("persist the completed turn".into()).await.unwrap();
    assert_eq!(run.remaining(), 2);
    assert_eq!(owner.execution_receipt()["grit_used"], 0);
    let corrective = make_loop_summarizer(
        server.uri(),
        "fixture".into(),
        newt_core::BackendKind::Openai,
        None,
        None,
        SummarizerOpts {
            retries: 0,
            run_allowance: Some(run.clone()),
            turn_admission: Some(owner.clone()),
            ..Default::default()
        },
    );
    corrective("compact for corrective continuation".into())
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert_eq!(run.remaining(), 1);
    assert_eq!(owner.execution_receipt()["grit_used"], 1);
}

/// Local request validation cannot spend shared inference or pending recovery.
#[tokio::test]
async fn grit_2449_summary_malformed_request_preserves_allowances() {
    let run = Arc::new(RunAllowance::new(3));
    let owner = pending(&run, CorrectionCause::ToolFailure);
    let summary = make_loop_summarizer(
        "http://[".into(),
        "fixture".into(),
        newt_core::BackendKind::Openai,
        None,
        None,
        SummarizerOpts {
            retries: 0,
            run_allowance: Some(run.clone()),
            turn_admission: Some(owner.clone()),
            ..Default::default()
        },
    );
    assert!(summary("summarize".into()).await.is_err());
    assert_eq!(
        run.remaining(),
        3,
        "a malformed local request sends nothing"
    );
    assert_eq!(owner.execution_receipt()["grit_used"], 0);
}

/// Ollama warmup is HTTP but not an inference debit; cancellation precedes it.
#[tokio::test]
async fn grit_2449_summary_cancelled_ollama_skips_warm_and_chat() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"message":{"content":"summary"},"done":true})),
        )
        .mount(&server)
        .await;
    let run = Arc::new(RunAllowance::new(3));
    let owner = pending(&run, CorrectionCause::ToolFailure);
    let summary = make_loop_summarizer(
        server.uri(),
        "fixture".into(),
        newt_core::BackendKind::Ollama,
        None,
        None,
        SummarizerOpts {
            retries: 0,
            run_allowance: Some(run.clone()),
            turn_admission: Some(owner.clone()),
            cancel: Some(Arc::new(AtomicBool::new(true))),
            ..Default::default()
        },
    );
    let result = summary("summarize".into()).await;
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "initial cancellation precedes warm /api/generate and inference /api/chat"
    );
    assert!(result.is_err());
    assert_eq!(run.remaining(), 3);
    assert_eq!(owner.execution_receipt()["grit_used"], 0);
}
