use super::*;
use crate::git_staging::Outcome;

const URL: &str = "https://github.com/o/r/pull/7";

fn created() -> VerificationLedger {
    let mut ledger = VerificationLedger::for_turn("refactor", false);
    ledger.record_publication_outcome(&Outcome::PrCreated { url: URL.into() });
    ledger
}

/// #2741: a confident closing PR claim requires this turn's governed receipt.
#[test]
fn issue_2741_missing_create_is_unverified() {
    for text in [
        "PR #1 is open: extract the loop",
        "Opened a pull request.",
        "I created PR #7.",
        "Pull request has been opened.",
        "PR opened: https://github.com/o/r/pull/7",
    ] {
        let got = VerificationLedger::default().annotate_pr_claim(text.into());
        assert!(got.starts_with(text));
        assert!(got.contains("Unverified"), "{got}");
    }
}

/// #2741: neither a different number nor a same-number foreign repo matches.
#[test]
fn issue_2741_mismatched_reference_is_refuted() {
    for text in [
        "PR #8 is open.",
        "PR 8 is open.",
        "Opened PR http://github.com/o/r/pull/7",
        "Created PR https://github.com/other/repo/pull/7",
        "Opened PR #8: https://github.com/o/r/pull/7",
        "PR #7 is open. Opened PR #9.",
    ] {
        let got = created().annotate_pr_claim(text.into());
        assert!(got.contains("Refuted"), "{got}");
        assert!(got.contains(URL), "{got}");
    }
}

/// #2741: matching claims and non-claims remain quiet and byte-identical.
#[test]
fn issue_2741_observed_create_and_non_claims_pass() {
    for text in [
        "PR #7 is open.",
        "Opened a pull request.",
        "Created PR #7 for issue #2741.",
        "Created PR https://github.com/o/r/pull/7",
        "Created pull request [#7](https://github.com/o/r/pull/7).",
    ] {
        assert_eq!(created().annotate_pr_claim(text.into()), text);
    }
    for text in [
        "I will open PR #7.",
        "PR #7 is not open.",
        "I did not create a PR.",
        "I have never opened a pull request.",
        "PR creation failed.",
        "> PR #7 is open.",
        "Previously opened PR #7.",
        "Remaining work:\nOpen PR #7",
        "No PR was created.",
        "PR #7 should be opened.",
        "If the PR is open, review it.",
        "PR #7 is ready to open.",
        "PR #7 needs to be created.",
    ] {
        assert_eq!(
            VerificationLedger::default().annotate_pr_claim(text.into()),
            text
        );
    }
}

/// #2741: failed or malformed governed outcomes cannot certify creation.
#[test]
fn issue_2741_failed_creation_and_turn_isolation() {
    let mut ledger = VerificationLedger::default();
    ledger.record_publication_outcome(&Outcome::Failed {
        category: crate::git_staging::FailureCategory::Parse,
    });
    ledger.record_publication_outcome(&Outcome::PrCreated {
        url: "not a URL".into(),
    });
    let text = "Opened PR #7.";
    assert!(ledger.annotate_pr_claim(text.into()).contains("Unverified"));
    assert_eq!(created().annotate_pr_claim(text.into()), text);
    assert!(VerificationLedger::for_turn("next turn", false)
        .annotate_pr_claim(text.into())
        .contains("Unverified"));
}

/// #2741: the actual command worktree, not only Cargo cwd, resolves file claims.
/// Real filesystem grounds the existing injected path-resolver tests.
#[test]
fn issue_2741_observed_command_worktree_resolves_path() {
    let root = tempfile::tempdir().unwrap();
    let launch = root.path().join("launch");
    let tree = root.path().join("task");
    std::fs::create_dir_all(&launch).unwrap();
    std::fs::create_dir_all(tree.join("newt-core/src")).unwrap();
    std::fs::write(tree.join("newt-core/src/loop.rs"), "// extracted").unwrap();
    let mut ledger = VerificationLedger::default();
    ledger.record_command_directory(&tree);
    let text = "Updated newt-core/src/loop.rs";
    let got = crate::agentic::claim_check::annotate_in_context(
        text.into(),
        launch.to_str().unwrap(),
        ledger.claim_directories(),
        &crate::Scope::All,
    );
    assert_eq!(got, text);
    let denied = crate::agentic::claim_check::annotate_in_context(
        text.into(),
        launch.to_str().unwrap(),
        ledger.claim_directories(),
        &crate::Scope::only([launch.to_string_lossy().into_owned()]),
    );
    assert!(denied.contains("unverified"), "{denied}");
    assert!(!denied.contains("not found"), "{denied}");
}

