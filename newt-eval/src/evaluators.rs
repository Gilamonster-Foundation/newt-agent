//! Evaluator trait + the bundled checks (next commit fills in the
//! per-evaluator logic).

use std::sync::Arc;

use crate::scorecard::{EvalContext, EvalResult};

/// Trait every check implements.
///
/// `evaluate` is intentionally sync — the heavy lifting (subprocess +
/// ACP I/O) happens in the runner, and evaluators are simple
/// post-condition checks against the resulting [`EvalContext`].
pub trait Evaluator: Send + Sync {
    /// Stable, kebab-case name (matches `case.toml` entries).
    fn name(&self) -> &str;
    /// Inspect the post-run context and return a verdict.
    fn evaluate(&self, ctx: &EvalContext) -> EvalResult;
}

// ── Stub evaluators (real logic lands next commit) ──────────────────

pub struct DiffNonemptyEvaluator;
impl Evaluator for DiffNonemptyEvaluator {
    fn name(&self) -> &str {
        "diff_nonempty"
    }
    fn evaluate(&self, _ctx: &EvalContext) -> EvalResult {
        EvalResult::fail(self.name(), "not yet implemented")
    }
}

pub struct DiffAppliesEvaluator;
impl Evaluator for DiffAppliesEvaluator {
    fn name(&self) -> &str {
        "diff_applies"
    }
    fn evaluate(&self, _ctx: &EvalContext) -> EvalResult {
        EvalResult::fail(self.name(), "not yet implemented")
    }
}

pub struct RustCompilesEvaluator;
impl Evaluator for RustCompilesEvaluator {
    fn name(&self) -> &str {
        "rust_compiles"
    }
    fn evaluate(&self, _ctx: &EvalContext) -> EvalResult {
        EvalResult::fail(self.name(), "not yet implemented")
    }
}

pub struct TestsPassEvaluator;
impl Evaluator for TestsPassEvaluator {
    fn name(&self) -> &str {
        "tests_pass"
    }
    fn evaluate(&self, _ctx: &EvalContext) -> EvalResult {
        EvalResult::fail(self.name(), "not yet implemented")
    }
}

pub struct PatternMatchEvaluator;
impl Evaluator for PatternMatchEvaluator {
    fn name(&self) -> &str {
        "pattern_match"
    }
    fn evaluate(&self, _ctx: &EvalContext) -> EvalResult {
        EvalResult::fail(self.name(), "not yet implemented")
    }
}

/// Lookup an evaluator by `case.toml` name. Returns `None` for unknown
/// names — callers should treat this as a configuration error.
pub fn evaluator_by_name(name: &str) -> Option<Arc<dyn Evaluator>> {
    match name {
        "diff_nonempty" => Some(Arc::new(DiffNonemptyEvaluator)),
        "diff_applies" => Some(Arc::new(DiffAppliesEvaluator)),
        "rust_compiles" => Some(Arc::new(RustCompilesEvaluator)),
        "tests_pass" => Some(Arc::new(TestsPassEvaluator)),
        "pattern_match" => Some(Arc::new(PatternMatchEvaluator)),
        _ => None,
    }
}

/// The full set of bundled evaluators in canonical order.
pub fn default_evaluators() -> Vec<Arc<dyn Evaluator>> {
    vec![
        Arc::new(DiffNonemptyEvaluator),
        Arc::new(DiffAppliesEvaluator),
        Arc::new(RustCompilesEvaluator),
        Arc::new(TestsPassEvaluator),
        Arc::new(PatternMatchEvaluator),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_known_evaluators() {
        for name in [
            "diff_nonempty",
            "diff_applies",
            "rust_compiles",
            "tests_pass",
            "pattern_match",
        ] {
            let ev = evaluator_by_name(name).unwrap_or_else(|| panic!("missing: {name}"));
            assert_eq!(ev.name(), name);
        }
    }

    #[test]
    fn lookup_unknown_returns_none() {
        assert!(evaluator_by_name("nope").is_none());
    }

    #[test]
    fn default_set_has_five() {
        let evs = default_evaluators();
        assert_eq!(evs.len(), 5);
    }
}
