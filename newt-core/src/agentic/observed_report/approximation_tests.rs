use super::*;

/// Approximate file sizes must not hide contradictions in a retained report.
#[test]
fn report_approximation_corrects_large_errors_and_keeps_close_estimates() {
    let dir = fixture();
    let file = dir.path().join("src/mod.rs");
    std::fs::write(&file, "x\n".repeat(11726)).unwrap();
    let before = files::snapshot(dir.path(), &Scope::All).unwrap();
    std::fs::write(&file, "x\n".repeat(11564)).unwrap();
    let after = files::snapshot(dir.path(), &Scope::All).unwrap();
    for (text, corrected) in [
        ("src/mod.rs (~4400 lines)", true),
        ("src/mod.rs (~3,800 lines)", true),
        ("src/mod.rs (~11,700 lines)", false),
        ("src/mod.rs is about 4400 lines", true),
        ("src/mod.rs is approximately 11,700 lines", false),
        ("src/mod.rs is roughly 3800 lines", true),
        ("src/mod.rs is ≈11700 lines", false),
        ("original src/mod.rs (~4400 lines)", true),
    ] {
        let review = review::review(
            text,
            review::Facts {
                before: Some(&before),
                after: Some(&after),
                checks: &[],
                publications: &[],
                root: dir.path(),
            },
        );
        let rendered = review.rendered();
        if corrected {
            assert!(
                rendered.contains("[corrected by newt:"),
                "{text}: {rendered}"
            );
            assert!(
                rendered.contains(if text.starts_with("original") {
                    "11726"
                } else {
                    "11564"
                }),
                "{rendered}"
            );
            assert!(!rendered.contains("4400") && !rendered.contains("3,800"));
            assert_eq!(review.restore(&rendered).as_deref(), Some(text));
        } else {
            assert_eq!(rendered, text);
        }
    }
}

/// Inclusive ten-percent bounds use observed counts; missing evidence abstains.
#[test]
fn report_approximation_boundaries_and_missing_observation() {
    let dir = fixture();
    std::fs::write(dir.path().join("src/mod.rs"), "x\n".repeat(100)).unwrap();
    let snapshot = files::snapshot(dir.path(), &Scope::All).unwrap();
    for (n, corrected) in [
        (89, true),
        (90, false),
        (100, false),
        (110, false),
        (111, true),
    ] {
        let text = format!("src/mod.rs is ~{n} lines");
        let rendered = review::review(
            &text,
            review::Facts {
                before: None,
                after: Some(&snapshot),
                checks: &[],
                publications: &[],
                root: dir.path(),
            },
        )
        .rendered();
        assert_eq!(
            rendered.contains("[corrected by newt:"),
            corrected,
            "{rendered}"
        );
        assert!(!rendered.contains("unverified"), "{rendered}");
    }
    for text in ["unknown.rs (~4400 lines)", "mod.rs (~4400 lines)"] {
        let rendered = review::review(
            text,
            review::Facts {
                before: None,
                after: None,
                checks: &[],
                publications: &[],
                root: dir.path(),
            },
        )
        .rendered();
        assert!(rendered.contains("[unverified by newt:"), "{rendered}");
    }
}

/// Explicit revision scope cannot borrow a matching count from the other revision.
#[test]
fn report_approximation_keeps_revision_scope_and_path_words() {
    let dir = fixture();
    let file = dir.path().join("src/original.rs");
    std::fs::write(&file, "x\n".repeat(1000)).unwrap();
    let before = files::snapshot(dir.path(), &Scope::All).unwrap();
    std::fs::write(&file, "x\n".repeat(100)).unwrap();
    let after = files::snapshot(dir.path(), &Scope::All).unwrap();
    for (text, corrected) in [
        ("src/original.rs is ~100 lines", false),
        ("src/original.rs before: ~100 lines", true),
        ("src/original.rs after: ~1000 lines", true),
        ("original src/original.rs (~1000 lines)", false),
        ("original src/original.rs is ~1000 lines", false),
        ("original src/original.rs is ~100 lines", true),
    ] {
        let result = review::review(
            text,
            review::Facts {
                before: Some(&before),
                after: Some(&after),
                checks: &[],
                publications: &[],
                root: dir.path(),
            },
        )
        .rendered();
        assert_eq!(
            result.contains("[corrected by newt:"),
            corrected,
            "{result}"
        );
        assert!(!result.contains("unverified"), "{result}");
    }
}

/// A final report with retained evidence corrects the count in host voice and
/// excludes that correction from assistant replay, preserving stage-2 auditing.
#[test]
fn report_approximation_final_composition_preserves_voice_and_evidence() {
    let dir = fixture();
    let file = dir.path().join("src/mod.rs");
    std::fs::write(&file, "x\n".repeat(11726)).unwrap();
    let workspace = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([7; 16]);
    let claims =
        TurnClaims::capture(workspace, &Scope::All, None).with_evidence(review::Evidence {
            spill: Some(&spill),
            ..Default::default()
        });
    std::fs::write(&file, "x\n".repeat(11564)).unwrap();
    let report = crate::agentic::finalize_final_text(
        "src/mod.rs (~4400 lines)".to_string(),
        workspace,
        &Scope::All,
        &crate::agentic::capability_check::Evidence::default(),
        None,
        &claims,
        &crate::agentic::self_verify::VerificationLedger::for_turn("refactor", false),
    );
    assert!(
        report.contains("[corrected by newt: src/mod.rs: 11726 → 11564 lines]"),
        "{report}"
    );
    assert!(report.contains("newt review:"), "{report}");
    assert!(!report.contains("4400"), "{report}");
    assert!(!assistant_prose(&report).contains("corrected by newt"));
}
