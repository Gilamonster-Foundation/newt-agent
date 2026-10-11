//! Regressions from the H1 composed report; public fixtures omit local locators.
use super::*;

const EXTRACTION: &str = "**Extraction:** the largest remaining inline block of `newt-core/src/agentic/mod.rs` (11,726 lines → largest in repo) moved into a new `src/agentic/ctx_budget.rs` module — `ChatCtx`, per-round observation types";
const PUBLICATION: &str = "**Committed** `d879ac1` (+594 −549), **pushed**, and **PR opened**: https://github.com/example/project/pull/41";
const OID: &str = "d879ac1cce86c913c88237372ed50baeda1570f7";

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
fn publications(claims: &TurnClaims<'_>, root: &str, push: bool, pr: bool) {
    if push {
        claims.observe_report(
            root,
            &Scope::All,
            &Capture::default(),
            None,
            Some(&crate::git_staging::Outcome::Pushed {
                oid: OID.into(),
                owner: "example".into(),
                name: "project".into(),
                branch: "refactor".into(),
            }),
        );
    }
    if pr {
        claims.observe_report(
            root,
            &Scope::All,
            &Capture::default(),
            None,
            Some(&crate::git_staging::Outcome::PrCreated {
                url: "https://github.com/example/project/pull/41".into(),
            }),
        );
    }
}

/// H1: a path's parenthetical absolute size is independent of a destination path.
#[test]
fn report_h1_absolute_descriptor_binds_its_own_path() {
    let dir = fixture();
    std::fs::create_dir_all(dir.path().join("newt-core/src/agentic")).unwrap();
    std::fs::write(
        dir.path().join("newt-core/src/agentic/mod.rs"),
        "x\n".repeat(11726),
    )
    .unwrap();
    let root = dir.path().to_str().unwrap();
    let claims = TurnClaims::capture(root, &Scope::All, None);
    std::fs::write(
        dir.path().join("newt-core/src/agentic/mod.rs"),
        "x\n".repeat(11600),
    )
    .unwrap();
    let report = finish(EXTRACTION, root, &claims);
    assert_eq!(model_explanation(&report), EXTRACTION, "{report}");
}

/// H1: independent governed push/PR receipts pay for both claims; the unobserved
/// diffstat must be isolated rather than poisoning or silently verifying them.
#[test]
fn report_h1_publication_and_diffstat_are_independent() {
    let dir = fixture();
    let root = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([31; 16]);
    let claims = TurnClaims::capture(root, &Scope::All, None).with_evidence(review::Evidence {
        spill: Some(&spill),
        ..Default::default()
    });
    publications(&claims, root, true, true);
    let text = PUBLICATION.replace(" (+594 −549)", "");
    assert_eq!(model_explanation(&finish(&text, root, &claims)), text);
    let report = finish(PUBLICATION, root, &claims);
    assert!(
        report.contains("**Committed** `d879ac1` ([unverified by newt: +594 −549"),
        "{report}"
    );
    assert!(
        report.contains(
            ", **pushed**, and **PR opened**: https://github.com/example/project/pull/41"
        ),
        "{report}"
    );
    assert!(!assistant_prose(&report).contains("594"));
}

/// H1: tail/grep/echo exit codes are not the inner Cargo status, and retained
/// failure output must never be rendered as Passed even with an outer exit 0.
#[test]
fn report_h1_pipeline_exit_is_not_cargo_success() {
    let dir = fixture();
    let root = dir.path();
    for (command, line) in [
        ("cargo check -p newt-core 2>&1 | tail -60", "error: could not compile `newt-core` (lib) due to 51 previous errors; 3 warnings emitted"),
        ("cargo test -p newt-core --lib 2>&1 | tail -40", "test result: FAILED. 1627 passed; 134 failed; 5 ignored; 0 measured; 0 filtered out; finished in 1.39s"),
        ("cargo check -p newt-core | grep error", ""),
        ("cargo check -p newt-core; echo done", ""),
    ] {
        let mut state = State::default();
        state.bind(root, &Scope::All);
        state.observe(root, &Scope::All, &Capture { command: Some((command.into(), root.display().to_string())), exit: Some(0), lines: vec![line.into()], truncated: false }, Some(ExecOutcome::Passed), None);
        let report = state.render(root, &Scope::All, "done");
        assert_eq!(assistant_prose(&format!("{report}done")), "done", "{report}");
        assert!(report.contains("pipeline exit 0 (inner command status not observed)"), "{report}");
        assert!(!report.contains("Passed; exit 0"), "{report}");
        if !line.is_empty() { assert!(report.contains("failure observed"), "{report}"); }
    }
}

/// H1: an audit locator is not a correction and must not become model speech.
#[test]
fn report_h1_audit_footer_is_labelled_not_a_correction() {
    let dir = fixture();
    let root = dir.path().to_str().unwrap();
    let spill = crate::agentic::SessionSpillStore::new([32; 16]);
    let claims = TurnClaims::capture(root, &Scope::All, None).with_evidence(review::Evidence {
        spill: Some(&spill),
        ..Default::default()
    });
    let report = finish("src/mod.rs is 999 lines.", root, &claims);
    assert!(
        report
            .lines()
            .last()
            .unwrap()
            .starts_with("newt review: spill:"),
        "{report}"
    );
    assert!(!report.contains("review evidence review"));
    assert!(assistant_prose(&report).is_empty());
    assert!(assistant_prose(&format!("{report}\n")).is_empty());
}

