//! Golden bridge: the Python grader consumes actual Rust-composed report bytes.
use super::*;
use crate::agentic::artifact_read::{ArtifactReadContext, ArtifactSource, SessionArtifactStore};

#[test]
fn report_stage2_grader_fixture_is_a_real_composed_report() {
    let root = Path::new("stage2-fixture-missing-root");
    assert!(!root.exists());
    let store = SessionArtifactStore::new("grader-fixture").unwrap();
    let prompt = crate::prompt::PromptId::new();
    let context = ArtifactReadContext::new(Some(prompt), Some(prompt), Some(prompt), Some(&store));
    let mut state = State::default();
    state.bind(root, &Scope::All);
    state.observe(root, &Capture {
        command: Some(("cargo test -p core".into(), root.display().to_string())),
        exit: Some(0),
        lines: vec!["test result: ok. 1850 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.0s".into()],
        truncated: false,
    }, Some(ExecOutcome::Passed), None);
    let original = "Finished refactor.\n`cargo test -p core` — 1617 tests passed.\nThe largest file is ~3,800 lines.\n";
    let report = state.compose(
        root,
        &Scope::All,
        original,
        original,
        review::Evidence {
            sink: Some(&store),
            context: Some(context),
            spill: None,
        },
        None,
    );
    let outcome =
        crate::agentic::record_turn_outcome(&store, context, &report, None, None, 1, None).unwrap();
    assert_eq!(outcome.body.as_deref(), Some(report.as_str()));
    let records = store.list_for_prompt(prompt, 0, 100).unwrap();
    let audits: Vec<_> = records
        .records
        .iter()
        .filter(|r| r.metadata["schema"] == "newt.report-review/v1")
        .map(|r| serde_json::json!({"body":r.body,"metadata":r.metadata}))
        .collect();
    let fixture = serde_json::json!({"report":report, "original_model_draft":original, "review_records":audits});
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../scripts/eval/tests/fixtures/refactor/composed-report.json"
    );
    if std::env::var_os("NEWT_UPDATE_REPORT_FIXTURE").is_some() {
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap()),
        )
        .unwrap();
    } else {
        let expected: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/eval/tests/fixtures/refactor/composed-report.json"
        ))
        .unwrap();
        assert_eq!(fixture, expected);
    }
}
