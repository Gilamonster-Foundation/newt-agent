//! Process-level acceptance for the denial repair journal presentation.

use assert_cmd::Command;
use predicates::prelude::*;
use std::path::Path;

fn record(command: &str) -> newt_core::denial_journal::DenialRecord {
    newt_core::denial_journal::DenialRecord {
        ts_claim: "2026-07-23T22:00:00Z".into(),
        command: command.into(),
        cwd: "/workspace".into(),
        stage: newt_core::denial_journal::DenialStage::AfterGrant,
        denials: vec![newt_core::denial_journal::JournalDenial {
            kind: "exec".into(),
            target: "confinement".into(),
            reason: "exec of \"confinement\" is not within the granted authority".into(),
        }],
    }
}

fn denials(journal: &Path) -> Command {
    let mut cmd = Command::cargo_bin("newt").unwrap();
    cmd.args(["ocap", "denials", "--journal", journal.to_str().unwrap()]);
    cmd
}

#[test]
fn ocap_denials_classifies_a_failed_grant_and_shows_its_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("denials.jsonl");
    newt_core::denial_journal::append_record(&journal, record("wc -l src/lib.rs")).unwrap();

    denials(&journal)
        .assert()
        .success()
        .stdout(predicate::str::contains("[grant-retry] exec:confinement"))
        .stdout(predicate::str::contains("wc -l src/lib.rs"))
        .stdout(predicate::str::contains("Do not add it to policy"))
        // The chain is verified by the command, not merely minted by the
        // writer: an intact journal says so, against the stored head ref.
        .stdout(predicate::str::contains(
            "1 record(s) verified, anchored to the stored head",
        ));
}

/// The whole point of the chain, proved through the real command: a record
/// deleted from the journal is reported, and the surviving evidence is marked
/// untrustworthy rather than presented as a clean repair summary.
#[test]
fn ocap_denials_reports_a_journal_with_a_record_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("denials.jsonl");
    for name in ["a.rs", "b.rs", "c.rs"] {
        newt_core::denial_journal::append_record(&journal, record(&format!("wc -l {name}")))
            .unwrap();
    }

    let body = std::fs::read_to_string(&journal).unwrap();
    let kept: Vec<&str> = body
        .lines()
        .enumerate()
        .filter(|(i, _)| *i != 1)
        .map(|(_, line)| line)
        .collect();
    std::fs::write(&journal, kept.join("\n")).unwrap();

    denials(&journal)
        .assert()
        .success()
        .stdout(predicate::str::contains("CHAIN BROKEN"))
        .stdout(predicate::str::contains("record 1: broken link"))
        .stdout(predicate::str::contains("NOT trustworthy evidence"));
}

/// The migration is announced where an operator hunting for missing evidence
/// looks: the command that reads the journal.
#[test]
fn ocap_denials_points_at_the_rotated_pre_chain_journal() {
    let dir = tempfile::tempdir().unwrap();
    let journal = dir.path().join("denials.jsonl");
    std::fs::write(
        &journal,
        format!(
            "{}\n",
            serde_json::to_string(&record("wc -l old.rs")).unwrap()
        ),
    )
    .unwrap();

    newt_core::denial_journal::append_record(&journal, record("wc -l new.rs")).unwrap();

    denials(&journal)
        .assert()
        .success()
        .stdout(predicate::str::contains("wc -l new.rs"))
        .stdout(predicate::str::contains("denials.pre-chain"))
        .stdout(predicate::str::contains("no integrity guarantee"));
}
