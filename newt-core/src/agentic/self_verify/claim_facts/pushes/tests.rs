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
        assert_eq!(claim(&ledger), "Pushed: origin/topic is live.");
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
    let text = crate::agentic::finalize_final_text(
        "Pushed: origin/topic is live.".into(),
        root.path().to_str().unwrap(),
        &crate::Scope::All,
        &crate::agentic::capability_check::Evidence::default(),
        None,
        None,
        &ledger,
    );
    assert!(text.contains("#2769"), "{text}");
    assert_eq!(ledger.annotate_push_claim(text.clone()), text);
    ledger.record_publication_outcome(&crate::git_staging::Outcome::Pushed {
        oid: "abc".into(),
        owner: "org".into(),
        name: "repo".into(),
        branch: "topic".into(),
    });
    assert!(!ledger.annotate_push_claim(text).contains("#2769"));
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
