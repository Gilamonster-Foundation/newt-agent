//! Real-subprocess integration tests for the `output_matches` oracle (#960).
//!
//! These execute an *actual* program via [`SubprocessRunner`], so they are
//! **real-resource** tests: marked `#[ignore]` to stay out of the per-PR unit
//! run (`cargo test --workspace`), and run in the **weekly + release** tiers,
//! single-threaded (`-- --ignored --test-threads=1`).
//!
//! They require `python3` on `PATH` (the weekly/release runners have it).
//!
//! Run locally with:
//! ```text
//! cargo test -p newt-eval --test output_matches_real -- --ignored --test-threads=1
//! ```

use std::path::Path;

use newt_acp_worker::TaskReply;
use newt_eval::evaluators::{Evaluator, OutputMatchesEvaluator, SubprocessRunner};
use newt_eval::{EvalContext, TestCase};

/// Build an `EvalContext` whose graded program is `argv`, run in `workspace`,
/// with the given `expected_output` and normalization strategies.
fn ctx_for(
    workspace: &Path,
    argv: &[&str],
    expected_output: &str,
    normalize: &[&str],
    extract_pattern: Option<&str>,
    epsilon: Option<f64>,
    timeout_ms: Option<u64>,
) -> EvalContext {
    let case = TestCase {
        name: "output-matches-real".to_string(),
        description: "real subprocess oracle fixture".to_string(),
        language: "python".to_string(),
        prompt: String::new(),
        evaluators: vec!["output_matches".to_string()],
        expected_patterns: vec![],
        expected_output: Some(expected_output.to_string()),
        output_match: Some(newt_eval::cases::OutputMatch {
            run: argv.iter().map(|s| (*s).to_string()).collect(),
            timeout_ms,
            normalize: normalize.iter().map(|s| (*s).to_string()).collect(),
            epsilon,
            extract_pattern: extract_pattern.map(str::to_string),
        }),
        mock_response: newt_eval::MockResponse {
            content: String::new(),
        },
        difficulty: "L1".to_string(),
        case_dir: workspace.to_path_buf(),
    };
    EvalContext {
        case,
        workspace: workspace.to_path_buf(),
        baseline: workspace.to_path_buf(),
        // The output oracle ignores the reply entirely (it grades the program's
        // stdout, not the diff); any well-formed reply will do.
        reply: TaskReply::new("test-model", "content", "", false).unwrap(),
    }
}

/// The tiny fixture "solution": prints a computed value surrounded by noise, so
/// the normalize pipeline (regex_extract + numeric_tolerance) has real work.
fn write_solution(dir: &Path) {
    std::fs::write(
        dir.join("solution.py"),
        "print('computing...')\nprint(f'Result: {6 * 7:.1f}')\n",
    )
    .unwrap();
}

fn python() -> &'static str {
    "python3"
}

#[test]
#[ignore = "real-resource: weekly/release tier, needs python3, run single-threaded"]
fn output_matches_passes_against_a_real_program() {
    let dir = tempfile::tempdir().unwrap();
    write_solution(dir.path());

    // Program prints "computing...\nResult: 42.0\n"; regex_extract pulls the
    // number, numeric_tolerance compares it to the expected 42 within epsilon.
    let ctx = ctx_for(
        dir.path(),
        &[python(), "solution.py"],
        "42",
        &["regex_extract", "numeric_tolerance"],
        Some(r"Result:\s*([\d.]+)"),
        Some(0.001),
        Some(10_000),
    );
    let r = OutputMatchesEvaluator::new(SubprocessRunner).evaluate(&ctx);
    assert!(r.passed, "expected pass, got: {r:?}");
}

#[test]
#[ignore = "real-resource: weekly/release tier, needs python3, run single-threaded"]
fn output_matches_fails_against_a_real_program_with_wrong_expected() {
    let dir = tempfile::tempdir().unwrap();
    write_solution(dir.path());

    let ctx = ctx_for(
        dir.path(),
        &[python(), "solution.py"],
        "99", // deliberately wrong
        &["regex_extract", "numeric_tolerance"],
        Some(r"Result:\s*([\d.]+)"),
        Some(0.001),
        Some(10_000),
    );
    let r = OutputMatchesEvaluator::new(SubprocessRunner).evaluate(&ctx);
    assert!(!r.passed, "expected fail, got: {r:?}");
    assert!(r.details.contains("!="), "details: {}", r.details);
}

#[test]
#[ignore = "real-resource: weekly/release tier, needs python3, run single-threaded"]
fn output_matches_reports_timeout_for_a_hanging_program() {
    let dir = tempfile::tempdir().unwrap();

    // A program that sleeps well past the budget → killed, reported timed_out.
    let ctx = ctx_for(
        dir.path(),
        &[python(), "-c", "import time; time.sleep(60)"],
        "never",
        &[],
        None,
        None,
        Some(300), // 300ms budget
    );
    let r = OutputMatchesEvaluator::new(SubprocessRunner).evaluate(&ctx);
    assert!(!r.passed, "expected fail, got: {r:?}");
    assert!(r.details.contains("timed out"), "details: {}", r.details);
}
