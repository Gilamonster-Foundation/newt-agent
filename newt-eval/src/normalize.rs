//! Output normalization strategies for the `output_matches` oracle (#959).
//!
//! The three-Cs move: tolerating whitespace / formatting differences between a
//! program's stdout and the case's `expected_output` is **configuration, not
//! code**. A case lists named strategies in `[output_match].normalize`; they are
//! looked up here, composed into a pipeline, and applied **in order** to both
//! sides before comparison.
//!
//! Two flavors of strategy:
//! - **String transforms** (`trim`, `collapse_whitespace`, `trailing_newline`,
//!   `regex_extract`): pure `String -> String`, applied to actual and expected
//!   alike.
//! - **Comparison mode** (`numeric_tolerance`): after the string transforms, if
//!   both sides parse as numbers, compare with `|a - b| <= epsilon` instead of
//!   byte equality.
//!
//! Tolerant-load convention (matching language packs): an **unknown** strategy
//! name, or `regex_extract` with a missing/invalid pattern, is **skipped with a
//! warning** — never fatal. An **empty** strategy list is exact-string match,
//! preserving the step-1 (#958) default.

use regex::Regex;

use crate::cases::OutputMatch;

/// Default tolerance when `numeric_tolerance` is requested without an epsilon.
pub const DEFAULT_EPSILON: f64 = 1e-9;

/// One resolved step of the pipeline. `regex_extract` carries its compiled
/// pattern so the compile (and its possible failure) happens once, at build.
enum Strategy {
    Trim,
    CollapseWhitespace,
    TrailingNewline,
    RegexExtract(Regex),
    /// Not a string transform — a flag that switches the final compare to a
    /// numeric one within `epsilon`.
    NumericTolerance,
}

impl Strategy {
    /// Apply a string transform. `NumericTolerance` is a no-op here (it only
    /// affects the final comparison), so the pipeline can map uniformly.
    fn transform(&self, s: &str) -> String {
        match self {
            Self::Trim => s.trim().to_string(),
            // Runs of whitespace → a single space. A leading/trailing run
            // collapses to a single space (pair `trim` before this to drop
            // edges entirely).
            Self::CollapseWhitespace => collapse_whitespace(s),
            // Drop any trailing CR/LF run — the classic "did the program end
            // with a newline" mismatch.
            Self::TrailingNewline => s.trim_end_matches(['\r', '\n']).to_string(),
            // Capture group 1 of the first match; if nothing matches, leave the
            // string unchanged (a deliberate no-op, so a genuine mismatch still
            // fails rather than silently passing).
            Self::RegexExtract(re) => re
                .captures(s)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
                .unwrap_or_else(|| s.to_string()),
            Self::NumericTolerance => s.to_string(),
        }
    }
}

/// Collapse every run of whitespace to a single ASCII space, edges included.
pub fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

/// The outcome of comparing two strings through a normalize pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    /// Whether the two sides are considered equal after normalization.
    pub equal: bool,
    /// Non-fatal notes (unknown strategy skipped, regex failed to compile, …).
    pub warnings: Vec<String>,
}

/// Resolve strategy names into a pipeline, surfacing warnings for anything
/// skipped. Unknown names and `regex_extract` without a usable pattern are
/// dropped with a warning rather than erroring.
fn build(om: &OutputMatch) -> (Vec<Strategy>, Vec<String>) {
    let mut steps = Vec::new();
    let mut warnings = Vec::new();
    for name in &om.normalize {
        match name.as_str() {
            "trim" => steps.push(Strategy::Trim),
            "collapse_whitespace" => steps.push(Strategy::CollapseWhitespace),
            "trailing_newline" => steps.push(Strategy::TrailingNewline),
            "numeric_tolerance" => steps.push(Strategy::NumericTolerance),
            "regex_extract" => match om.extract_pattern.as_deref() {
                Some(pat) => match Regex::new(pat) {
                    Ok(re) => steps.push(Strategy::RegexExtract(re)),
                    Err(e) => {
                        warnings.push(format!("regex_extract: invalid pattern {pat:?}: {e}"));
                    }
                },
                None => warnings
                    .push("regex_extract: no extract_pattern configured; skipped".to_string()),
            },
            other => warnings.push(format!("unknown normalize strategy {other:?}; skipped")),
        }
    }
    (steps, warnings)
}

