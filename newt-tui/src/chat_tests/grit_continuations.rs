//! #2449: real queued host inputs retain executed verification evidence.
use super::*;
use newt_core::agentic::turn_admission::{TurnAdmission, TurnPolicy};
use newt_core::agentic::{chat_complete, ChatCtx, PromptDisposition};
use newt_core::{Caveats, MemMessage, Tenacity};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};

#[path = "grit_http_fixture.rs"]
mod fixture;

pub(super) async fn phase(
    workspace: &std::path::Path,
    task: &str,
    disposition: PromptDisposition,
    owner: Arc<TurnAdmission>,
    command: Option<&str>,
) -> (
    newt_core::TurnEndReason,
    Vec<newt_core::BehaviorSignal>,
    usize,
) {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let served = calls.clone();
    let command = command.map(str::to_string);
    Mock::given(method("POST")).respond_with(move |_: &Request| {
        let step = served.fetch_add(1, Ordering::SeqCst);
        let message = if step == 0 && command.is_some() {
            serde_json::json!({"role":"assistant","content":"", "tool_calls":[{
                "function":{"name":"run_command","arguments":{"command":command}}}]})
        } else {
            serde_json::json!({"role":"assistant","content":"The requested assessment is complete."})
        };
        ResponseTemplate::new(200).set_body_json(serde_json::json!({"message":message,"done":true}))
    }).mount(&server).await;
    let _binding = owner.bind_policy();
    let messages = [MemMessage::user(task)];
    let caveats = Caveats::top();
    let url = server.uri();
    let workspace = workspace.to_string_lossy();
    let mut ctx: ChatCtx<'_> = fixture::ctx(&url, &messages, &caveats);
    let mut reason = None;
    let mut observation = newt_core::agentic::SolveObservation::default();
    ctx.task = task;
    ctx.workspace = &workspace;
    ctx.turn_admission = Some(owner);
    ctx.prompt_disposition = disposition;
    ctx.max_tool_rounds = 20;
    ctx.end_reason = Some(&mut reason);
    ctx.solve_obs = Some(&mut observation);
    chat_complete(ctx, &mut Mcp::empty()).await.unwrap();
    (
        reason.unwrap(),
        observation.behavior_signals,
        calls.load(Ordering::SeqCst),
    )
}

/// Start with a real Act check, then consume the production retry queue.
/// A failed check stays failed; a pass followed by actual changed bytes is
/// stale. Neither is a never-run check after host re-entry.
async fn queued_retry_retains_check_evidence(failed: bool) {
    let _settings = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let _env = crate::test_env_guard::env_write_guard_async().await;
    let _verify = crate::disable_ocap_session_tests::EnvVar::set("NEWT_SELF_VERIFY", "0");
    let _engine =
        crate::disable_ocap_session_tests::EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = crate::disable_ocap_session_tests::EnvVar::unset("NEWT_DISABLE_OCAP");
    let _tenacity = newt_core::tenacity::scoped_tenacity_settings(
        Tenacity::Resolute,
        newt_core::tenacity::TenacityBudgets::default(),
    );
    {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("notes.txt"), "before\n").unwrap();
        let check = if failed {
            "sh -c 'printf checked > check-marker; exit 1'"
        } else {
            "sh -c 'printf checked > check-marker; test -f notes.txt'"
        };
        let task = format!("Perform the requested check. Verify using `{check}`.");
        let owner = TurnAdmission::new(
            TurnPolicy::capture(
                newt_core::tenacity::resolve_tool_round_limit(20, None, None),
                0,
            ),
            None,
        );
        let (reason, _, initial_requests) = phase(
            workspace.path(),
            &task,
            PromptDisposition::Act,
            owner.clone(),
            Some(check),
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("check-marker")).unwrap(),
            "checked",
            "the first phase actually executed its check"
        );
        assert_eq!(
            reason,
            if failed {
                newt_core::TurnEndReason::RepairExhausted
            } else {
                newt_core::TurnEndReason::Completed
            }
        );
        assert_eq!(initial_requests, if failed { 4 } else { 2 });
        let before = owner.execution_receipt();
        if !failed {
            std::fs::write(workspace.path().join("notes.txt"), "after\n").unwrap();
        }
        let parent = newt_core::TurnPromptContext::ephemeral_operator(
            "fixture",
            task.as_bytes().to_vec(),
            task.as_bytes().to_vec(),
        );
        let queued = PendingRetry {
            text: "Continue the requested assessment using the observed check result.".into(),
            parent: Box::new(parent),
        };
        let (input, origin) = queued.into_input();
        let ReadOutcome::Line(text) = input else {
            panic!("queued input was lost")
        };
        let mut retained = Some(owner.clone());
        retain_external_admission(&origin, &mut retained);
        assert!(Arc::ptr_eq(retained.as_ref().unwrap(), &owner));
        let (reason, signals, _) = phase(
            workspace.path(),
            &text,
            PromptDisposition::Act,
            retained.unwrap(),
            None,
        )
        .await;
        let wanted = if failed { "failed" } else { "stale" };
        assert!(
            signals
                .iter()
                .filter_map(|signal| {
                    let newt_core::BehaviorSignal::Verification { report, .. } = signal else {
                        return None;
                    };
                    Some(serde_json::to_value(report).unwrap())
                })
                .any(|report| report["checks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|check| check["status"] == wanted)),
            "queued {wanted} evidence was lost: {signals:?}"
        );
        assert_eq!(
            reason,
            if failed {
                newt_core::TurnEndReason::RepairExhausted
            } else {
                newt_core::TurnEndReason::VerificationIncomplete
            }
        );
        if !failed {
            assert_eq!(owner.execution_receipt()["grit_used"], before["grit_used"]);
        }
        let mut reset = Some(owner);
        retain_external_admission(&ModelInputOrigin::Operator, &mut reset);
        assert!(
            reset.is_none(),
            "fresh external input owns a fresh allowance"
        );
    }
}

