//! #2449: invalid local requests never consume a pending corrective admission.
use super::*;
use crate::agentic::turn_admission::{CorrectionCause, TurnAdmission, TurnPolicy};
use crate::tenacity::{Tenacity, TenacityBudgets};
use std::sync::Arc;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

fn pending(run: &RunAllowance) -> Arc<TurnAdmission> {
    let owner = TurnAdmission::new(
        TurnPolicy {
            tenacity: Tenacity::Grit,
            budgets: TenacityBudgets::default(),
            rounds: crate::tenacity::resolve_tool_round_limit(8, None, None),
            grace_rounds: 0,
        },
        Some(run.clone()),
    );
    owner.begin_round();
    owner.observe(Some(crate::ExecOutcome::Failed), false);
    owner
        .propose(CorrectionCause::ToolFailure, true, None)
        .unwrap();
    owner
}

fn scope<'a>(
    ledger: &'a Mutex<AttemptLedger>,
    owner: &'a TurnAdmission,
    run: &'a RunAllowance,
) -> AttemptScope<'a> {
    AttemptScope {
        ledger,
        admission: Some(owner),
        turn: "operator",
        model: "test",
        backend: "fixture",
        cancel: None,
        run_allowance: Some(run),
    }
}

/// Grounds the old bodyless-request refusal in actual untouched global/Grit accounting.
#[tokio::test]
async fn grit_2449_bodyless_request_does_not_admit_correction() {
    let server = MockServer::start().await;
    let run = RunAllowance::new(3);
    let owner = pending(&run);
    let ledger = Mutex::new(AttemptLedger::default());
    let error = send(
        Some(scope(&ledger, &owner, &run)),
        "primary",
        reqwest::Client::new().get(server.uri()),
        "fixture",
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("without an in-memory body"));
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(ledger.lock().unwrap().head().is_none());
    assert_eq!(
        run.remaining(),
        3,
        "local refusal spends no global model request"
    );
    assert_eq!(owner.execution_receipt()["grit_used"], 0);
    assert_eq!(owner.rounds_used(), 0);
}

/// A malformed URL fails during local construction, before model admission.
#[tokio::test]
async fn grit_2449_malformed_request_does_not_admit_correction() {
    let run = RunAllowance::new(3);
    let owner = pending(&run);
    let ledger = Mutex::new(AttemptLedger::default());
    assert!(send(
        Some(scope(&ledger, &owner, &run)),
        "primary",
        reqwest::Client::new().post("http://["),
        "fixture"
    )
    .await
    .is_err());
    assert!(ledger.lock().unwrap().head().is_none());
    assert_eq!(
        run.remaining(),
        3,
        "a builder error is not an admitted request"
    );
    assert_eq!(owner.execution_receipt()["grit_used"], 0);
    assert_eq!(owner.rounds_used(), 0);
}

/// The refusal controls above must not refund work a real backend rejected.
#[tokio::test]
async fn grit_2449_backend_failure_does_not_refund_admitted_correction() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let run = RunAllowance::new(3);
    let owner = pending(&run);
    let ledger = Mutex::new(AttemptLedger::default());
    let policy = owner.policy_receipt();
    let (response, attempt) = send(
        Some(scope(&ledger, &owner, &run)),
        "primary",
        reqwest::Client::new().post(server.uri()).body("{}"),
        "fixture",
    )
    .await
    .unwrap();
    assert_eq!(response.status().as_u16(), 503);
    failed(attempt.as_ref(), &anyhow::anyhow!("backend unavailable"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(run.remaining(), 2);
    assert_eq!(owner.execution_receipt()["grit_used"], 1);
    assert_eq!(owner.rounds_used(), 1);
    assert_eq!(
        owner.policy_receipt(),
        policy,
        "execution cannot alter effective-config policy identity"
    );
}

/// Two obligations addressed by one continuation spend one Grit unit and
/// preserve the additional verification bound, regardless of proposal order.
#[test]
fn grit_2449_overlapping_proposals_keep_both_bounds() {
    for causes in [
        [CorrectionCause::ToolFailure, CorrectionCause::FailedCheck],
        [CorrectionCause::FailedCheck, CorrectionCause::ToolFailure],
        [CorrectionCause::Verification, CorrectionCause::ToolFailure],
    ] {
        let run = RunAllowance::new(3);
        let owner = pending(&run);
        owner.propose(causes[0], true, None).unwrap();
        owner.observe(Some(crate::ExecOutcome::Failed), false);
        owner.propose(causes[1], true, None).unwrap();
        owner.reserve_model(None).unwrap();
        assert_eq!(owner.execution_receipt()["grit_used"], 1);
        assert_eq!(owner.verification_used(), 1);
        assert!(
            !owner.has_unacknowledged_failure(),
            "all failures addressed by this continuation share its one admission"
        );
        assert_eq!(run.remaining(), 2);
    }
}
