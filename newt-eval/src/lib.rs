//! `newt-eval` — end-to-end evaluation framework for the `newt worker` binary.
//!
//! This crate is the **scorecard** for the worker. It answers:
//! "does this version of the worker actually produce useful patches?"
//!
//! It is also the dogfood substrate for the drake-arbiter scoring pattern —
//! the same shape of test-case + evaluators + scorecard that the arbiter
//! uses to grade swarm candidates.
//!
//! # Two modes
//!
//! - **Mock mode** — runs in CI. A `wiremock` HTTP server stands in for
//!   Ollama and returns a canned diff. The runner spawns the real `newt
//!   worker` binary, drives it over ACP stdio JSON-RPC, and the evaluators
//!   verify the worker did the right thing with the canned response. Fully
//!   deterministic.
//! - **Live mode** — opt-in via `just eval`. No mock — the worker talks to
//!   a real local Ollama. Used to compare models / track regressions on a
//!   developer workstation. **Not** a CI gate.
//!
//! # Shape
//!
//! - [`TestCase`] — loaded from `cases/NNN-name/case.toml` plus a
//!   `workspace/` fixture directory.
//! - [`Evaluator`] — trait; one instance per check (diff non-empty, diff
//!   applies, rust compiles, tests pass, regex pattern match).
//! - [`EvalContext`] — what an evaluator sees after the worker runs.
//! - [`EvalResult`] / [`Scorecard`] — the output rows + their roll-up.
//! - [`runner`] — spawns the `newt worker` subprocess and drives ACP.
//!
//! See `newt-eval/README.md` for how to add a new case.

pub mod cases;
pub mod evaluators;
pub mod runner;
pub mod scorecard;

#[cfg(feature = "pyo3")]
pub mod pyo3_module;

pub use cases::{default_cases_dir, load_all, MockResponse, TestCase};
pub use evaluators::{
    default_evaluators, evaluator_by_name, DiffAppliesEvaluator, DiffNonemptyEvaluator, Evaluator,
    PatternMatchEvaluator, RustCompilesEvaluator, TestsPassEvaluator,
};
pub use runner::{run_case, RunOutcome, RunnerConfig};
pub use scorecard::{CaseScorecard, EvalContext, EvalResult, Scorecard};
