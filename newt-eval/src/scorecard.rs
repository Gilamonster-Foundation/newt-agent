//! Evaluation context, per-evaluator result, and the roll-up scorecard.
//!
//! These are the data structures the runner and the evaluators exchange.
//! They are deliberately Plain Old Data — `Serialize` so a CI run can dump
//! them as JSON, `Debug` for `cargo test` output.

use std::path::PathBuf;

use newt_acp_worker::TaskReply;
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

    /// Render a fixed-width text table suitable for stdout. The output is
    /// deterministic so snapshot tests can use it.
    pub fn render_table(&self) -> String {
        let mut out = String::new();
        out.push_str("case                          evaluator         pass  score  details\n");
        out.push_str("----------------------------  ----------------  ----  -----  -------\n");
        for case in &self.cases {
            for r in &case.results {
                let case_name = truncate(&case.case_name, 28);
                let ev = truncate(&r.evaluator, 16);
                let pass = if r.passed { "ok" } else { "FAIL" };
                let details = truncate(&r.details.replace('\n', " "), 60);
                out.push_str(&format!(
                    "{case_name:<28}  {ev:<16}  {pass:<4}  {:>5.2}  {details}\n",
                    r.score
                ));
            }
        }
        if self.cases.is_empty() {
            out.push_str("(no cases)\n");
        }
        out
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
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

    #[test]
    fn render_table_has_header_and_rows() {
        let mut s = Scorecard::new();
        s.push(CaseScorecard {
            case_name: "001-foo".into(),
            results: vec![EvalResult::pass("diff_nonempty", "all good")],
        });
        let table = s.render_table();
        assert!(table.contains("case "));
        assert!(table.contains("001-foo"));
        assert!(table.contains("diff_nonempty"));
        assert!(table.contains(" ok "));
    }

    #[test]
    fn render_table_empty() {
        let s = Scorecard::new();
        let table = s.render_table();
        assert!(table.contains("(no cases)"));
    }

    #[test]
    fn render_table_truncates_long_strings() {
        let mut s = Scorecard::new();
        s.push(CaseScorecard {
            case_name: "x".repeat(50),
            results: vec![EvalResult::pass("evaluator", "details")],
        });
        let table = s.render_table();
        // The first column is capped at 28 chars + ellipsis.
        assert!(table.contains("…"));
    }
}
