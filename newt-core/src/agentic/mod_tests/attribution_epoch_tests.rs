use super::*;
use crate::attribution::AttributionLedger;
use std::cell::RefCell;

fn ledger() -> RefCell<AttributionLedger> {
    RefCell::new(AttributionLedger::new(
        crate::agent_identity::DEFAULT_AGENT_EMAIL,
    ))
}

fn edit(ledger: &RefCell<AttributionLedger>, model: &str) {
    ledger_note_attribution(
        Some(ledger),
        model,
        "edit_file",
        &serde_json::json!({}),
        true,
    );
}

/// A edits -> C1 -> A edits more -> switch B -> C2 must credit A + B.
#[test]
fn epoch_clear_lets_post_commit_work_survive_to_the_next_commit() {
    let ledger = ledger();
    let git = ConfirmedGit::default();
    edit(&ledger, "model-a");
    let epoch = AttributionEpoch::new(Some(&ledger), Some(&git), "model-a", "git");
    git.publish();
    epoch.finish(&serde_json::json!({"op":"commit"}), true);
    assert!(ledger.borrow().is_empty());

    edit(&ledger, "model-a");
    edit(&ledger, "model-b");
    assert_eq!(pending_models(&ledger), ["model-a", "model-b"]);
    let epoch = AttributionEpoch::new(Some(&ledger), Some(&git), "model-b", "git");
    git.publish();
    epoch.finish(&serde_json::json!({"op":"commit"}), true);
    assert!(ledger.borrow().is_empty());
}

#[test]
fn a_failed_commit_consumes_nothing() {
    let ledger = ledger();
    let git = ConfirmedGit::default();
    edit(&ledger, "model-a");
    let epoch = AttributionEpoch::new(Some(&ledger), Some(&git), "model-b", "git");
    epoch.finish(&serde_json::json!({"op":"commit"}), false);
    assert_eq!(pending_models(&ledger), ["model-a"]);
}

#[test]
fn operations_without_publication_preserve_contributors() {
    for op in [
        "status", "log", "diff", "add", "branch", "checkout", "stash", "rebase",
    ] {
        let ledger = ledger();
        let git = ConfirmedGit::default();
        edit(&ledger, "model-a");
        let epoch = AttributionEpoch::new(Some(&ledger), Some(&git), "model-a", "git");
        epoch.finish(&serde_json::json!({"op":op}), true);
        assert_eq!(
            pending_models(&ledger),
            ["model-a"],
            "{op} without a confirmed commit"
        );
    }
}

#[test]
fn confirmed_embedded_publication_consumes_without_result_parsing() {
    for (op, ok) in [
        ("commit", true),
        ("amend", true),
        ("rebase", true),
        ("commit", false),
    ] {
        let ledger = ledger();
        let git = ConfirmedGit::default();
        edit(&ledger, "earlier-model");
        let epoch = AttributionEpoch::new(Some(&ledger), Some(&git), "active-model", "git");
        git.publish();
        epoch.finish(&serde_json::json!({"op":op}), ok);
        assert!(ledger.borrow().is_empty(), "confirmed {op}, result ok={ok}");
    }
}

#[test]
fn rebase_all_drop_preserves_pending_contributors() {
    let ledger = ledger();
    let git = ConfirmedGit::default();
    edit(&ledger, "model-a");
    let epoch = AttributionEpoch::new(Some(&ledger), Some(&git), "model-a", "git");
    // LocalGitTool emits no success event for a produced==0 rebase.
    epoch.finish(&serde_json::json!({"op":"rebase"}), true);
    assert_eq!(pending_models(&ledger), ["model-a"]);
    let epoch = AttributionEpoch::new(Some(&ledger), Some(&git), "model-a", "git");
    git.publish();
    epoch.finish(&serde_json::json!({"op":"rebase"}), true);
    assert!(ledger.borrow().is_empty());
}

#[test]
fn an_earlier_success_is_not_consumed_again_during_a_later_call() {
    let ledger = ledger();
    let git = ConfirmedGit::default();
    git.publish();
    edit(&ledger, "post-commit-model");
    let epoch = AttributionEpoch::new(Some(&ledger), Some(&git), "next-model", "edit_file");
    epoch.finish(&serde_json::json!({}), true);
    assert_eq!(pending_models(&ledger), ["post-commit-model", "next-model"]);
}

