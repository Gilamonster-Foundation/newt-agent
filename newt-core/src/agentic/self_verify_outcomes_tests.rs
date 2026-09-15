//! #2315: the result-aware decision (`conclude`), the bounded tree state, and
//! the mutation-chain fallback. Pure: no filesystem, no process.

use super::*;
use crate::ExecOutcome::{Denied, Failed, Passed, TimedOut, Unavailable};
use crate::TurnEndReason::{RepairExhausted, VerificationIncomplete};
use content_addressable::ContentId;

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
        |l: &mut VerificationLedger| l.record_exec("touch notes.txt", Passed, None),
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
            &|path: &Path| files.get(path).map(|b| b.len() as u64),
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

/// A14: the receipt names the mode the gate instantiates from the switches the
/// host read, and the allowance only where it applies. Pure: no env.
#[test]
fn the_verification_receipt_names_the_mode_the_gate_runs_in() {
    assert_eq!(
        verification_receipt(true, true, false),
        serde_json::json!({"mode": "attempted"})
    );
    assert_eq!(
        verification_receipt(false, true, true),
        serde_json::json!({"mode": "off"})
    );
    assert_eq!(
        verification_receipt(true, true, true),
        serde_json::json!({"mode": "result_aware", "repair_allowance": VERIFY_REPAIR_ALLOWANCE})
    );
    assert_eq!(
        verification_receipt(true, false, true),
        serde_json::json!({"mode": "off"})
    );
}

/// The switch is opt-in. Pure: no test writes the process env, so the hosts
/// that read it each turn never race one.
#[test]
fn the_outcomes_policy_is_opt_in() {
    assert!(!outcomes_switch(None), "unset is off");
    for (value, on) in [
        ("1", true),
        (" ON ", true),
        ("true", true),
        ("0", false),
        ("banana", false),
    ] {
        assert_eq!(outcomes_switch(Some(value)), on, "{value:?}");
    }
}

// ---------------------------------------------------------------------------
// #2374 review: what counts as pass evidence.
// ---------------------------------------------------------------------------

fn decide_with(
    checks: &[VerifyCheck],
    ledger: &VerificationLedger,
    tree_now: Option<ContentId>,
) -> Decision {
    conclude(&Conclusion {
        checks,
        requested: &[],
        ledger,
        tree_now,
        repairs_used: 0,
        rounds_left: true,
    })
    .0
}

fn python_checks() -> Vec<VerifyCheck> {
    detect_checks(&["test_x.py".to_string()], "")
}

fn cargo_checks() -> Vec<VerifyCheck> {
    detect_checks(&["Cargo.toml".to_string()], "")
}

/// Finding 1: a command that merely mentions a check's file is not a run of
/// the check. After a failing `pytest`, `cat test_x.py` (exit 0) must not read
/// as a fresh pass.
#[test]
fn a_command_that_mentions_the_test_file_is_not_a_pass() {
    let mut ledger = VerificationLedger::default();
    ledger.record_exec("pytest test_x.py", Failed, None);
    ledger.record_write();
    ledger.record_exec("cat test_x.py", Passed, Some(id("tree-t")));
    let decision = decide_with(&python_checks(), &ledger, Some(id("tree-t")));
    assert_ne!(decision, Decision::Accept, "{decision:?}");
}

/// Finding 1: after a failure, a different passing run does not clear it, and
/// a run that executes no tests is not a pass. Twins: the same command (modulo
/// whitespace) clears it, and a `cd dir && runner` form is a pass.
#[test]
fn a_narrower_or_non_running_pass_does_not_clear_a_failure() {
    let t = || Some(id("tree-t"));
    let mut narrowed = VerificationLedger::default();
    narrowed.record_exec("pytest", Failed, None);
    narrowed.record_exec("pytest test_a.py::test_one", Passed, t());
    assert_ne!(
        decide_with(&python_checks(), &narrowed, t()),
        Decision::Accept
    );

    let mut no_run = VerificationLedger::default();
    no_run.record_exec("cargo test --no-run", Passed, t());
    assert_ne!(decide_with(&cargo_checks(), &no_run, t()), Decision::Accept);

    for (failed, passed) in [
        ("pytest", "pytest"),
        ("pytest  test_a.py", "pytest test_a.py"),
    ] {
        let mut ledger = VerificationLedger::default();
        ledger.record_exec(failed, Failed, None);
        ledger.record_exec(passed, Passed, t());
        assert_eq!(
            decide_with(&python_checks(), &ledger, t()),
            Decision::Accept,
            "{failed} then {passed}"
        );
    }
    let mut cd = VerificationLedger::default();
    cd.record_exec("cd backend && cargo test -p api", Passed, t());
    assert_eq!(decide_with(&cargo_checks(), &cd, t()), Decision::Accept);
}

