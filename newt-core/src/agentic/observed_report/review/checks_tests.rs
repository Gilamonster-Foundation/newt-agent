use super::*;

fn observation(command: &str, lines: &[&str]) -> super::super::super::Check {
    super::super::super::Check {
        command: command.into(),
        cwd: "project".into(),
        outcome: "Passed".into(),
        exit: Some(0),
        lines: lines.iter().map(|s| (*s).into()).collect(),
        truncated: false,
    }
}

#[test]
fn report_stage2_aggregates_one_complete_named_invocation_not_last_doctest() {
    let check = observation("cargo test -p core", &[
        "Finished `test` profile [unoptimized] target(s) in 1.0s",
        "test result: ok. 1847 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.0s",
        "test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 1.0s",
    ]);
    assert_eq!(totals(&check), Some([1850, 0, 1, 1850]));
    let mut partial = check.clone();
    partial.truncated = true;
    assert_eq!(totals(&partial), None);
    let mut rerun = check.clone();
    rerun.lines.extend(check.lines.clone());
    assert_eq!(totals(&rerun), None);
    let mut pipeline = check.clone();
    pipeline.command = "cargo test -p core | tail -5".into();
    assert_eq!(totals(&pipeline), None);
}

#[test]
fn report_stage2_nextest_and_failed_target_counts_keep_their_meaning() {
    let check = observation(
        "cargo nextest run -p core",
        &["Summary [ 1.0s] 1850 tests run: 1850 passed (2 slow), 7 skipped"],
    );
    assert_eq!(totals(&check), Some([1850, 0, -1, 1850]));
    let mut failed = observation("cargo test -p core", &[
        "test result: FAILED. 1846 passed; 1 failed; 6 ignored; 0 measured; 0 filtered out; finished in 1.0s",
        "test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 1.0s",
    ]);
    failed.exit = Some(101);
    failed.outcome = "Failed".into();
    assert_eq!(totals(&failed), Some([1849, 1, 7, 1850]));
    failed.exit = Some(0);
    assert_eq!(
        totals(&failed),
        None,
        "a passing tail cannot hide the failure"
    );
}

#[test]
fn report_stage2_named_target_command_is_not_a_future_target() {
    let check = observation("cargo test -p core --target test-platform", &[
        "test result: ok. 1850 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.0s",
    ]);
    let facts = Facts {
        before: None,
        after: None,
        checks: &[check],
        publications: &[],
        root: Path::new("project"),
    };
    assert!(matches!(
        classify(
            "`cargo test -p core --target test-platform` — 1617 tests passed.",
            &facts
        ),
        Some(ClaimAssessment::Corrected(_))
    ));
}

/// Stage 2 must not certify later objects from the first matching receipt, nor
/// confuse a URL with another URL whose PR number merely shares its prefix.
#[test]
fn report_stage2_publication_objects_cannot_borrow_a_receipt() {
    let facts = Facts {
        before: None,
        after: None,
        checks: &[],
        publications: &["PR creation observed: https://github.com/o/r/pull/70".into()],
        root: Path::new("project"),
    };
    for text in [
        "Created PR https://github.com/o/r/pull/7",
        "Created PR #70 and PR #99.",
        "Created PR #70 and #99.",
    ] {
        assert!(
            matches!(classify(text, &facts), Some(ClaimAssessment::Unverified(_))),
            "{text}"
        );
    }
}

/// Approximate totals and multiple named invocations are never exact proof.
#[test]
fn report_stage2_test_quantity_boundaries_are_unverified() {
    let check = observation("cargo test -p core", &[
        "test result: ok. 1850 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.0s",
    ]);
    let facts = Facts {
        before: None,
        after: None,
        checks: &[check],
        publications: &[],
        root: Path::new("project"),
    };
    for text in [
        "`cargo test -p core` — roughly 1850 tests passed.",
        "`cargo test -p core` — ≈1850 tests passed.",
        "`cargo test -p core` and `cargo test -p other` — 1850 tests passed.",
    ] {
        assert!(
            matches!(classify(text, &facts), Some(ClaimAssessment::Unverified(_))),
            "{text}"
        );
    }
}