/// Apply `om`'s normalization pipeline to `actual` and `expected`, then report
/// whether they match. An empty `normalize` list is an exact byte compare.
pub fn compare(actual: &str, expected: &str, om: &OutputMatch) -> Comparison {
    let (steps, warnings) = build(om);

    // Apply every string transform, in order, to both sides.
    let mut a = actual.to_string();
    let mut e = expected.to_string();
    for step in &steps {
        a = step.transform(&a);
        e = step.transform(&e);
    }

    let numeric = steps
        .iter()
        .any(|s| matches!(s, Strategy::NumericTolerance));
    let equal = if numeric {
        match (a.trim().parse::<f64>(), e.trim().parse::<f64>()) {
            (Ok(av), Ok(ev)) => (av - ev).abs() <= om.epsilon.unwrap_or(DEFAULT_EPSILON),
            // Not both numeric → fall back to the (already-normalized) string
            // compare, so a non-numeric line still gets a sane verdict.
            _ => a == e,
        }
    } else {
        a == e
    };

    Comparison { equal, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn om(normalize: &[&str]) -> OutputMatch {
        OutputMatch {
            normalize: normalize.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    // ── individual string transforms ────────────────────────────────

    #[test]
    fn trim_strips_leading_and_trailing_whitespace() {
        assert_eq!(Strategy::Trim.transform("  hi \n"), "hi");
        assert_eq!(Strategy::Trim.transform(""), "");
        assert_eq!(Strategy::Trim.transform("   \t\n "), "");
    }

    #[test]
    fn collapse_whitespace_reduces_runs_to_single_space() {
        assert_eq!(
            Strategy::CollapseWhitespace.transform("a  \t b\nc"),
            "a b c"
        );
        // edges collapse to a single space (pair with trim to drop them).
        assert_eq!(Strategy::CollapseWhitespace.transform("  a  "), " a ");
        assert_eq!(Strategy::CollapseWhitespace.transform("   "), " ");
        assert_eq!(Strategy::CollapseWhitespace.transform(""), "");
    }

    #[test]
    fn trailing_newline_drops_only_trailing_crlf() {
        assert_eq!(Strategy::TrailingNewline.transform("x\n"), "x");
        assert_eq!(Strategy::TrailingNewline.transform("x\r\n"), "x");
        assert_eq!(Strategy::TrailingNewline.transform("x\n\n"), "x");
        // interior newlines untouched.
        assert_eq!(Strategy::TrailingNewline.transform("a\nb\n"), "a\nb");
    }

    #[test]
    fn regex_extract_pulls_capture_group_one() {
        let re = Regex::new(r"Result:\s*(\d+)").unwrap();
        let s = Strategy::RegexExtract(re);
        assert_eq!(s.transform("noise\nResult: 42\nmore"), "42");
        // no match → unchanged (so a real mismatch still fails).
        assert_eq!(s.transform("nothing here"), "nothing here");
    }

    // ── comparison / composition ────────────────────────────────────

    #[test]
    fn empty_normalize_is_exact_match() {
        assert!(compare("a\n", "a\n", &om(&[])).equal);
        assert!(!compare("a\n", "a", &om(&[])).equal);
    }

    #[test]
    fn trim_then_collapse_matches_reformatted_output() {
        let c = compare("  a   b  ", "a b", &om(&["trim", "collapse_whitespace"]));
        assert!(c.equal, "{c:?}");
        assert!(c.warnings.is_empty());
    }

    #[test]
    fn order_matters_collapse_before_trim_leaves_edge_space() {
        // collapse first turns "  a  " into " a ", then trim removes edges → "a".
        // reversed also lands on "a"; both orders equal here, but assert the
        // pipeline runs in the given order without panicking on either.
        assert!(compare("  a  ", "a", &om(&["collapse_whitespace", "trim"])).equal);
        assert!(compare("  a  ", "a", &om(&["trim", "collapse_whitespace"])).equal);
    }

    #[test]
    fn numeric_tolerance_matches_within_epsilon() {
        let mut o = om(&["numeric_tolerance"]);
        o.epsilon = Some(0.01);
        assert!(compare("3.14159", "3.14", &o).equal, "just inside");
        assert!(!compare("3.20", "3.14", &o).equal, "just outside");
    }

    #[test]
    fn numeric_tolerance_default_epsilon_is_tight() {
        // No epsilon set → DEFAULT_EPSILON (1e-9): only near-identical passes.
        let o = om(&["numeric_tolerance"]);
        assert!(compare("2.0000000001", "2.0", &o).equal);
        assert!(!compare("2.001", "2.0", &o).equal);
    }

    #[test]
    fn numeric_tolerance_falls_back_to_string_when_not_numeric() {
        let mut o = om(&["numeric_tolerance"]);
        o.epsilon = Some(1.0);
        // "hi" doesn't parse → compare as strings.
        assert!(compare("hi", "hi", &o).equal);
        assert!(!compare("hi", "bye", &o).equal);
    }

    #[test]
    fn regex_extract_then_numeric_tolerance_composes() {
        let mut o = om(&["regex_extract", "numeric_tolerance"]);
        o.extract_pattern = Some(r"Result:\s*([\d.]+)".to_string());
        o.epsilon = Some(0.5);
        assert!(compare("Result: 42.3\n", "The answer is Result: 42.0", &o).equal);
    }

    #[test]
    fn unknown_strategy_is_skipped_with_warning_and_compare_still_runs() {
        let c = compare(" x ", "x", &om(&["bogus", "trim"]));
        assert!(c.equal, "trim still applied");
        assert_eq!(c.warnings.len(), 1);
        assert!(c.warnings[0].contains("bogus"));
    }

    #[test]
    fn regex_extract_without_pattern_warns_and_is_skipped() {
        let c = compare("Result: 5", "Result: 5", &om(&["regex_extract"]));
        assert!(
            c.equal,
            "no pattern → strategy skipped, exact compare holds"
        );
        assert_eq!(c.warnings.len(), 1);
        assert!(c.warnings[0].contains("no extract_pattern"));
    }

    #[test]
    fn regex_extract_invalid_pattern_warns_and_is_skipped() {
        let mut o = om(&["regex_extract"]);
        o.extract_pattern = Some("(unclosed".to_string());
        let c = compare("a", "a", &o);
        assert!(c.equal);
        assert_eq!(c.warnings.len(), 1);
        assert!(c.warnings[0].contains("invalid pattern"));
    }
}
