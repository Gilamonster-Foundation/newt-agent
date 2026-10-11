use super::*;
fn claim(ledger: &VerificationLedger) -> String {
    ledger.annotate_push_claim("Pushed: origin/topic is live.".into())
}
/// #2769: missing and failed pushes cannot support the closing summary.
#[test]
fn push_2769_missing_failed_and_wrong_branch_warn() {
    let mut ledger = VerificationLedger::default();
    assert!(claim(&ledger).contains("#2769"));
    ledger.observe_plain_push(
        "run_command",
        &serde_json::json!({"command":"git push origin topic"}),
        Some(ExecOutcome::Failed),
        true,
    );
    assert!(claim(&ledger).contains("#2769"));
    ledger.record_push_outcome(&crate::git_staging::Outcome::Pushed {
        oid: "abc".into(),
        owner: "org".into(),
        name: "repo".into(),
        branch: "other".into(),
    });
    assert!(claim(&ledger).contains("#2769"));
}
/// #2769: a successful governed receipt or ambient plain push pays only
/// for its destination branch, with no network lookup.
#[test]
fn push_2769_successful_observation_covers_claim() {
    for governed in [false, true] {
        let mut ledger = VerificationLedger::default();
        if governed {
            ledger.record_push_outcome(&crate::git_staging::Outcome::Pushed {
                oid: "abc".into(),
                owner: "org".into(),
                name: "repo".into(),
                branch: "topic".into(),
            });
        } else {
            ledger.observe_plain_push(
                "run_command",
                &serde_json::json!({"command":"git.exe push -u origin HEAD:refs/heads/topic"}),
                Some(ExecOutcome::Passed),
                true,
            );
        }
        if governed {
            assert_eq!(
                ledger.annotate_push_claim("Pushed topic".into()),
                "Pushed topic"
            );
            assert!(
                claim(&ledger).contains("#2769"),
                "a governed destination is not an origin alias"
            );
        } else {
            assert_eq!(claim(&ledger), "Pushed: origin/topic is live.");
        }
    }
}
/// #2769: dry runs, masked failures, non-bypass shell results and prose
/// mentioning push cannot manufacture a successful publication fact.
#[test]
fn push_2769_non_evidence_and_nonclaims() {
    for command in [
        "git push --dry-run origin topic",
        "git push origin topic | cat",
        "git push origin topic || true",
        "git push origin topic; echo ok",
        "echo git push origin topic",
        "git push origin :topic",
    ] {
        let mut ledger = VerificationLedger::default();
        ledger.observe_plain_push(
            "run_command",
            &serde_json::json!({"command":command}),
            Some(ExecOutcome::Passed),
            true,
        );
        assert!(claim(&ledger).contains("#2769"), "{command}");
    }
    let ledger = VerificationLedger::default();
    for text in [
        "I have not pushed topic.",
        "I will push topic.",
        "> Pushed topic.",
        "```\nPushed topic.\n```",
    ] {
        assert_eq!(ledger.annotate_push_claim(text.into()), text);
    }
}

/// #2769: all providers and cap exits use the existing finalizer, and a later
/// observation replaces an earlier warning rather than treating it as proof.
#[test]
fn push_2769_finalizer_and_updated_facts() {
    let root = tempfile::tempdir().unwrap();
    let mut ledger = VerificationLedger::default();
    let claims = crate::agentic::claim_check::TurnClaims::capture(
        root.path().to_str().unwrap(),
        &crate::Scope::All,
        None,
    );
    let finalize = |text: String, ledger: &VerificationLedger| {
        crate::agentic::finalize_final_text(
            text,
            root.path().to_str().unwrap(),
            &crate::Scope::All,
            &crate::agentic::capability_check::Evidence::default(),
            None,
            &claims,
            ledger,
        )
    };
    let model = "Pushed: origin/topic is live.";
    let text = finalize(model.to_owned(), &ledger);
    assert!(text.contains("#2769"), "{text}");
    // Stage 2 marks the unsupported claim inline, preserving the original for
    // the operator but excluding host presentation from assistant history.
    assert!(text.contains(&format!("[unverified by newt: {model} —")));
    assert!(crate::agentic::model_reply_for_history(&text).is_empty());
    assert!(crate::agentic::model_reply_for_history(&finalize(text.clone(), &ledger)).is_empty());
    ledger.record_publication_outcome(&crate::git_staging::Outcome::Pushed {
        oid: "abc".into(),
        owner: "org".into(),
        name: "repo".into(),
        branch: "topic".into(),
    });
    let updated = finalize(model.replace("origin/topic", "org/repo/topic"), &ledger);
    assert!(!updated.contains("#2769"), "{updated}");
    assert!(
        updated.contains("[unverified by newt:"),
        "the scoped report still has no receipt: {updated}"
    );
    assert!(crate::agentic::model_reply_for_history(&updated).is_empty());
}