/// Finding 2: a pass whose exit status a later pipe, `||`, `;` or `&` hides is
/// unverified, and the nudge says to run it unmasked. Twins: redirects and a
/// leading `cd dir &&` keep the check as the command's last word.
#[test]
fn a_status_masked_pass_is_unverified() {
    let t = || Some(id("tree-t"));
    for masked in [
        "cargo test 2>&1 | tail -30",
        "cargo test || true",
        "cargo test; echo done",
        "cargo test & wait",
    ] {
        let mut ledger = VerificationLedger::default();
        ledger.record_exec(masked, Passed, t());
        let decision = decide_with(&cargo_checks(), &ledger, t());
        match &decision {
            Decision::Nudge(text) => assert!(text.contains("exit status"), "{masked}: {text}"),
            other => panic!("{masked}: expected a nudge, got {other:?}"),
        }
    }
    for honest in [
        "cargo test 2>&1",
        "cd api && cargo test",
        "cargo test > out.txt 2>&1",
    ] {
        let mut ledger = VerificationLedger::default();
        ledger.record_exec(honest, Passed, t());
        assert_eq!(
            decide_with(&cargo_checks(), &ledger, t()),
            Decision::Accept,
            "{honest}"
        );
    }
}

/// Finding 5: every call that may change the workspace is a mutation for the
/// fallback chain, and a check-matching command that is not a plain run of the
/// check is not exempt from it.
#[tokio::test]
async fn the_mutation_chain_counts_every_workspace_changing_call() {
    let ws = "newt-core-test-workspace-that-does-not-exist";
    let run_json = |command: &str| serde_json::json!({ "command": command });
    let mut deleted = VerificationLedger::for_turn("", true);
    deleted
        .observe(
            "run_command",
            &run_json("cargo test"),
            true,
            Some(Passed),
            ws,
        )
        .await;
    deleted
        .observe(
            "delete_file",
            &serde_json::json!({"path": "src/lib.rs"}),
            true,
            None,
            ws,
        )
        .await;
    let mut sed = VerificationLedger::for_turn("", true);
    sed.observe("run_command", &run_json("pytest"), true, Some(Passed), ws)
        .await;
    sed.observe(
        "run_command",
        &run_json("sed -i s/a/b/ tests/test_util.py"),
        true,
        Some(Passed),
        ws,
    )
    .await;
    assert_ne!(
        decide_with(&cargo_checks(), &deleted, None),
        Decision::Accept,
        "delete_file"
    );
    assert_ne!(
        decide_with(&python_checks(), &sed, None),
        Decision::Accept,
        "sed -i"
    );
}

/// Finding 6: the byte bound is checked against a file's size BEFORE it is
/// read, so peak memory never follows the largest file in the workspace.
#[test]
fn tree_state_never_reads_a_file_past_the_byte_budget() {
    use std::path::Path;
    let state = tree_state(
        Path::new("/ws"),
        &|_: &Path| vec![("huge.bin".to_string(), false)],
        &|_: &Path| Some(1 << 40),
        &|path: &Path| panic!("read {} although it exceeds the budget", path.display()),
        100,
        1 << 20,
    );
    assert_eq!(state, None);
}

/// Finding 7: the receipt reports a mode only for a turn that has a gate to
/// run it. Without SmartHarness the Ollama and Responses loops carry none, so
/// an A/B split on the receipt must see them as `off`, not treated. Reads the
/// typed wire API, never `NEWT_OPENAI_API`.
#[test]
fn the_receipt_says_off_where_the_loop_has_no_gate() {
    use crate::BackendKind::{Anthropic, Ollama, Openai};
    use crate::OpenAiApi::{ChatCompletions, Responses};
    let mode = |kind, api, smart| {
        verification_receipt(verification_gate_present(kind, api, smart), true, true)["mode"]
            .clone()
    };
    assert_eq!(mode(Ollama, ChatCompletions, false), "off");
    assert_eq!(mode(Ollama, ChatCompletions, true), "result_aware");
    assert_eq!(mode(Openai, ChatCompletions, false), "result_aware");
    assert_eq!(mode(Anthropic, ChatCompletions, false), "result_aware");
    assert_eq!(
        mode(Openai, Responses, false),
        "off",
        "Responses has no ordinary gate"
    );
    assert_eq!(mode(Openai, Responses, true), "result_aware");
}

// ---------------------------------------------------------------------------
// #2374 review round two, C and E: one pass-evidence rule. Restored in round
// three (3d8aa43e dropped them) and extended by its shapes.
// ---------------------------------------------------------------------------

