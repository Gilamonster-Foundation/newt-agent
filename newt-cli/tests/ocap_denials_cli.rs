//! Process-level acceptance for the denial repair journal presentation.

use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn ocap_denials_classifies_a_failed_grant_and_shows_its_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("denials.jsonl");
    let record = newt_core::denial_journal::DenialRecord {
        ts_claim: "2026-07-23T22:00:00Z".into(),
        command: "wc -l src/lib.rs".into(),
        cwd: "/workspace".into(),
        stage: newt_core::denial_journal::DenialStage::AfterGrant,
        denials: vec![newt_core::denial_journal::JournalDenial {
            kind: "exec".into(),
            target: "confinement".into(),
            reason: "exec of \"confinement\" is not within the granted authority".into(),
        }],
    };
    record.append_jsonl(&journal).unwrap();

    Command::cargo_bin("newt")
        .unwrap()
        .args(["ocap", "denials", "--journal", journal.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("[grant-retry] exec:confinement"))
        .stdout(predicate::str::contains("wc -l src/lib.rs"))
        .stdout(predicate::str::contains("Do not add it to policy"));
}