/// #2741: all provider closing paths share this finalizer, including cap exits.
#[test]
fn issue_2741_finalizer_checks_pr_claims() {
    let root = tempfile::tempdir().unwrap();
    let text = "PR #1 is open: extracted loop";
    let got = crate::agentic::finalize_final_text(
        text.into(),
        root.path().to_str().unwrap(),
        &crate::Scope::All,
        &crate::agentic::capability_check::Evidence::default(),
        None,
        &crate::agentic::claim_check::TurnClaims::capture(
            root.path().to_str().unwrap(),
            &crate::Scope::All,
            None,
        ),
        &VerificationLedger::default(),
    );
    assert!(got.contains("Unverified"), "{got}");
}

/// #2741: successful shell requests or echoed receipts are not governed outcomes.
#[tokio::test]
async fn issue_2741_plain_shell_success_cannot_certify_a_pr() {
    let mut ledger = VerificationLedger::for_turn("refactor", false);
    for command in [
        "gh pr create --title t --body b",
        "echo pr_created https://github.com/o/r/pull/7",
    ] {
        ledger
            .observe(
                "run_command",
                &serde_json::json!({"command":command}),
                true,
                Some(ExecOutcome::Passed),
                "/launch",
            )
            .await;
    }
    let got = ledger.annotate_pr_claim("Opened PR #7.".into());
    assert!(got.contains("Unverified"), "{got}");
}

fn final_claim(ledger: &VerificationLedger, text: &str) -> String {
    let root = tempfile::tempdir().unwrap();
    crate::agentic::finalize_final_text(
        text.into(),
        root.path().to_str().unwrap(),
        &crate::Scope::All,
        &crate::agentic::capability_check::Evidence::default(),
        None,
        &crate::agentic::claim_check::TurnClaims::capture(
            root.path().to_str().unwrap(),
            &crate::Scope::All,
            None,
        ),
        ledger,
    )
}

/// #2741 round 2: each coordinated creation has its own governed receipt.
#[test]
fn issue_2741_round2_multiple_created_prs_pass_finalizer() {
    let mut ledger = created();
    ledger.record_publication_outcome(&Outcome::PrCreated {
        url: "https://github.com/o/r/pull/8".into(),
    });
    for text in [
        "Opened PR #7 and PR #8.",
        "Created PR #7 to replace PR #6 and opened PR #8.",
        "Created pull request #7 and pull request #8.",
        "Opened PR #7: https://github.com/o/r/pull/7 and PR #8: https://github.com/o/r/pull/8",
    ] {
        assert_eq!(final_claim(&ledger, text), text);
    }
}

/// #2741 round 2: a replaced or compared PR is not claimed newly created.
#[test]
fn issue_2741_round2_comparison_reference_is_not_a_creation() {
    for text in [
        "Created PR #7 to replace PR #6.",
        "Created PR #7 to replace https://github.com/o/r/pull/6",
        "Opened PR #7 and closed PR #6.",
        "Created PR #7 unlike PR #6.",
    ] {
        assert_eq!(final_claim(&created(), text), text);
    }
}

/// #2741 round 2: independent receipts must not hide a false creation or
/// lend different receipts to one PR's inconsistent number/URL pair.
#[test]
fn issue_2741_round2_false_creation_among_true_ones_is_refuted() {
    let mut ledger = created();
    ledger.record_publication_outcome(&Outcome::PrCreated {
        url: "https://github.com/o/r/pull/8".into(),
    });
    for text in [
        "Opened PR #7 and PR #9.",
        "Opened PR #7 and PR #8 and PR #9.",
        "Created PR #7 to replace PR #6 and opened PR #9.",
        "Opened PR #7: https://github.com/o/r/pull/8",
    ] {
        let got = final_claim(&ledger, text);
        assert!(got.starts_with(text));
        assert!(got.contains("Refuted"), "{got}");
    }
}

/// #2741 round 2: an object list need not repeat the PR noun; retain its
/// separators so each creation is checked, including a false later member.
#[test]
fn issue_2741_round2_coordinated_bare_references() {
    let mut ledger = created();
    ledger.record_publication_outcome(&Outcome::PrCreated {
        url: "https://github.com/o/r/pull/8".into(),
    });
    for text in [
        "Opened PR #7, #8.",
        "Opened PR #7 and #8.",
        "Created PR #7,#8.",
        "Created PR https://github.com/o/r/pull/7 and https://github.com/o/r/pull/8",
    ] {
        assert_eq!(final_claim(&ledger, text), text);
    }
    for text in ["Opened PR #7 and #9.", "Opened PR #7, #8, #9."] {
        let got = final_claim(&ledger, text);
        assert!(got.contains("Refuted"), "{got}");
    }
}