#[tokio::test]
#[serial_test::serial(real_fs)]
async fn grit_2449_queued_retry_retains_failed_check() {
    queued_retry_retains_check_evidence(true).await;
}

#[tokio::test]
#[serial_test::serial(real_fs)]
async fn grit_2449_queued_retry_retains_stale_check() {
    queued_retry_retains_check_evidence(false).await;
}

/// Plan performs no forbidden operation. Approval activates strict Act checks
/// against the original accepted objective without changing the captured dials.
#[tokio::test]
#[serial_test::serial(real_fs)]
async fn grit_2449_plan_approval_activates_original_verification() {
    let _settings = newt_core::test_guard::GlobalSettingsGuard::acquire();
    let _env = crate::test_env_guard::env_write_guard_async().await;
    let _verify = crate::disable_ocap_session_tests::EnvVar::set("NEWT_SELF_VERIFY", "0");
    let _tenacity = newt_core::tenacity::scoped_tenacity_settings(
        Tenacity::Resolute,
        newt_core::tenacity::TenacityBudgets::default(),
    );
    let workspace = tempfile::tempdir().unwrap();
    let task = "Plan the requested change. Verify using `sh -c 'test -f notes.txt'`.";
    let owner = TurnAdmission::new(
        TurnPolicy::capture(
            newt_core::tenacity::resolve_tool_round_limit(20, None, None),
            0,
        ),
        None,
    );
    let (reason, _, requests) = phase(
        workspace.path(),
        task,
        PromptDisposition::Plan,
        owner.clone(),
        None,
    )
    .await;
    assert_eq!(reason, newt_core::TurnEndReason::Completed);
    assert_eq!(requests, 1);
    let queued = PendingPlanTurn {
        text: "Implement the approved plan now.".into(),
        parent: Box::new(newt_core::TurnPromptContext::ephemeral_operator(
            "fixture",
            task.as_bytes().to_vec(),
            task.as_bytes().to_vec(),
        )),
        kind: PlanTurnKind::Approval,
    };
    let (input, origin) = queued.into_input();
    let ReadOutcome::Line(text) = input else {
        panic!("lost queued approval")
    };
    let mut retained = Some(owner.clone());
    retain_external_admission(&origin, &mut retained);
    assert!(Arc::ptr_eq(retained.as_ref().unwrap(), &owner));
    let (reason, signals, _) = phase(
        workspace.path(),
        &text,
        PromptDisposition::Act,
        retained.unwrap(),
        None,
    )
    .await;
    assert_eq!(reason, newt_core::TurnEndReason::VerificationIncomplete);
    assert!(
        signals.iter().any(|signal| matches!(signal,
        newt_core::BehaviorSignal::Verification { report, .. }
        if serde_json::to_value(report).unwrap()["checks"].as_array().unwrap()
            .iter().any(|check| check["status"] == "never_run"))),
        "approval retains the original named check without pretending Plan ran it: {signals:?}"
    );
    assert!(!workspace.path().join("notes.txt").exists());
    assert_eq!(owner.execution_receipt()["grit_used"], 0);
}
