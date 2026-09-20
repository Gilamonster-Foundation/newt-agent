//! #2449 implementation review: typed filesystem outcomes retain mutation semantics.
use super::*;

async fn status_after(name: &str, args: serde_json::Value) -> CheckStatus {
    let root = tempfile::tempdir().unwrap();
    let task = "Verify using `cargo test`.";
    let checks = detect_checks(&[], task);
    let mut ledger = VerificationLedger::for_turn(task, true);
    // Unknown bounded tree state deliberately exercises the existing fallback.
    ledger.record_exec("cargo test", ExecOutcome::Passed, None);
    ledger
        .observe(
            name,
            &args,
            true,
            Some(ExecOutcome::Passed),
            root.path().to_str().unwrap(),
        )
        .await;
    let (_, report) = conclude(&Conclusion {
        checks: &checks,
        requested: &[],
        ledger: &ledger,
        tree_now: None,
        repairs_used: 0,
        rounds_left: true,
    });
    assert_eq!(report.basis, StateBasis::MutationChain);
    report.checks[0].status
}

#[tokio::test]
async fn grit_2449_typed_read_preserves_pass_without_tree_snapshot() {
    assert_eq!(
        status_after("read_file", serde_json::json!({"path":"notes.txt"})).await,
        CheckStatus::Passed
    );
}

#[tokio::test]
async fn grit_2449_typed_listing_preserves_pass_without_tree_snapshot() {
    assert_eq!(
        status_after("list_dir", serde_json::json!({"path":"."})).await,
        CheckStatus::Passed
    );
}

#[tokio::test]
async fn grit_2449_typed_write_stales_pass_without_tree_snapshot() {
    assert_eq!(
        status_after(
            "write_file",
            serde_json::json!({"path":"notes.txt","content":"changed"})
        )
        .await,
        CheckStatus::Stale
    );
}
