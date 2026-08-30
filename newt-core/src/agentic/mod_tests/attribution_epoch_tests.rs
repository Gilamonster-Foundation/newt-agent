use super::*;
use crate::attribution::AttributionLedger;
use std::cell::RefCell;

/// The canonical "A edits → C1 → A edits more → turn ends → switch B → C2"
/// regression (#1709 req 6): C2 must credit A + B. A's "edits more" (post-C1
/// work) re-records fresh after the C1 epoch clear, so it survives the turn
/// boundary into C2's snapshot; B is recorded after the switch. Without
/// the epoch clear (the old end-of-turn blanket clear), A's post-C1 record
/// was deduped against the pre-C1 entry and then wiped, so C2 lost A.
#[test]
fn epoch_clear_lets_post_commit_work_survive_to_the_next_commit() {
    let ledger = RefCell::new(AttributionLedger::new(
        crate::agent_identity::DEFAULT_AGENT_EMAIL,
    ));
    let attr: Option<&RefCell<AttributionLedger>> = Some(&ledger);
    let write = serde_json::json!({});
    let commit = serde_json::json!({"op": "commit"});

    // "A edits" — a non-read-only tool call records the active model A.
    ledger_note_attribution(attr, "model-a", "edit_file", &write, true);
    assert_eq!(ledger.borrow().contributors().len(), 1);
    assert_eq!(ledger.borrow().contributors()[0].model, "model-a");

    // "C1" — the commit call records A (the commit is non-read-only), THEN
    // the epoch clear consumes the whole ledger (the pre-commit contributors
    // are already credited on C1 via the loop-top snapshot).
    ledger_note_attribution(attr, "model-a", "git", &commit, true);
    ledger_consume_at_commit_epoch(attr, "git", &commit, true, "committed abc123 fix");
    assert!(
        ledger.borrow().is_empty(),
        "C1 epoch boundary consumed the ledger"
    );

    // "A edits more" — A re-records FRESH (the dedup set was reset by the
    // epoch clear), so A's post-C1 contribution is now pending.
    ledger_note_attribution(attr, "model-a", "edit_file", &write, true);
    assert_eq!(
        ledger.borrow().contributors().len(),
        1,
        "A re-recorded fresh after the epoch clear"
    );

    // "turn ends" — NO blanket clear (req 5: removed). The ledger survives
    // the turn boundary with A still pending.

    // "switch B" — B edits; B records alongside A.
    ledger_note_attribution(attr, "model-b", "edit_file", &write, true);
    let pending: Vec<String> = ledger
        .borrow()
        .contributors()
        .iter()
        .map(|c| c.model.clone())
        .collect();
    assert_eq!(
        pending,
        vec!["model-a".to_string(), "model-b".to_string()],
        "C2's snapshot credits A (edits more) + B: {pending:?}"
    );

    // "C2" — the loop-top snapshot of this ledger is exactly what the
    // finalizer merges with the active model, so C2 credits A + B (plus the
    // active-at-commit model, deduped). The epoch invariant holds.
    ledger_consume_at_commit_epoch(attr, "git", &commit, true, "committed def456 more");
    assert!(
        ledger.borrow().is_empty(),
        "C2 epoch boundary consumed the ledger"
    );
}

/// A FAILED commit consumes nothing (#1709 req 4) — the same contributors
/// remain pending for the next attempt.
#[test]
fn a_failed_commit_consumes_nothing() {
    let ledger = RefCell::new(AttributionLedger::new(
        crate::agent_identity::DEFAULT_AGENT_EMAIL,
    ));
    let attr: Option<&RefCell<AttributionLedger>> = Some(&ledger);
    let write = serde_json::json!({});
    let commit = serde_json::json!({"op": "commit"});

    ledger_note_attribution(attr, "model-a", "edit_file", &write, true);
    // A failed commit records nothing (ok=false) and consumes nothing.
    ledger_note_attribution(attr, "model-a", "git", &commit, false);
    ledger_consume_at_commit_epoch(attr, "git", &commit, false, "error: denied");
    assert_eq!(
        ledger.borrow().contributors().len(),
        1,
        "a failed commit must NOT consume the contributor"
    );
}