/// #2769: the actual tool-result funnel, not an ok-looking tool string, owns
/// ambient evidence; a confined shell pass cannot stand in for a receipt.
#[tokio::test]
async fn push_2769_actual_observation_funnel() {
    use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
    let _lock = env_lock().await;
    for bypass in ["0", "1"] {
        let _env = EnvVar::set("NEWT_DISABLE_OCAP", bypass);
        for outcome in [
            ExecOutcome::Passed,
            ExecOutcome::Failed,
            ExecOutcome::Denied,
            ExecOutcome::TimedOut,
        ] {
            let mut ledger = VerificationLedger::default();
            ledger
                .observe_routed(
                    "run_command",
                    &serde_json::json!({"command":"git push origin topic"}),
                    true,
                    Some(outcome),
                    ".",
                    None,
                )
                .await;
            assert_eq!(
                claim(&ledger).contains("#2769"),
                !(bypass == "1" && outcome == ExecOutcome::Passed)
            );
        }
    }
}

/// #2769: branch specificity, live-on-origin phrasing, and generic claims
/// remain distinct; default pushes cannot certify an unknown named branch.
#[test]
fn push_2769_claim_spellings_and_default_refspecs() {
    let mut ledger = VerificationLedger::default();
    for text in [
        "topic is live on origin",
        "branch topic pushed",
        "Pushed topic",
        "The branch is pushed",
    ] {
        assert!(
            ledger.annotate_push_claim(text.into()).contains("#2769"),
            "{text}"
        );
    }
    ledger.record_publication_outcome(&crate::git_staging::Outcome::DryRunChecked);
    assert!(claim(&ledger).contains("#2769"));
    ledger.observe_plain_push(
        "run_command",
        &serde_json::json!({"command":"git push"}),
        Some(ExecOutcome::Passed),
        true,
    );
    assert_eq!(
        ledger.annotate_push_claim("The branch is pushed".into()),
        "The branch is pushed"
    );
    assert!(claim(&ledger).contains("#2769"));
}

/// #2769: a leading cwd change preserves the final push status; unrelated
/// prefixes, pipelines and conditional fallbacks cannot claim that status.
#[test]
fn push_2769_leading_cd_preserves_evidence() {
    for command in [
        "cd ../task && git push origin topic",
        r#"cd /d "C:\task tree" && git.exe push origin HEAD:refs/heads/topic"#,
    ] {
        let mut ledger = VerificationLedger::default();
        ledger.observe_plain_push(
            "run_command",
            &serde_json::json!({"command":command}),
            Some(ExecOutcome::Passed),
            true,
        );
        assert_eq!(
            ledger.annotate_push_claim("Pushed topic".into()),
            "Pushed topic"
        );
        assert!(!claim(&ledger).contains("#2769"), "{command}");
    }
    for command in [
        "false || git push origin topic",
        "echo ok && git push origin topic",
        "cd $(echo task) && git push origin topic",
    ] {
        let mut ledger = VerificationLedger::default();
        ledger.observe_plain_push(
            "run_command",
            &serde_json::json!({"command":command}),
            Some(ExecOutcome::Passed),
            true,
        );
        assert!(claim(&ledger).contains("#2769"), "{command}");
    }
}

/// #2769: origin cannot borrow a backup push. Round 3 deliberately allows
/// the same remote+branch observation from any cwd in the turn.
#[test]
fn push_2769_round2_destination_binding() {
    for (args, warns) in [
        (serde_json::json!({"command":"git push backup topic"}), true),
        (
            serde_json::json!({"command":"git push origin topic", "cwd":"other-repository"}),
            false,
        ),
    ] {
        let mut ledger = VerificationLedger::default();
        ledger.observe_plain_push("run_command", &args, Some(ExecOutcome::Passed), true);
        assert_eq!(claim(&ledger).contains("#2769"), warns, "{args}");
    }
}

