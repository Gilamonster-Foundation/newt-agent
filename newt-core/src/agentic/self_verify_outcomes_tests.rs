//! #2315: the result-aware decision (`conclude`), the bounded tree state, and
//! the mutation-chain fallback. Pure: no filesystem, no process.

use super::*;
use crate::ExecOutcome::{Denied, Failed, Passed, TimedOut, Unavailable};
use crate::TurnEndReason::{RepairExhausted, VerificationIncomplete};
use content_addressable::{ContentAddressable, ContentId};

const CHECK: &str = "sh -c true";
const OTHER: &str = "git status";

fn checks() -> Vec<VerifyCheck> {
    detect_checks(&[], &format!("You can run `{CHECK}` to verify."))
}

fn id(tag: &str) -> ContentId {
    crate::event_journal::MerkleNode::genesis(tag.to_string())
        .id()
        .unwrap()
}

fn decide(
    ledger: &VerificationLedger,
    requested: &[&str],
    tree_now: Option<ContentId>,
    repairs_used: usize,
    rounds_left: bool,
) -> (Decision, VerificationReport) {
    let requested: Vec<String> = requested.iter().map(|c| c.to_string()).collect();
    conclude(&Conclusion {
        checks: &checks(),
        requested: &requested,
        ledger,
        tree_now,
        repairs_used,
        rounds_left,
    })
}

fn nudge(decision: &Decision) -> &str {
    match decision {
        Decision::Nudge(text) => text,
        other => panic!("expected a nudge, got {other:?}"),
    }
}

#[test]
fn a9_no_detected_checks_accepts_silently() {
    let ledger = VerificationLedger::default();
    let decision = conclude(&Conclusion {
        checks: &[],
        requested: &[],
        ledger: &ledger,
        tree_now: None,
        repairs_used: 0,
        rounds_left: true,
    })
    .0;
    assert_eq!(decision, Decision::Accept);
}

#[test]
fn a7_a_fresh_pass_accepts() {
    let mut ledger = VerificationLedger::default();
    ledger.record_exec(CHECK, Passed, Some(id("tree-a")));
    let (decision, report) = decide(&ledger, &[CHECK], Some(id("tree-a")), 0, true);
    assert_eq!(decision, Decision::Accept);
    assert_eq!(report.basis, StateBasis::Tree);
}

/// A4/A10: a failure is repaired with a numbered nudge until the allowance is
/// spent, then the turn stops `RepairExhausted`.
#[test]
fn a4_a_failure_is_repaired_until_the_allowance_then_exhausts() {
    let mut ledger = VerificationLedger::default();
    ledger.record_exec(CHECK, Failed, None);
    for used in 0..VERIFY_REPAIR_ALLOWANCE {
        let (decision, _) = decide(&ledger, &[CHECK], None, used, true);
        let text = nudge(&decision);
        assert!(
            text.contains(&format!(
                "(verification repair {}/{VERIFY_REPAIR_ALLOWANCE})",
                used + 1
            )),
            "{text}"
        );
        assert!(text.contains(CHECK), "the nudge names the check: {text}");
    }
    let (decision, _) = decide(&ledger, &[CHECK], None, VERIFY_REPAIR_ALLOWANCE, true);
    assert_eq!(decision, Decision::Stop(RepairExhausted));
}

/// A5: a timeout is named as a timeout; its twin, a failure, is not.
#[test]
fn a5_a_timeout_repair_names_the_timeout() {
    let mut timed_out = VerificationLedger::default();
    timed_out.record_exec(CHECK, TimedOut, None);
    let (decision, _) = decide(&timed_out, &[CHECK], None, 0, true);
    assert!(nudge(&decision).contains("timed out"));
    let mut failed = VerificationLedger::default();
    failed.record_exec(CHECK, Failed, None);
    let (decision, _) = decide(&failed, &[CHECK], None, 0, true);
    assert!(!nudge(&decision).contains("timed out"));
}

/// A3: a denied or unavailable check never enters repair and is never a pass,
/// even with allowance and rounds left.
#[test]
fn a3_denied_and_unavailable_stop_incomplete_without_repair() {
    for outcome in [Denied, Unavailable] {
        let mut ledger = VerificationLedger::default();
        ledger.record_exec(CHECK, outcome, None);
        let (decision, _) = decide(&ledger, &[CHECK], None, 0, true);
        assert_eq!(
            decision,
            Decision::Stop(VerificationIncomplete),
            "{outcome:?}"
        );
    }
}

/// A2: a check the model requested but that never executed (a rejected batch)
/// is incomplete, never a pass.
#[test]
fn a2_requested_but_unexecuted_is_incomplete() {
    let (decision, _) = decide(&VerificationLedger::default(), &[CHECK], None, 0, true);
    assert_eq!(decision, Decision::Stop(VerificationIncomplete));
}

/// A check nobody ran or requested gets a verification nudge, not a repair.
#[test]
fn a_never_run_check_gets_a_verify_nudge_not_a_repair() {
    let (decision, _) = decide(&VerificationLedger::default(), &[], None, 0, true);
    let text = nudge(&decision);
    assert!(text.contains(CHECK), "{text}");
    assert!(!text.contains("verification repair"), "{text}");
}

/// A11: with no round left the policy stops, never nudges.
#[test]
fn a11_the_final_round_stops_instead_of_nudging() {
    let mut failed = VerificationLedger::default();
    failed.record_exec(CHECK, Failed, None);
    assert_eq!(
        decide(&failed, &[CHECK], None, 0, false).0,
        Decision::Stop(RepairExhausted)
    );
    assert_eq!(
        decide(&VerificationLedger::default(), &[], None, 0, false).0,
        Decision::Stop(VerificationIncomplete)
    );
}

