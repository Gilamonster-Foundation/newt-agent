use super::*;

fn finish(text: &str, root: &str, claims: &TurnClaims<'_>) -> String {
    crate::agentic::finalize_final_text(
        text.to_owned(),
        root,
        &Scope::All,
        &crate::agentic::capability_check::Evidence::default(),
        None,
        claims,
        &crate::agentic::self_verify::VerificationLedger::default(),
    )
}
fn observe(claims: &TurnClaims<'_>, root: &str) {
    claims.observe_report(root, &Scope::All, &Capture {
        command: Some(("cargo test -p newt-core".into(), root.into())), exit: Some(0),
        lines: vec!["test result: ok. 1850 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.0s".into()],
        truncated: false,
    }, Some(ExecOutcome::Passed), None);
}

/// PR #2855 R2: a correct file fact cannot certify or hide other claim families.
#[test]
fn report_stage2_round2_mixed_claims_are_independent() {
    let dir = fixture();
    let root = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([19; 16]);
    let claims = TurnClaims::capture(root, &Scope::All, None).with_evidence(review::Evidence {
        spill: Some(&spill),
        ..Default::default()
    });
    let draft = "src/mod.rs is 2 lines; 1617 tests passed; PR #999 created.";
    let report = finish(draft, root, &claims);
    assert!(
        report.contains("src/mod.rs is 2 lines; [unverified by newt: 1617"),
        "{report}"
    );
    assert!(report.contains("[unverified by newt: PR #999"), "{report}");
    let replay = assistant_prose(&report);
    assert!(replay.contains("src/mod.rs is 2 lines"), "{replay}");
    assert!(
        !replay.contains("1617")
            && !replay.contains("999")
            && !replay.contains("unverified by newt"),
        "{replay}"
    );
}

/// PR #2855 R2: same-line checks are corrected independently, not swallowed.
#[test]
fn report_stage2_round2_mixed_corrected_check_preserves_file_fact() {
    let dir = fixture();
    let root = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([20; 16]);
    let claims = TurnClaims::capture(root, &Scope::All, None).with_evidence(review::Evidence {
        spill: Some(&spill),
        ..Default::default()
    });
    observe(&claims, root);
    let report = finish(
        "src/mod.rs is 2 lines; `cargo test -p newt-core` — 1617 tests passed.",
        root,
        &claims,
    );
    assert!(
        report.contains("src/mod.rs is 2 lines; [corrected by newt:"),
        "{report}"
    );
    assert!(
        report.contains("1850 passed") && !report.contains("1617"),
        "{report}"
    );
}

/// PR #2855 R2: a byte edit with unchanged LF count invalidates current claims.
#[test]
fn report_stage2_round2_check_is_bound_to_observed_content() {
    let dir = fixture();
    let root = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([21; 16]);
    let claims = TurnClaims::capture(root, &Scope::All, None).with_evidence(review::Evidence {
        spill: Some(&spill),
        ..Default::default()
    });
    observe(&claims, root);
    let current = "`cargo test -p newt-core` — 1850 tests passed on the current tree.";
    assert_eq!(model_explanation(&finish(current, root, &claims)), current);
    std::fs::write(dir.path().join("src/mod.rs"), "changed\nbytes\n").unwrap();
    let report = finish(current, root, &claims);
    assert!(report.contains("[unverified by newt:"), "{report}");
    assert!(report.contains("stale"), "{report}");
    assert!(assistant_prose(&report).is_empty());
    let historical = "Previously, `cargo test -p newt-core` — 1850 tests passed.";
    assert_eq!(
        model_explanation(&finish(historical, root, &claims)),
        historical
    );
    observe(&claims, root);
    assert_eq!(model_explanation(&finish(current, root, &claims)), current);
}

