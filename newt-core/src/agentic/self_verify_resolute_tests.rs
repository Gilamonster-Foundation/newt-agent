//! #2451: failure and cancellation at the async completion boundary.
use super::*;

#[tokio::test]
async fn resolute_2451_scan_failure_cannot_accept_old_explicit_pass() {
    let mut ledger = VerificationLedger::for_turn("Run `true` to verify.", true);
    ledger.policy.required = true;
    ledger.record_exec("true", ExecOutcome::Passed, None);
    // A child below an absent temporary path is deterministically unreadable;
    // no permission-bit assumption fails when tests happen to run as root.
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let (decision, _) = conclude_turn(
        Concluding {
            cancel: None,
            messages: &[],
            workspace: missing.to_str().unwrap(),
            task: "Run `true` to verify.",
            rounds_left: true,
            round: 0,
            ledger: &ledger,
            solve_obs: None,
        },
        0,
    )
    .await;
    assert_eq!(
        decision,
        Decision::Stop(crate::TurnEndReason::VerificationIncomplete)
    );
}

#[test]
fn resolute_2451_cancel_during_check_scan_wins_over_completion() {
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Poll;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        // Occupy the sole blocking worker so the first poll MUST yield in the
        // real scan, rather than depending on a race with a fast filesystem.
        let blocker = tokio::task::spawn_blocking(move || {
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        ready_rx.await.unwrap();
        let mut ledger = VerificationLedger::for_turn("", true);
        ledger.policy.required = true;
        let flag = AtomicBool::new(false);
        let dir = tempfile::tempdir().unwrap();
        let future = conclude_turn(
            Concluding {
                cancel: Some(&flag),
                messages: &[],
                workspace: dir.path().to_str().unwrap(),
                task: "",
                rounds_left: true,
                round: 0,
                ledger: &ledger,
                solve_obs: None,
            },
            0,
        );
        tokio::pin!(future);
        std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("the occupied blocking pool must hold the scan"),
        })
        .await;
        flag.store(true, Ordering::SeqCst);
        release_tx.send(()).unwrap();
        let (decision, _) = future.await;
        blocker.await.unwrap();
        assert_eq!(decision, Decision::Stop(crate::TurnEndReason::Cancelled));
    });
}

/// #2451: a strict task cannot bypass an inferred check by claiming completion
/// before writing anything. Normal retains its existing unchanged-tree filter.
#[tokio::test]
async fn resolute_2451_unchanged_workspace_still_requires_its_manifest_check() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .unwrap();
    let workspace = dir.path().to_str().unwrap();
    for required in [false, true] {
        let ledger = VerificationLedger::for_workspace(
            "Complete the task.",
            VerificationPolicy {
                required,
                enabled: true,
                result_aware: true,
            },
            workspace,
            true,
        )
        .await;
        let (decision, report) = conclude_turn(
            Concluding {
                cancel: None,
                messages: &[],
                workspace,
                task: "Complete the task.",
                rounds_left: false,
                round: 0,
                ledger: &ledger,
                solve_obs: None,
            },
            0,
        )
        .await;
        if required {
            assert_eq!(
                decision,
                Decision::Stop(crate::TurnEndReason::VerificationIncomplete)
            );
            assert!(report
                .checks
                .iter()
                .any(|check| check.status == CheckStatus::NeverRun));
        } else {
            assert_eq!(decision, Decision::Accept);
            assert!(report.checks.is_empty());
        }
    }
}