/// A8 (tree basis): a pass is stale exactly when the tree changed since it ran.
#[test]
fn a8_tree_basis_a_changed_tree_makes_the_pass_stale() {
    let mut ledger = VerificationLedger::default();
    ledger.record_exec(CHECK, Passed, Some(id("tree-a")));
    ledger.record_exec(OTHER, Passed, None);
    let (decision, report) = decide(&ledger, &[CHECK], Some(id("tree-a")), 0, true);
    assert_eq!(
        decision,
        Decision::Accept,
        "a read-only command changed no bytes"
    );
    assert_eq!(report.basis, StateBasis::Tree);
    let (decision, _) = decide(&ledger, &[CHECK], Some(id("tree-b")), 0, true);
    assert!(nudge(&decision).contains(CHECK));
}

/// A8 (fallback): when a tree state is unavailable (bounds exceeded), the
/// mutation chain decides, and the report says so. A write or a non-check
/// command after the pass makes it stale; another check's run does not.
#[test]
fn a8_the_mutation_chain_decides_when_the_tree_is_unavailable() {
    let mut pass = VerificationLedger::default();
    pass.record_exec(CHECK, Passed, None);
    let (decision, clean) = decide(&pass, &[CHECK], None, 0, true);
    assert_eq!(decision, Decision::Accept);
    assert_eq!(clean.basis, StateBasis::MutationChain);

    let mut rerun = VerificationLedger::default();
    rerun.record_exec(CHECK, Passed, None);
    rerun.record_exec(CHECK, Passed, None);
    let (decision, rerun_report) = decide(&rerun, &[CHECK], None, 0, true);
    assert_eq!(decision, Decision::Accept);
    assert_eq!(
        rerun_report.state_now, clean.state_now,
        "a check run is not a mutation"
    );

    for mutate in [
        |l: &mut VerificationLedger| l.record_write(),
        |l: &mut VerificationLedger| l.record_exec(OTHER, Passed, None),
    ] {
        let mut stale = VerificationLedger::default();
        stale.record_exec(CHECK, Passed, None);
        mutate(&mut stale);
        let (decision, report) = decide(&stale, &[CHECK], None, 0, true);
        assert!(nudge(&decision).contains(CHECK));
        assert_ne!(
            report.state_now, clean.state_now,
            "a mutation moves the head"
        );
    }
}

/// The tree state is a pure function of (path, bytes) under the scan bounds.
#[test]
fn tree_state_follows_bytes_and_gives_up_past_its_bounds() {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    let tree = |files: &[(&str, &str)], max_entries: usize, max_bytes: u64| {
        let files: BTreeMap<PathBuf, Vec<u8>> = files
            .iter()
            .map(|(p, b)| (Path::new("/ws").join(p), b.as_bytes().to_vec()))
            .collect();
        let list = |dir: &Path| {
            let mut names = BTreeMap::new();
            for path in files.keys() {
                if let Ok(rest) = path.strip_prefix(dir) {
                    let mut parts = rest.components();
                    let first = parts
                        .next()
                        .unwrap()
                        .as_os_str()
                        .to_string_lossy()
                        .into_owned();
                    names.insert(first, parts.next().is_some());
                }
            }
            names.into_iter().collect::<Vec<_>>()
        };
        tree_state(
            Path::new("/ws"),
            &list,
            &|path: &Path| files.get(path).cloned(),
            max_entries,
            max_bytes,
        )
    };
    let base = [("src/lib.rs", "fn a() {}"), ("Cargo.toml", "[package]")];
    let a = tree(&base, 100, 1 << 20).expect("within bounds");
    assert_eq!(tree(&base, 100, 1 << 20), Some(a), "deterministic");
    let edited = [("src/lib.rs", "fn b() {}"), ("Cargo.toml", "[package]")];
    assert_ne!(
        tree(&edited, 100, 1 << 20),
        Some(a),
        "a byte change moves the id"
    );
    let built = [
        ("src/lib.rs", "fn a() {}"),
        ("Cargo.toml", "[package]"),
        ("target/debug/out", "artifact"),
    ];
    assert_eq!(
        tree(&built, 100, 1 << 20),
        Some(a),
        "SKIP_DIRS are not state"
    );
    assert_eq!(tree(&base, 1, 1 << 20), None, "entry bound exceeded");
    assert_eq!(tree(&base, 100, 4), None, "byte bound exceeded");
}

#[serial_test::serial(newt_self_verify_env)]
#[test]
fn the_outcomes_policy_is_opt_in() {
    let saved = std::env::var_os("NEWT_VERIFY_OUTCOMES");
    std::env::remove_var("NEWT_VERIFY_OUTCOMES");
    assert!(!outcomes_enabled(), "unset is off");
    for (value, on) in [
        ("1", true),
        ("on", true),
        ("true", true),
        ("0", false),
        ("banana", false),
    ] {
        std::env::set_var("NEWT_VERIFY_OUTCOMES", value);
        assert_eq!(outcomes_enabled(), on, "{value:?}");
    }
    match saved {
        Some(v) => std::env::set_var("NEWT_VERIFY_OUTCOMES", v),
        None => std::env::remove_var("NEWT_VERIFY_OUTCOMES"),
    }
}