/// Only commit-PRODUCING git ops are epoch boundaries; read-only / staging /
/// ref ops create no commit and must not consume the ledger.
#[test]
fn only_commit_producing_git_ops_are_epoch_boundaries() {
    assert!(is_commit_producing_git_call(
        "git",
        &serde_json::json!({"op": "commit"})
    ));
    assert!(is_commit_producing_git_call(
        "git",
        &serde_json::json!({"op": "amend"})
    ));
    assert!(is_commit_producing_git_call(
        "git",
        &serde_json::json!({"op": "rebase"})
    ));
    // Read-only / staging / ref ops are NOT epoch boundaries.
    assert!(!is_commit_producing_git_call(
        "git",
        &serde_json::json!({"op": "status"})
    ));
    assert!(!is_commit_producing_git_call(
        "git",
        &serde_json::json!({"op": "log"})
    ));
    assert!(!is_commit_producing_git_call(
        "git",
        &serde_json::json!({"op": "diff"})
    ));
    assert!(!is_commit_producing_git_call(
        "git",
        &serde_json::json!({"op": "add"})
    ));
    assert!(!is_commit_producing_git_call(
        "git",
        &serde_json::json!({"op": "branch"})
    ));
    assert!(!is_commit_producing_git_call(
        "git",
        &serde_json::json!({"op": "checkout"})
    ));
    // A non-git tool is never a commit epoch.
    assert!(!is_commit_producing_git_call(
        "edit_file",
        &serde_json::json!({})
    ));
    assert!(!is_commit_producing_git_call(
        "run_command",
        &serde_json::json!({"command": "git commit"})
    ));
}

/// `parse_rebase_produced` reads the `produced` count out of the rebase
/// tool result string. It is the signal that distinguishes an attribution
/// epoch (`produced > 0`) from a successful-but-commitless history op
/// (`produced == 0`, e.g. an all-drop plan).
#[test]
fn parse_rebase_produced_reads_the_commit_count() {
    assert_eq!(
        parse_rebase_produced("rebased onto abc → def123 (3 commit(s), 1 dropped)"),
        Some(3)
    );
    // All-drop plan: zero commits produced.
    assert_eq!(
        parse_rebase_produced("rebased onto abc → def123 (0 commit(s), 2 dropped)"),
        Some(0)
    );
    // Unrecognized shape → None (caller falls back to the safe clear).
    assert_eq!(parse_rebase_produced("rebased onto abc"), None);
    assert_eq!(parse_rebase_produced(""), None);
}

/// The requested regression: a rebase that produced ZERO commits (an
/// all-drop plan) is a successful history operation but NOT an attribution
/// epoch. The pending contributor ledger/snapshot is PRESERVED — a later
/// commit in the same lifecycle still credits those contributors — and the
/// epoch clear does NOT fire. A rebase that produced > 0 IS an epoch: the
/// ledger is consumed.
#[test]
fn rebase_all_drop_preserves_pending_contributors() {
    let ledger = RefCell::new(AttributionLedger::new(
        crate::agent_identity::DEFAULT_AGENT_EMAIL,
    ));
    let attr: Option<&RefCell<AttributionLedger>> = Some(&ledger);
    let write = serde_json::json!({});
    let rebase = serde_json::json!({"op": "rebase"});

    // Model A does work → recorded as a pending contributor.
    ledger_note_attribution(attr, "model-a", "edit_file", &write, true);
    assert_eq!(ledger.borrow().contributors().len(), 1);

    // All-drop rebase: produced == 0. It is NOT an epoch — the ledger is
    // preserved (the contributor remains pending for a later commit).
    ledger_consume_at_commit_epoch(
        attr,
        "git",
        &rebase,
        true,
        "rebased onto abc → def123 (0 commit(s), 2 dropped)",
    );
    assert_eq!(
        ledger.borrow().contributors().len(),
        1,
        "a 0-produced rebase must NOT consume the pending contributor"
    );

    // A rebase that DID produce commits is an epoch — the ledger clears.
    ledger_consume_at_commit_epoch(
        attr,
        "git",
        &rebase,
        true,
        "rebased onto abc → def456 (2 commit(s), 0 dropped)",
    );
    assert!(
        ledger.borrow().is_empty(),
        "a >0-produced rebase IS an epoch: the ledger is consumed"
    );
}