#[test]
fn attribution_without_a_git_collaborator_still_records_material_work() {
    let ledger = ledger();
    let epoch = AttributionEpoch::new(Some(&ledger), None, "model-a", "edit_file");
    epoch.finish(&serde_json::json!({}), true);
    assert_eq!(pending_models(&ledger), ["model-a"]);
}

#[derive(Default)]
struct ConfirmedGit {
    commits: std::sync::atomic::AtomicUsize,
}

impl ConfirmedGit {
    fn publish(&self) {
        self.commits
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl GitTool for ConfirmedGit {
    fn confirmed_commit_count(&self) -> usize {
        self.commits.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn dispatch(
        &self,
        _op: &str,
        _args: &serde_json::Value,
        _caveats: &crate::git_caveats::GitCaveats,
        _session: &crate::caveats::Caveats,
    ) -> Result<String, String> {
        unreachable!("the fixture publishes the same host-only success signal directly")
    }
}

fn pending_models(ledger: &RefCell<AttributionLedger>) -> Vec<String> {
    ledger
        .borrow()
        .contributors()
        .iter()
        .map(|c| c.model.clone())
        .collect()
}

#[test]
fn confirmed_native_epoch_consumes_foreign_credit_even_when_the_tail_fails() {
    let ledger = RefCell::new(AttributionLedger::new(
        crate::agent_identity::DEFAULT_AGENT_EMAIL,
    ));
    let git = ConfirmedGit::default();
    ledger_note_attribution(
        Some(&ledger),
        "earlier-model",
        "edit_file",
        &serde_json::json!({}),
        true,
    );
    let epoch = AttributionEpoch::new(Some(&ledger), Some(&git), "active-model", "run_command");
    git.publish();
    // Native Git landed, but a later command's exit status makes the whole
    // shell tool fail. The result is deliberately unrelated to Git's output.
    epoch.finish(
        &serde_json::json!({"command":"git commit -m change && false"}),
        false,
    );
    assert_eq!(
        pending_models(&ledger),
        ["active-model"],
        "consume old credit, retain possible post-commit effects"
    );

    ledger_note_attribution(
        Some(&ledger),
        "active-model",
        "edit_file",
        &serde_json::json!({}),
        true,
    );
    ledger_note_attribution(
        Some(&ledger),
        "next-model",
        "edit_file",
        &serde_json::json!({}),
        true,
    );
    assert_eq!(
        pending_models(&ledger),
        ["active-model", "next-model"],
        "later work survives the next turn's snapshot"
    );
}

#[test]
fn a_successful_tool_message_without_a_confirmed_commit_never_consumes_credit() {
    let ledger = RefCell::new(AttributionLedger::new(
        crate::agent_identity::DEFAULT_AGENT_EMAIL,
    ));
    let git = ConfirmedGit::default();
    ledger_note_attribution(
        Some(&ledger),
        "earlier-model",
        "edit_file",
        &serde_json::json!({}),
        true,
    );
    let epoch = AttributionEpoch::new(Some(&ledger), Some(&git), "active-model", "git");
    epoch.finish(&serde_json::json!({"op":"commit"}), true);
    assert_eq!(
        pending_models(&ledger),
        ["earlier-model", "active-model"],
        "output text is not publication evidence"
    );
}

#[test]
fn confirmed_native_epoch_is_observed_when_tool_scope_is_cancelled() {
    let ledger = RefCell::new(AttributionLedger::new(
        crate::agent_identity::DEFAULT_AGENT_EMAIL,
    ));
    let git = ConfirmedGit::default();
    ledger_note_attribution(
        Some(&ledger),
        "earlier-model",
        "edit_file",
        &serde_json::json!({}),
        true,
    );
    {
        let _epoch =
            AttributionEpoch::new(Some(&ledger), Some(&git), "active-model", "run_command");
        git.publish();
        // An early return/cancel drops the scope before normal result accounting.
    }
    assert_eq!(pending_models(&ledger), ["active-model"]);
}
