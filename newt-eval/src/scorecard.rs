//! Evaluation context, per-evaluator result, and the roll-up scorecard.
//!
//! These are the data structures the runner and the evaluators exchange.
//! They are deliberately Plain Old Data — `Serialize` so a CI run can dump
//! them as JSON, `Debug` for `cargo test` output.

use std::fmt;
use std::path::PathBuf;

use newt_acp_worker::TaskReply;
use newt_core::markup::table::{render_table, Align, Column};
use serde::{Deserialize, Serialize};

use crate::cases::TestCase;

/// What an evaluator sees after the worker finishes one turn.
///
/// - `case` — the test case definition (prompt, expected patterns, etc.).
/// - `workspace` — the post-worker state. The worker has already applied
///   any diff it produced.
/// - `baseline` — a parallel directory containing the *pre*-worker state,
///   handy for evaluators that need to diff before vs. after.
/// - `reply` — the [`TaskReply`] the worker returned over ACP.
#[derive(Debug, Clone)]
pub struct EvalContext {
    pub case: TestCase,
    pub workspace: PathBuf,
    pub baseline: PathBuf,
    pub reply: TaskReply,
}

/// One row of the scorecard — the verdict of one evaluator on one case.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvalResult {
    /// Evaluator name, e.g. `"diff_applies"`.
    pub evaluator: String,
    /// Hard pass/fail. `score == 1.0` implies `passed`; `passed` may also
    /// be set for partial credit if the evaluator chooses.
    pub passed: bool,
    /// 0.0–1.0 — finer-grained signal than `passed` alone.
    pub score: f64,
    /// Free-text explanation, included in scorecard tables.
    pub details: String,
}

impl EvalResult {
    /// Convenience constructor for a clean pass (score 1.0).
    pub fn pass(evaluator: impl Into<String>, details: impl Into<String>) -> Self {
        Self {
            evaluator: evaluator.into(),
            passed: true,
            score: 1.0,
            details: details.into(),
        }
    }

    /// Convenience constructor for a clean fail (score 0.0).
    pub fn fail(evaluator: impl Into<String>, details: impl Into<String>) -> Self {
        Self {
            evaluator: evaluator.into(),
            passed: false,
            score: 0.0,
            details: details.into(),
        }
    }
}

/// All evaluator verdicts for one case, plus the case's name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CaseScorecard {
    pub case_name: String,
    pub results: Vec<EvalResult>,
}

impl CaseScorecard {
    /// True if every evaluator passed.
    pub fn all_passed(&self) -> bool {
        !self.results.is_empty() && self.results.iter().all(|r| r.passed)
    }

    /// Mean score across all evaluators. `0.0` if `results` is empty so a
    /// case with no evaluators can't accidentally count as a win.
    pub fn mean_score(&self) -> f64 {
        if self.results.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.results.iter().map(|r| r.score).sum();
        sum / self.results.len() as f64
    }
}

/// Aggregate scorecard across N cases. Renders as a small Markdown-ish
/// table when printed for human eyes.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Scorecard {
    pub cases: Vec<CaseScorecard>,
}

impl Scorecard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, case: CaseScorecard) {
        self.cases.push(case);
    }

    /// True if every case passed every evaluator.
    pub fn all_passed(&self) -> bool {
        !self.cases.is_empty() && self.cases.iter().all(CaseScorecard::all_passed)
    }

    /// The scorecard's rows — one per (case, evaluator) result.
    ///
    /// DATA, not presentation: [`fmt::Display`] hands these to the one table
    /// algorithm. Keeping the two separable is what stops a second
    /// scorecard-shaped renderer growing beside this one (epic #1803, law 10).
    fn table_rows(&self) -> Vec<Vec<String>> {
        self.cases
            .iter()
            .flat_map(|case| {
                case.results.iter().map(move |r| {
                    vec![
                        case.case_name.clone(),
                        r.evaluator.clone(),
                        if r.passed { "ok" } else { "FAIL" }.to_string(),
                        format!("{:.2}", r.score),
                        r.details.clone(),
                    ]
                })
            })
            .collect()
    }
}

/// The scorecard's columns.
///
/// The caps are content policy, not layout — a 200-character evaluator detail
/// is not worth a 200-cell column — and they are the SAME caps the hand-rolled
/// renderer carried (28 / 16 / 60), now counted in display CELLS instead of
/// chars. That is the fix, not a side effect: the old `truncate` capped by
/// char and the `{:<28}` pad that followed measured the same string a third
/// way, so a CJK case name overflowed the column it had just been cut to fit.
fn table_columns() -> Vec<Column> {
    vec![
        Column::new("case").max_width(28),
        Column::new("evaluator").max_width(16),
        Column::new("pass"),
        Column::new("score").align(Align::Right),
        Column::new("details").max_width(60),
    ]
}