/// #2769 review: incomplete intentions are not completed publication claims;
/// an unrelated negative clause must not hide the actual completed claim.
#[test]
fn push_2769_round2_incomplete_and_unrelated_negation() {
    let ledger = VerificationLedger::default();
    for text in [
        "Branch topic is yet to be pushed",
        "Ready to be pushed",
        "Branch topic is ready to be pushed",
        "No branch has been pushed",
        "Branch topic would be pushed",
    ] {
        assert_eq!(ledger.annotate_push_claim(text.into()), text);
    }
    for text in [
        "Not committed; pushed origin/topic.",
        "Pushed origin/topic, but not opened a PR.",
    ] {
        assert!(
            ledger.annotate_push_claim(text.into()).contains("#2769"),
            "{text}"
        );
    }
}

/// #2769 round 2: preserve known aliases and governed owner/repo destinations;
/// remote-qualified claims match the same destination anywhere in the turn.
#[test]
fn push_2769_round2_matching_destinations() {
    let mut ledger = VerificationLedger::default();
    ledger.observe_plain_push(
        "run_command",
        &serde_json::json!({"command":"git push backup topic", "cwd":"task"}),
        Some(ExecOutcome::Passed),
        true,
    );
    for (text, warns) in [
        ("Pushed backup/topic", false),
        ("Pushed origin/topic", true),
        ("Pushed topic", false),
    ] {
        assert_eq!(
            ledger.annotate_push_claim(text.into()).contains("#2769"),
            warns,
            "{text}"
        );
    }
    ledger.record_push_outcome(&crate::git_staging::Outcome::Pushed {
        oid: "abc".into(),
        owner: "owner".into(),
        name: "repo".into(),
        branch: "topic".into(),
    });
    assert_eq!(
        ledger.annotate_push_claim("Pushed owner/repo/topic".into()),
        "Pushed owner/repo/topic"
    );
    assert!(ledger
        .annotate_push_claim("Pushed other/repo/topic".into())
        .contains("#2769"));
    assert!(!claim_check::claims_committed("Ready to be committed"));
    assert!(claim_check::claims_committed("Committed, but not pushed"));
}

/// #2769 round 3: only the object attached to the completed push is a claim;
/// a negated or incidental slash-bearing token is not another pushed branch.
#[test]
fn push_2769_round3_mixed_objects() {
    let text = "Pushed origin/topic, not origin/other.";
    let mut ledger = VerificationLedger::default();
    let missing = ledger.annotate_push_claim(text.into());
    let warning = missing.strip_prefix(text).unwrap();
    assert!(warning.contains("origin/topic"), "{missing}");
    assert!(!warning.contains("origin/other"), "{missing}");
    ledger.observe_plain_push(
        "run_command",
        &serde_json::json!({"command":"git push origin topic"}),
        Some(ExecOutcome::Passed),
        true,
    );
    assert_eq!(ledger.annotate_push_claim(text.into()), text);
    let incidental = "Pushed origin/topic, see docs/guide.md.";
    assert_eq!(ledger.annotate_push_claim(incidental.into()), incidental);
}

/// #2769 round 3: ground the symlink/parent counterexample in a real filesystem.
/// The conductor's turn-wide remote+branch rule deliberately needs no cwd identity.
#[cfg(unix)]
#[tokio::test]
async fn push_2769_round3_symlink_parent_observation() {
    use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
    let _lock = env_lock().await;
    let _bypass = EnvVar::set("NEWT_DISABLE_OCAP", "1");
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    let other = temp.path().join("other");
    std::fs::create_dir(&main).unwrap();
    std::fs::create_dir_all(other.join("sub")).unwrap();
    std::os::unix::fs::symlink(other.join("sub"), main.join("link")).unwrap();
    assert_eq!(
        main.join("link/..").canonicalize().unwrap(),
        other.canonicalize().unwrap()
    );
    let mut ledger = VerificationLedger::default();
    ledger
        .observe_routed(
            "run_command",
            &serde_json::json!({"command":"git push origin topic", "cwd":"link/.."}),
            true,
            Some(ExecOutcome::Passed),
            main.to_str().unwrap(),
            None,
        )
        .await;
    let text = "Pushed origin/topic";
    assert_eq!(ledger.annotate_push_claim(text.into()), text);
}
