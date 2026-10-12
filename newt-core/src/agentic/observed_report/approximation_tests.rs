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

fn review_unique_size(text: &str, observed: usize) -> String {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/mod.rs"), "x\n".repeat(observed)).unwrap();
    git(dir.path(), &["add", "."]);
    let snapshot = files::snapshot(dir.path(), &Scope::All).unwrap();
    review::review(
        text,
        review::Facts {
            before: None,
            after: Some(&snapshot),
            checks: &[],
            publications: &[],
            root: dir.path(),
        },
    )
    .rendered()
}

/// PR #2862: an approximate helper count cannot relax an exact line count.
#[test]
fn report_approximation_does_not_borrow_trailing_qualifier() {
    let text = "src/mod.rs is 110 lines with about 5 helpers";
    let rendered = review_unique_size(text, 100);
    assert!(
        rendered.contains("[corrected by newt: src/mod.rs: 100 lines]"),
        "{rendered}"
    );
}

/// PR #2862: an approximate file inventory cannot relax a later exact size.
#[test]
fn report_approximation_does_not_borrow_leading_qualifier() {
    let text = "about 3 files, mod.rs is 11,000 lines";
    let rendered = review_unique_size(text, 11726);
    assert!(
        rendered.contains("[corrected by newt: src/mod.rs: 11726 lines]"),
        "{rendered}"
    );
}

/// Binding uses positions, not equal numeral strings or clause-wide qualifiers.
#[test]
fn report_approximation_qualifier_binding_controls() {
    for qualifier in ["~", "≈", "about ", "approximately ", "roughly "] {
        for count in ["90", "110"] {
            let text = format!("src/mod.rs is {qualifier}{count} lines with about 5 helpers");
            assert_eq!(review_unique_size(&text, 100), text);
        }
        let outside = format!("src/mod.rs is {qualifier}111 lines");
        assert!(review_unique_size(&outside, 100).contains("corrected by newt"));
        let separate = format!("{qualifier}110 helpers, src/mod.rs is 110 lines");
        assert!(review_unique_size(&separate, 100).contains("corrected by newt"));
    }
    let exact = "about 5 helpers, src/mod.rs is 100 lines";
    assert_eq!(review_unique_size(exact, 100), exact);
}
