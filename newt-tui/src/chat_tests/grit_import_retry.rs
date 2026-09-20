//! #2449: existing import rollback stays ledger-fenced; queued model work shares Grit.
use super::*;
use newt_core::agentic::turn_admission::{TurnAdmission, TurnPolicy};
use newt_core::verify_gate::{SurfaceMatch, WriteLedger};
use std::sync::Arc;

fn owner(retries: u32, rounds: usize) -> Arc<TurnAdmission> {
    TurnAdmission::new(
        TurnPolicy {
            tenacity: newt_core::Tenacity::Grit,
            budgets: newt_core::tenacity::TenacityBudgets {
                grit_retries: retries,
            },
            rounds: newt_core::tenacity::resolve_tool_round_limit(rounds, None, None),
            grace_rounds: 0,
        },
        None,
    )
}

fn parent() -> newt_core::TurnPromptContext {
    newt_core::TurnPromptContext::ephemeral_operator(
        "fixture",
        b"Correct the requested import.".to_vec(),
        b"Correct the requested import.".to_vec(),
    )
}

fn git(root: &std::path::Path, args: &[&str]) -> Vec<u8> {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[tokio::test]
#[serial_test::serial(real_fs)]
async fn grit_2449_import_zero_refuses_queue_after_ledger_only_revert() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("newt-core/src")).unwrap();
    std::fs::write(
        root.path().join("newt-core/src/pyo3_module.rs"),
        "#[pyclass(name=\"X\", module=\"newt_agent._newt_agent.core\")] struct X;",
    )
    .unwrap();
    let edited = root.path().join("edited.py");
    std::fs::write(
        &edited,
        "from newt_agent._newt_agent.core import X\n# staged\n",
    )
    .unwrap();
    git(root.path(), &["init", "-q"]);
    git(root.path(), &["add", "edited.py"]);
    let index = git(root.path(), &["ls-files", "--stage"]);
    let index_bytes = std::fs::read(root.path().join(".git/index")).unwrap();
    let operator_bytes = "from newt_agent._newt_agent.core import X\n# operator dirty\n";
    std::fs::write(&edited, operator_bytes).unwrap();
    let untouched = root.path().join("operator.py");
    std::fs::write(&untouched, "import newt_core\n# untouched operator file\n").unwrap();
    let ledger = std::cell::RefCell::new(WriteLedger::new());
    ledger.borrow_mut().note_before_write(&edited);
    std::fs::write(&edited, "import newt_coder\n").unwrap();
    let action = retry_revert(root.path().to_str().unwrap(), SurfaceMatch::Exact, &ledger)
        .await
        .expect("newt's fabricated import is reverted");
    assert_eq!(std::fs::read_to_string(&edited).unwrap(), operator_bytes);
    assert_eq!(git(root.path(), &["ls-files", "--stage"]), index);
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        index_bytes,
        "ledger-only rollback preserves literal index bytes"
    );
    assert_eq!(
        std::fs::read_to_string(&untouched).unwrap(),
        "import newt_core\n# untouched operator file\n"
    );
    let owner = owner(0, 8);
    let mut profile_budget = 3;
    let (queued, _) = import_retry::queue(
        &mut profile_budget,
        3,
        Some(parent()),
        action.corrective,
        Some(&owner),
        None,
    );
    assert!(queued.is_none(), "profile retry cannot bypass zero Grit");
    assert_eq!(
        profile_budget, 3,
        "shared refusal admits no profile correction"
    );
    assert_eq!(owner.execution_receipt()["grit_used"], 0);
}

#[tokio::test]
#[serial_test::serial(real_fs)]
async fn grit_2449_import_queue_spends_once_on_actual_next_model_request() {
    let _env = crate::test_env_guard::env_write_guard_async().await;
    let _verify = crate::disable_ocap_session_tests::EnvVar::set("NEWT_SELF_VERIFY", "0");
    let workspace = tempfile::tempdir().unwrap();
    let owner = owner(1, 8);
    let mut profile_budget = 2;
    let (queued, _) = import_retry::queue(
        &mut profile_budget,
        2,
        Some(parent()),
        "Assess the corrected import using the supplied evidence.".into(),
        Some(&owner),
        None,
    );
    assert_eq!(
        owner.execution_receipt()["grit_used"],
        0,
        "queueing is not dispatch"
    );
    let (input, origin) = queued.expect("one correction fits").into_input();
    let ReadOutcome::Line(text) = input else {
        panic!("lost retry")
    };
    let mut retained = Some(owner.clone());
    retain_external_admission(&origin, &mut retained);
    assert!(Arc::ptr_eq(retained.as_ref().unwrap(), &owner));
    let (_, _, requests) = grit_continuations_tests::phase(
        workspace.path(),
        &text,
        newt_core::agentic::PromptDisposition::Act,
        retained.unwrap(),
        None,
    )
    .await;
    assert_eq!(requests, 1);
    assert_eq!(owner.execution_receipt()["grit_used"], 1);
    assert_eq!(owner.execution_receipt()["verification_used"], 1);
    let (second, _) = import_retry::queue(
        &mut profile_budget,
        2,
        Some(parent()),
        "Another repair".into(),
        Some(&owner),
        None,
    );
    assert!(
        second.is_none(),
        "remaining profile allowance cannot refill Grit"
    );
}

#[test]
fn grit_2449_import_queue_preserves_profile_cap_control() {
    let owner = owner(2, 8);
    let mut profile_budget = 0;
    let (queued, _) = import_retry::queue(
        &mut profile_budget,
        0,
        Some(parent()),
        "Repair".into(),
        Some(&owner),
        None,
    );
    assert!(queued.is_none());
    assert_eq!(owner.execution_receipt()["grit_used"], 0);
}

fn assert_refused(owner: &TurnAdmission, cancel: Option<&std::sync::atomic::AtomicBool>) {
    let mut profile_budget = 2;
    let (queued, _) = import_retry::queue(
        &mut profile_budget,
        2,
        Some(parent()),
        "Repair".into(),
        Some(owner),
        cancel,
    );
    assert!(
        queued.is_none(),
        "inadmissible correction must not enter the host queue"
    );
    assert_eq!(owner.execution_receipt()["grit_used"], 0);
    assert_eq!(profile_budget, 2);
}

#[test]
fn grit_2449_import_queue_checks_cancellation() {
    assert_refused(
        &owner(2, 8),
        Some(&std::sync::atomic::AtomicBool::new(true)),
    );
}

#[test]
fn grit_2449_import_queue_checks_remaining_rounds() {
    assert_refused(&owner(2, 0), None);
}

#[test]
fn grit_2449_import_queue_checks_remaining_run_calls() {
    let policy = owner(2, 8).policy;
    let run = newt_core::agentic::run_allowance::RunAllowance::new(0);
    let owner = TurnAdmission::new(policy, Some(run.clone()));
    assert_refused(&owner, None);
    assert_eq!(run.remaining(), 0);
}

/// Missing prompt lineage cannot consume profile or shared recovery allowance.
#[test]
fn grit_2449_import_without_parent_does_not_propose_or_spend() {
    let owner = owner(2, 8);
    let mut profile_budget = 2;
    let (queued, _) = import_retry::queue(
        &mut profile_budget,
        2,
        None,
        "Repair".into(),
        Some(&owner),
        None,
    );
    assert!(queued.is_none());
    assert_eq!(profile_budget, 2);
    owner.reserve_model(None).unwrap();
    assert_eq!(
        owner.execution_receipt()["grit_used"],
        0,
        "unrelated later model work must not consume an unqueued import correction"
    );
}