/// H1: one matching receipt cannot pay for the other, nor for another revision.
#[test]
fn report_h1_compound_publication_requires_both_matching_receipts() {
    let dir = fixture();
    let root = dir.path().to_str().unwrap();
    let text = PUBLICATION.replace(" (+594 −549)", "");
    for (push, pr) in [(true, false), (false, true), (false, false)] {
        let claims = TurnClaims::capture(root, &Scope::All, None);
        publications(&claims, root, push, pr);
        let report = finish(&text, root, &claims);
        assert!(
            model_explanation(&report).contains("[unverified by newt:"),
            "{report}"
        );
    }
    let claims = TurnClaims::capture(root, &Scope::All, None);
    publications(&claims, root, true, true);
    for wrong in [
        text.replace("d879ac1", "123abcd"),
        text.replace("/pull/41", "/pull/42"),
    ] {
        assert!(model_explanation(&finish(&wrong, root, &claims)).contains("[unverified by newt:"));
    }
}

/// PR #2856: a matching first branch cannot certify a later unmatched branch,
/// revision or PR. Separately delimited supported claims remain untouched.
#[test]
fn report_h1_compound_publication_checks_every_mention() {
    let dir = fixture();
    let root = dir.path().to_str().unwrap();
    let claims = TurnClaims::capture(root, &Scope::All, None);
    publications(&claims, root, true, true);
    let pr = "PR opened: https://github.com/example/project/pull/41";
    for text in [
        format!("Pushed branch refactor and pushed branch other, {pr}"),
        format!("Pushed branch other and pushed branch refactor, {pr}"),
        format!("Pushed branch `refactor` and pushed branch `other`, {pr}"),
        format!("Pushed d879ac1 and pushed 123abcd, {pr}"),
        "Pushed branch refactor, PR #41 opened and PR #42 opened".into(),
        format!("Pushed branch refactor, {pr} and PR opened: https://github.com/example/project/pull/42"),
    ] {
        let rendered = finish(&text, root, &claims);
        assert!(model_explanation(&rendered).contains("[unverified by newt:"), "{rendered}");
    }
    let supported = format!("Pushed branch refactor and pushed branch refactor, {pr}");
    assert_eq!(
        model_explanation(&finish(&supported, root, &claims)),
        supported
    );
    let separated = format!("Pushed branch refactor; pushed branch other; {pr}");
    let rendered = finish(&separated, root, &claims);
    let explanation = model_explanation(&rendered);
    assert!(
        explanation.starts_with("Pushed branch refactor; [unverified by newt: pushed branch other"),
        "{rendered}"
    );
    assert!(
        explanation.contains(&format!("]; {pr}\n\nnewt review:")),
        "{rendered}"
    );
}

/// H1: exact descriptors still abstain for unknown/ambiguous/approximate counts
/// and must not hide an unsupported quantitative residue.
#[test]
fn report_h1_descriptor_negative_controls() {
    let dir = fixture();
    let root = dir.path().to_str().unwrap();
    let claims = TurnClaims::capture(root, &Scope::All, None);
    for text in [
        "src/mod.rs (999 lines → largest in repo) moved into src/new.rs",
        "mod.rs (2 lines → largest in repo) moved into src/new.rs",
        "src/mod.rs (~2 lines → largest in repo) moved into src/new.rs",
        "src/mod.rs (2 lines) has a helper of 999 lines",
    ] {
        let report = finish(text, root, &claims);
        assert!(
            model_explanation(&report).contains("[unverified by newt:"),
            "{report}"
        );
    }
}

/// H1: test the real execution-result tap, including a successful tail after a
/// failure, a genuinely passing direct command, and a quoted pipe argument.
#[test]
fn report_h1_pipeline_and_direct_status_controls() {
    let dir = fixture();
    let root = dir.path();
    for (command, output, expected) in [
        (
            "cargo check -p newt-core",
            "error: could not compile `newt-core`",
            "Failed; exit 0",
        ),
        (
            "cargo check -p newt-core",
            "Finished `dev` profile",
            "Passed; exit 0",
        ),
        ("cargo test -p newt-core 'a|b'", "", "Passed; exit 0"),
        (
            "cargo test -p newt-core | tail -40",
            "test result: ok. 2 passed; 0 failed",
            "Cargo test result observed: ok (excerpt only)",
        ),
        (
            "cargo test -p newt-core | tail -40",
            "test result: FAILED. 1 passed; 1 failed\ntest result: ok. 2 passed; 0 failed",
            "failure observed",
        ),
        (
            "cargo nextest run | tail -40",
            "Summary [ 1.0s] 2 tests run: 1 passed, 1 failed",
            "failure observed",
        ),
    ] {
        let capture = std::sync::Mutex::new(Capture::default());
        let mut quiet = Quiet;
        let mut tap = Presentation {
            inner: &mut quiet,
            capture: Some(&capture),
        };
        tap.execution_command(command, root.to_str().unwrap());
        tap.execution_result(&serde_json::json!({"exit_code":0,"stdout":output}));
        let mut state = State::default();
        state.bind(root, &Scope::All);
        state.observe(
            root,
            &Scope::All,
            &capture.lock().unwrap(),
            Some(ExecOutcome::Passed),
            None,
        );
        let report = state.render(root, &Scope::All, "done");
        assert_eq!(
            assistant_prose(&format!("{report}done")),
            "done",
            "{report}"
        );
        assert!(report.contains(expected), "{report}");
        if command.contains(" | ") {
            assert!(!report.contains("Passed; exit"), "{report}");
        }
    }
}