impl fmt::Display for Scorecard {
    /// A GFM pipe table through the one table algorithm (D3a, #1874).
    ///
    /// This replaced a bespoke fixed-width renderer. The byte diff is
    /// intentional and pinned by `the_scorecard_table_is_byte_exact_gfm`.
    /// Still deterministic, so snapshot tests can use it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.cases.is_empty() {
            return writeln!(f, "(no cases)");
        }
        f.write_str(&render_table(&table_columns(), &self.table_rows()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_result_pass_helper() {
        let r = EvalResult::pass("ev", "details");
        assert!(r.passed);
        assert_eq!(r.score, 1.0);
        assert_eq!(r.evaluator, "ev");
    }

    #[test]
    fn eval_result_fail_helper() {
        let r = EvalResult::fail("ev", "boom");
        assert!(!r.passed);
        assert_eq!(r.score, 0.0);
    }

    #[test]
    fn case_scorecard_all_passed_requires_results() {
        let empty = CaseScorecard {
            case_name: "x".into(),
            results: vec![],
        };
        assert!(!empty.all_passed());

        let mixed = CaseScorecard {
            case_name: "x".into(),
            results: vec![EvalResult::pass("a", ""), EvalResult::fail("b", "")],
        };
        assert!(!mixed.all_passed());

        let all_pass = CaseScorecard {
            case_name: "x".into(),
            results: vec![EvalResult::pass("a", ""), EvalResult::pass("b", "")],
        };
        assert!(all_pass.all_passed());
    }

    #[test]
    fn case_scorecard_mean_score() {
        let case = CaseScorecard {
            case_name: "x".into(),
            results: vec![
                EvalResult {
                    evaluator: "a".into(),
                    passed: true,
                    score: 1.0,
                    details: "".into(),
                },
                EvalResult {
                    evaluator: "b".into(),
                    passed: false,
                    score: 0.5,
                    details: "".into(),
                },
            ],
        };
        assert!((case.mean_score() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn case_scorecard_mean_of_empty_is_zero() {
        let case = CaseScorecard {
            case_name: "x".into(),
            results: vec![],
        };
        assert_eq!(case.mean_score(), 0.0);
    }

    #[test]
    fn scorecard_all_passed_empty_is_false() {
        let s = Scorecard::new();
        assert!(!s.all_passed());
    }

    #[test]
    fn scorecard_all_passed_propagates() {
        let mut s = Scorecard::new();
        s.push(CaseScorecard {
            case_name: "a".into(),
            results: vec![EvalResult::pass("ev", "")],
        });
        assert!(s.all_passed());
        s.push(CaseScorecard {
            case_name: "b".into(),
            results: vec![EvalResult::fail("ev", "")],
        });
        assert!(!s.all_passed());
    }

    /// **The intentional diff (D3a, #1874).** The scorecard was a bespoke
    /// fixed-width table; it is now a GFM pipe table from the one algorithm.
    /// These are the exact new bytes — the old ones are quoted in the PR body.
    #[test]
    fn the_scorecard_table_is_byte_exact_gfm() {
        let mut s = Scorecard::new();
        s.push(CaseScorecard {
            case_name: "001-foo".into(),
            results: vec![EvalResult::pass("diff_nonempty", "all good")],
        });
        s.push(CaseScorecard {
            case_name: "002-bar".into(),
            results: vec![EvalResult::fail("diff_nonempty", "boom")],
        });
        assert_eq!(
            s.to_string(),
            "\
| case    | evaluator     | pass | score | details  |
| ------- | ------------- | ---- | ----: | -------- |
| 001-foo | diff_nonempty | ok   |  1.00 | all good |
| 002-bar | diff_nonempty | FAIL |  0.00 | boom     |
"
        );
    }

    /// A detail string carrying a shell pipeline used to be pasted into a
    /// fixed-width row, where a `|` was just a character. In a pipe table an
    /// unescaped `|` FORGES A COLUMN — the row still renders, with the wrong
    /// data in the wrong columns. Reachable from any evaluator that quotes a
    /// command it ran.
    #[test]
    fn a_pipe_in_a_detail_cannot_forge_a_column() {
        let mut s = Scorecard::new();
        s.push(CaseScorecard {
            case_name: "c".into(),
            results: vec![EvalResult::fail("ev", "ran `grep x | wc -l`")],
        });
        let table = s.to_string();
        assert!(
            table.contains("`grep x \\| wc -l`"),
            "the pipe must be escaped: {table:?}"
        );
        // An escaped pipe is CONTENT: it must not be counted as a column
        // boundary, which is exactly the confusion the escape prevents.
        let unescaped_pipes = |line: &str| {
            let b = line.as_bytes();
            (0..b.len())
                .filter(|&i| b[i] == b'|' && (i == 0 || b[i - 1] != b'\\'))
                .count()
        };
        for line in table.lines() {
            assert_eq!(
                unescaped_pipes(line),
                6,
                "five columns means six boundaries, escaped pipes aside: {line:?}"
            );
        }
    }

    #[test]
    fn render_table_has_header_and_rows() {
        let mut s = Scorecard::new();
        s.push(CaseScorecard {
            case_name: "001-foo".into(),
            results: vec![EvalResult::pass("diff_nonempty", "all good")],
        });
        let table = s.to_string();
        assert!(table.contains("case "));
        assert!(table.contains("001-foo"));
        assert!(table.contains("diff_nonempty"));
        assert!(table.contains(" ok "));
    }

    #[test]
    fn render_table_empty() {
        let s = Scorecard::new();
        let table = s.to_string();
        assert!(table.contains("(no cases)"));
    }

    #[test]
    fn render_table_safe_with_multibyte_chars() {
        // Regression: truncate() used to slice by byte index and would
        // panic when a multi-byte codepoint straddled the boundary.
        let mut s = Scorecard::new();
        s.push(CaseScorecard {
            case_name: "x".into(),
            results: vec![EvalResult::fail(
                "ev",
                "an em dash — appears here, then a lot more text to force truncation",
            )],
        });
        // Must not panic.
        let _ = s.to_string();
    }

    #[test]
    fn render_table_truncates_long_strings() {
        let mut s = Scorecard::new();
        s.push(CaseScorecard {
            case_name: "x".repeat(50),
            results: vec![EvalResult::pass("evaluator", "details")],
        });
        let table = s.to_string();
        // The first column is capped at 28 chars + ellipsis.
        assert!(table.contains("…"));
    }
}