fn standing(
    checks: &[VerifyCheck],
    runs: &[(&str, crate::ExecOutcome)],
    tree_now: Option<ContentId>,
) -> CheckStatus {
    let mut ledger = VerificationLedger::default();
    for (command, outcome) in runs {
        let tree = (*outcome == Passed)
            .then(|| id("tree-t"))
            .filter(|_| tree_now.is_some());
        ledger.record_exec(command, *outcome, tree);
    }
    let (_, report) = conclude(&Conclusion {
        checks,
        requested: &[],
        ledger: &ledger,
        tree_now,
        repairs_used: 0,
        rounds_left: true,
    });
    report.checks[0].status
}

/// Checks, their runs in order, and the standing the rule must give.
type Case<'a> = (
    &'a [VerifyCheck],
    &'a [(&'a str, crate::ExecOutcome)],
    CheckStatus,
);

fn named(command: &str) -> Vec<VerifyCheck> {
    detect_checks(&[], &format!("You can run `{command}` to verify."))
}

/// C: a run is pass evidence only when its runner segment is the last one, it
/// is not backgrounded, no earlier `||` can skip it, and it is not a non-run
/// form. A failure is cleared only by the same normalized full command, a
/// denial never erases a pass or a failure, and an unverified re-run keeps a
/// genuine pass.
#[test]
fn one_rule_decides_pass_evidence_for_every_command_shape() {
    use CheckStatus::{Failed as F, NeverRun, Passed as P, Unverified as U};
    let t = || Some(id("tree-t"));
    let cargo = cargo_checks();
    let python = python_checks();
    let go = detect_checks(&["go.mod".to_string()], "");
    let make = detect_checks(&["Makefile".to_string()], "");
    let named_make = named("make");
    let named_pytest = named("pytest");
    let cases: &[Case] = &[
        (&cargo, &[("cargo test", Passed)], P),
        (&cargo, &[("cd backend && cargo test", Passed)], P),
        (&cargo, &[("cargo test 2>&1", Passed)], P),
        (&cargo, &[("RUST_BACKTRACE=1 cargo test", Passed)], P),
        (
            &cargo,
            &[("cargo test && sed -i s/a/b/ src/lib.rs", Passed)],
            U,
        ),
        (&cargo, &[("cargo test && rm src/lib.rs", Passed)], U),
        (&cargo, &[("cargo test 2>&1 | tail -30", Passed)], U),
        (&cargo, &[("cargo test; echo done", Passed)], U),
        (&cargo, &[("cargo test &", Passed)], U),
        (&cargo, &[("true || cargo test", Passed)], U),
        (&cargo, &[("cargo test --no-run", Passed)], U),
        (&cargo, &[("cargo test -- --list", Passed)], U),
        (&cargo, &[("cargo nextest list", Passed)], U),
        (&python, &[("pytest --collect-only", Passed)], U),
        (&python, &[("pytest --co -q", Passed)], U),
        (&python, &[("pytest --version", Passed)], U),
        (&go, &[("go test -c ./...", Passed)], U),
        (&go, &[("go test -list .", Passed)], U),
        (
            &python,
            &[("pytest", Failed), ("pytest test_a.py", Passed)],
            F,
        ),
        (
            &python,
            &[("pytest test_a.py", Failed), ("pytest", Passed)],
            F,
        ),
        (
            &cargo,
            &[("cargo test --workspace", Failed), ("cargo test", Passed)],
            F,
        ),
        (
            &cargo,
            &[("cd x && cargo test", Failed), ("cargo test", Passed)],
            F,
        ),
        (
            &python,
            &[("PYTEST_ADDOPTS=-x pytest", Failed), ("pytest", Passed)],
            F,
        ),
        (
            &cargo,
            &[("cargo test", Failed), ("cargo  test", Passed)],
            P,
        ),
        (&cargo, &[("cargo test", Failed), ("cargo test", Denied)], F),
        (
            &cargo,
            &[("cargo test", Failed), ("cargo test | tail", Passed)],
            F,
        ),
        (
            &cargo,
            &[("cargo test", Passed), ("cargo test | tail", Passed)],
            P,
        ),
        (&named_make, &[("make install", Passed)], NeverRun),
        (&named_make, &[("make -j4", Passed)], P),
        // Round three, item 5: a denied or unavailable re-run erases no pass.
        (&cargo, &[("cargo test", Passed), ("cargo test", Denied)], P),
        (
            &cargo,
            &[("cargo test", Passed), ("cargo test", Unavailable)],
            P,
        ),
        // Item 6: `-C dir` is a real `go test` run; `-c` builds without running.
        (&go, &[("go test -C sub ./...", Passed)], P),
        // Item 6: a named check with a separator or a prefix matches its own
        // command.
        (
            &named("cd app && pytest"),
            &[("cd app && pytest", Passed)],
            P,
        ),
        (
            &named("make build; make test"),
            &[("make build; make test", Passed)],
            P,
        ),
        (&named("time make test"), &[("time make test", Passed)], P),
        // Item 11: an escaped quote inside double quotes does not end them.
        (
            &python,
            &[(r#"pytest -k "not \"slow\"" | tail -5"#, Passed)],
            U,
        ),
        // Item 12: more forms that run no test.
        (&python, &[("pytest -h", Passed)], U),
        (&python, &[("pytest --collectonly", Passed)], U),
        (&python, &[("pytest --fixtures", Passed)], U),
        (&python, &[("pytest --markers", Passed)], U),
        (&python, &[("pytest --setup-plan", Passed)], U),
        (&cargo, &[("cargo test -h", Passed)], U),
        (&cargo, &[("cargo test --help", Passed)], U),
        (&cargo, &[("cargo nextest archive", Passed)], U),
        (&cargo, &[("cargo nextest show-config", Passed)], U),
        (&go, &[("go test -list=. ./...", Passed)], U),
        (&go, &[("go test --list .", Passed)], U),
        (&go, &[("go test -n ./...", Passed)], U),
        (&make, &[("make test -n", Passed)], U),
        (&named_make, &[("make -n", Passed)], U),
        (&named_pytest, &[("pytest --collect-only", Passed)], U),
        (&named_pytest, &[("pytest -x", Passed)], P),
    ];
    let wrong: Vec<String> = cases
        .iter()
        .filter_map(|(checks, runs, expected)| {
            let got = standing(checks, runs, t());
            (got != *expected).then(|| format!("{runs:?}: {got:?}, expected {expected:?}"))
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// E: on the mutation-chain basis, a read-only probe after a pass is not a
/// mutation, so it does not cost a repair nudge. A failed mutating call still
/// counts. Round three, item 10: `sed` is never a read here (`-ri`, `-Ei`,
/// `--in-place` and the `w` command all write), nor are `find`'s file-writing
/// actions, `git --output=` or `tree -o`, however the token is quoted.
#[test]
fn a_read_only_probe_after_a_pass_does_not_stale_it_on_the_chain() {
    let cargo = cargo_checks();
    let after_pass =
        |command: &str| standing(&cargo, &[("cargo test", Passed), (command, Passed)], None);
    let mut wrong: Vec<String> = [
        "cat src/lib.rs",
        "ls",
        "git status",
        "grep -o fn src/lib.rs",
        "find . -name a -o -name b",
    ]
    .into_iter()
    .filter(|probe| after_pass(probe) != CheckStatus::Passed)
    .map(|probe| format!("{probe}: a read staled the pass"))
    .collect();
    assert_eq!(
        standing(
            &cargo,
            &[("cargo test", Passed), ("rm src/lib.rs", Failed)],
            None
        ),
        CheckStatus::Stale,
        "a failed mutating call still counts"
    );
    for writer in [
        "sed -n 1p src/lib.rs",
        "sed -ri s/a/b/ src/lib.rs",
        "sed -Ei s/a/b/ src/lib.rs",
        "sed --in-place s/a/b/ src/lib.rs",
        "find . -fprint out.txt",
        "find . -fprint0 out.txt",
        "find . -fprintf out.txt %p",
        "find . -fls out.txt",
        "find . -okdir rm {} +",
        "find . -name x \"-delete\"",
        "find . -name x -exe\\c rm {} +",
        "git diff --output=patch.txt",
        "tree -o out.txt",
    ] {
        if after_pass(writer) != CheckStatus::Stale {
            wrong.push(format!("{writer}: counted as a read"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// Round three, item 4: an alias is classified as the tool it reaches. A shell
/// alias runs its `command`, so `bash` running `cargo test` is the check's run
/// and `sh` running `ls` is a read; a corrective alias (`str_replace`) and a
/// rewrite to an inert tool (`get_plan`) change nothing.
#[tokio::test]
async fn an_alias_is_classified_as_the_tool_it_reaches() {
    let ws = "newt-core-test-workspace-that-does-not-exist";
    let mut ledger = VerificationLedger::for_turn("", true);
    let command = |c: &str| serde_json::json!({ "command": c });
    ledger
        .observe("bash", &command("cargo test"), true, Some(Passed), ws)
        .await;
    ledger
        .observe("sh", &command("ls"), true, Some(Passed), ws)
        .await;
    for name in ["str_replace", "get_plan"] {
        ledger
            .observe(
                name,
                &serde_json::json!({"path": "src/lib.rs"}),
                true,
                None,
                ws,
            )
            .await;
    }
    assert_eq!(
        decide_with(&cargo_checks(), &ledger, None),
        Decision::Accept
    );
}