/// PR #2855 R2: unsafe mixed clauses abstain; sentence boundaries preserve
/// unrelated numbers and commands, and inline markers stay out of replay.
#[test]
fn report_stage2_round2_residue_and_inline_replay_boundaries() {
    let dir = fixture();
    let root = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([22; 16]);
    let claims = TurnClaims::capture(root, &Scope::All, None).with_evidence(review::Evidence {
        spill: Some(&spill),
        ..Default::default()
    });
    observe(&claims, root);
    for draft in [
        "src/mod.rs is 2 lines and 1617 tests passed.",
        "`cargo test -p newt-core` — 1850 tests passed and PR #999 created.",
        "src/mod.rs is 2 lines (PR #999 created).",
    ] {
        let report = finish(draft, root, &claims);
        assert!(report.contains("[unverified by newt:"), "{report}");
        assert!(assistant_prose(&report).is_empty(), "{report}");
    }
    for retain in [false, true] {
        let claims = TurnClaims::capture(root, &Scope::All, None).with_evidence(review::Evidence {
            spill: retain.then_some(&spill),
            ..Default::default()
        });
        for prefix in [
            "Version 1.2.3 for issue #2855.",
            "Use `echo '1617 tests passed; PR #999 created.'`.",
            "Use ``echo '1617 tests passed; PR #999 created.'``.",
            "I discuss [corrected by newt: a marker] here.",
            "I discuss \\[unverified by newt: a marker] here.",
        ] {
            let report = finish(
                &format!("{prefix} src/mod.rs is 999 lines; PR #999 created [unclosed."),
                root,
                &claims,
            );
            let replay = assistant_prose(&report);
            assert!(replay.contains(prefix), "{report} => {replay}");
            assert!(!replay.contains("src/mod.rs is 999"), "{replay}");
            assert!(!replay.contains("unclosed"), "{replay}");
        }
    }
}

/// PR #2855 R2: untracked files also invalidate observations; historical wording
/// cannot launder a current-tree qualifier, and unavailable reads fail closed.
#[test]
fn report_stage2_round2_freshness_requires_complete_authorized_content() {
    let dir = fixture();
    let root = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([23; 16]);
    let claims = TurnClaims::capture(root, &Scope::All, None).with_evidence(review::Evidence {
        spill: Some(&spill),
        ..Default::default()
    });
    observe(&claims, root);
    std::fs::write(dir.path().join("new.rs"), "new input\n").unwrap();
    for draft in [
        "`cargo test -p newt-core` — 1850 tests passed.",
        "`cargo test -p newt-core` — 1617 tests passed.",
        "Previously, `cargo test -p newt-core` — 1850 tests passed on the current tree.",
    ] {
        let report = finish(draft, root, &claims);
        assert!(
            model_explanation(&report).starts_with("[unverified by newt:"),
            "{report}"
        );
    }
    observe(&claims, root);
    let report = crate::agentic::finalize_final_text(
        "`cargo test -p newt-core` — 1850 tests passed.".to_owned(),
        root,
        &Scope::none(),
        &crate::agentic::capability_check::Evidence::default(),
        None,
        &claims,
        &crate::agentic::self_verify::VerificationLedger::default(),
    );
    assert!(
        model_explanation(&report).starts_with("[unverified by newt:"),
        "{report}"
    );
}

/// PR #2855 R2: even publication subfamilies cannot share a receipt implicitly.
#[test]
fn report_stage2_round2_pr_receipt_cannot_hide_push_claim() {
    let dir = fixture();
    let root = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([24; 16]);
    let claims = TurnClaims::capture(root, &Scope::All, None).with_evidence(review::Evidence {
        spill: Some(&spill),
        ..Default::default()
    });
    claims.observe_report(
        root,
        &Scope::All,
        &Capture::default(),
        None,
        Some(&crate::git_staging::Outcome::PrCreated {
            url: "https://github.com/o/r/pull/70".into(),
        }),
    );
    let draft = "PR #70 created and pushed branch other at 123abcd.";
    let report = finish(draft, root, &claims);
    assert!(
        model_explanation(&report).starts_with("[unverified by newt:"),
        "{report}"
    );
    assert!(assistant_prose(&report).is_empty());
}
