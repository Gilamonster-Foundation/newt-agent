//! End-to-end coverage for `grade_workspace` (the `newt-eval grade` core).
//!
//! These exercise the real diff reconstruction — filesystem copy + `git` — so
//! per the repo's testing strategy they live in the EXPENSIVE tier (integration
//! tests), never the fully-mocked unit tier. Each test uses its own tempdirs,
//! so they are independent; run the binary single-threaded if it ever contends.

use std::fs;
use std::path::Path;

use newt_eval::{grade_workspace, CaseScorecard, EvalResult, MockResponse, TestCase};

/// Build a case whose fixture lives at `<case_dir>/workspace/` with one seed
/// file, graded by the named evaluators (the no-cargo subset keeps it fast).
fn seed_case(case_dir: &Path, evaluators: &[&str], patterns: &[&str]) -> TestCase {
    let ws = case_dir.join("workspace");
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::write(
        ws.join("src/lib.rs"),
        "pub fn greet() -> &'static str {\n    \"hi\"\n}\n",
    )
    .unwrap();
    TestCase {
        name: "t-grade".to_string(),
        description: "grade-subcommand integration case".to_string(),
        language: "rust".to_string(),
        prompt: String::new(),
        evaluators: evaluators.iter().map(|s| s.to_string()).collect(),
        expected_patterns: patterns.iter().map(|s| s.to_string()).collect(),
        mock_response: MockResponse {
            content: String::new(),
        },
        difficulty: "L1".to_string(),
        case_dir: case_dir.to_path_buf(),
        expected_output: None,
        output_match: None,
    }
}

/// A post-run workspace tree containing `src/lib.rs` with `content`.
fn post_tree(content: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), content).unwrap();
    dir
}

fn result<'a>(card: &'a CaseScorecard, name: &str) -> &'a EvalResult {
    card.results
        .iter()
        .find(|r| r.evaluator == name)
        .unwrap_or_else(|| panic!("evaluator {name} missing from scorecard"))
}

#[test]
fn grades_a_changed_workspace_against_the_named_evaluators() {
    let case_dir = tempfile::tempdir().unwrap();
    let case = seed_case(
        case_dir.path(),
        &["diff_nonempty", "pattern_match"],
        &["hello"],
    );
    // post-run tree introduces the change the case's pattern expects.
    let post = post_tree("pub fn greet() -> &'static str {\n    \"hello\"\n}\n");

    let card = grade_workspace(&case, post.path()).unwrap();

    assert_eq!(card.case_name, "t-grade");
    assert!(
        result(&card, "diff_nonempty").passed,
        "reconstructed diff should be non-empty: {}",
        result(&card, "diff_nonempty").details
    );
    assert!(
        result(&card, "pattern_match").passed,
        "diff introduces \"hello\", so pattern_match should pass: {}",
        result(&card, "pattern_match").details
    );
}

#[test]
fn unchanged_workspace_reconstructs_an_empty_diff_and_fails_nonempty() {
    let case_dir = tempfile::tempdir().unwrap();
    let case = seed_case(case_dir.path(), &["diff_nonempty"], &[]);
    // identical to the fixture → no change → empty reconstructed diff.
    let post = post_tree("pub fn greet() -> &'static str {\n    \"hi\"\n}\n");

    let card = grade_workspace(&case, post.path()).unwrap();

    assert!(
        !result(&card, "diff_nonempty").passed,
        "an unchanged tree must fail diff_nonempty (no work was delivered)"
    );
}

#[test]
fn pattern_match_fails_when_the_change_does_not_contain_the_pattern() {
    let case_dir = tempfile::tempdir().unwrap();
    let case = seed_case(case_dir.path(), &["pattern_match"], &["nonexistent_token"]);
    let post = post_tree("pub fn greet() -> &'static str {\n    \"hello\"\n}\n");

    let card = grade_workspace(&case, post.path()).unwrap();

    assert!(
        !result(&card, "pattern_match").passed,
        "the expected pattern is absent from the diff, so pattern_match should fail"
    );
}

#[test]
fn empty_evaluator_list_falls_back_to_the_default_set() {
    let case_dir = tempfile::tempdir().unwrap();
    let case = seed_case(case_dir.path(), &[], &[]);
    let post = post_tree("pub fn greet() -> &'static str {\n    \"hello\"\n}\n");

    let card = grade_workspace(&case, post.path()).unwrap();

    // the default set is broader than any single named evaluator.
    assert!(
        card.results.len() > 1,
        "empty evaluator list should run the full default set, got {}",
        card.results.len()
    );
    assert!(card.results.iter().any(|r| r.evaluator == "diff_nonempty"));
}
