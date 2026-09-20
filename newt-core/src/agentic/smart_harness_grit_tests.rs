//! #2449: refused global admission cannot consume a reusable auxiliary budget.
use super::*;
use crate::agentic::run_allowance::RunAllowance;
use crate::agentic::turn_admission::{CorrectionCause, TurnAdmission, TurnPolicy};
use crate::tenacity::{Tenacity, TenacityBudgets};
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn owner(run: &RunAllowance) -> Arc<TurnAdmission> {
    TurnAdmission::new(
        TurnPolicy {
            tenacity: Tenacity::Grit,
            budgets: TenacityBudgets::default(),
            rounds: crate::tenacity::resolve_tool_round_limit(8, None, None),
            grace_rounds: 0,
        },
        Some(run.clone()),
    )
}

fn observe(h: &SmartHarness) {
    h.request(
        &serde_json::json!({"messages":[{"role":"user","content":"Explain the result."}]}),
        "openai",
    )
    .unwrap();
    h.observe(b"Expected nonzero.").unwrap();
}

async fn http_harness() -> (SmartHarness, MockServer) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("\"answer\""))
        .mount(&server)
        .await;
    let uri = server.uri();
    let complete: Arc<SummarizeFn> = Arc::new(move |prompt| {
        let uri = uri.clone();
        Box::pin(async move {
            let response = reqwest::Client::new().post(uri).body(prompt).send().await?;
            Ok((response.text().await?, None))
        })
    });
    let h = SmartHarness::new(
        Session::new(Default::default()).unwrap(),
        complete,
        AdjudicationSettings {
            max_calls: 1,
            ..Default::default()
        },
    )
    .unwrap();
    (h, server)
}

/// The actual classify path must retain its local budget when shared admission
/// refuses before the auxiliary callback can send HTTP. Dropping the binding
/// must also let the same harness use a different external turn's allowance.
#[tokio::test]
async fn grit_2449_refused_auxiliary_preserves_local_calls_and_binding() {
    let (h, server) = http_harness().await;
    let exhausted = RunAllowance::new(0);
    let blocked = owner(&exhausted);
    observe(&h);
    {
        let _binding = h.bind_admission(Some(blocked.clone())).unwrap();
        assert!(matches!(
            h.classify("Expected nonzero.", 2, true, None)
                .await
                .unwrap(),
            Control::Finish {
                reason: crate::TurnEndReason::Failed,
                ..
            }
        ));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(blocked.execution_receipt()["grit_used"], 0);
    assert_eq!(
        h.state().unwrap().calls,
        0,
        "refused work must not spend a local auxiliary call"
    );
    assert!(h.state().unwrap().admission.is_none());
    let available = RunAllowance::new(1);
    let next = owner(&available);
    observe(&h);
    {
        let _binding = h.bind_admission(Some(next)).unwrap();
        assert!(matches!(
            h.classify("Expected nonzero.", 2, true, None)
                .await
                .unwrap(),
            Control::Answer
        ));
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(available.remaining(), 0);
    assert_eq!(h.state().unwrap().calls, 1);
}

/// Existing biased outer cancellation is a control, not an asserted new red.
/// A proposed correction must remain unspent when classify is already cancelled.
#[tokio::test]
async fn grit_2449_cancelled_classification_sends_no_corrective_auxiliary() {
    let (h, server) = http_harness().await;
    let run = RunAllowance::new(2);
    let admission = owner(&run);
    admission.begin_round();
    admission.observe(Some(crate::ExecOutcome::Failed), false);
    admission
        .propose(CorrectionCause::ToolFailure, true, None)
        .unwrap();
    observe(&h);
    let cancel = std::sync::atomic::AtomicBool::new(true);
    let _binding = h.bind_admission(Some(admission.clone())).unwrap();
    assert!(matches!(
        h.classify("Expected nonzero.", 2, true, Some(&cancel))
            .await
            .unwrap(),
        Control::Finish {
            reason: crate::TurnEndReason::Cancelled,
            ..
        }
    ));
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(run.remaining(), 2);
    assert_eq!(admission.execution_receipt()["grit_used"], 0);
    assert_eq!(admission.rounds_used(), 0);
    assert_eq!(h.state().unwrap().calls, 0);
}
