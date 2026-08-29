//! Python bindings for `newt-eval` — sync surfaces only.
//!
//! Compiled only when the `pyo3` cargo feature is on. Exposes:
//! - `TestCase` + `MockResponse` data types (load + inspect cases)
//! - `EvalResult`, `CaseScorecard`, `Scorecard`
//! - `EvalContext` (constructed by the test harness, fed to evaluators)
//! - The five evaluators as Python classes with sync `evaluate(ctx)`
//! - `RunnerConfig` (the *config* struct only; the async `run_case`
//!   runner spawns a subprocess and is intentionally left to Rust)
//!
//! The async `run_case` is NOT bound here. Python consumers that want
//! to drive `newt worker` subprocesses for evaluation should do so
//! with `asyncio.create_subprocess_exec` against the released binary
//! — the same shape the Rust runner uses internally.

use std::path::PathBuf;
use std::time::Duration;

use crate::cases::{MockResponse, TestCase};
use crate::evaluators::{
    default_evaluators, evaluator_by_name, DiffAppliesEvaluator, DiffNonemptyEvaluator, Evaluator,
    PatternMatchEvaluator, RustCompilesEvaluator, TestsPassEvaluator,
};
use crate::runner::RunnerConfig;
use crate::scorecard::{CaseScorecard, EvalContext, EvalResult, Scorecard};
use newt_acp_worker::pyo3_module::PyTaskReply;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyModule, PyType};
use std::sync::Arc;

// ---- MockResponse ----

#[pyclass(
    name = "MockResponse",
    module = "newt_agent._newt_agent.eval",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyMockResponse {
    pub inner: MockResponse,
}

#[pymethods]
impl PyMockResponse {
    #[new]
    fn new(content: String) -> Self {
        Self {
            inner: MockResponse { content },
        }
    }

    #[getter]
    fn content(&self) -> &str {
        &self.inner.content
    }
}

// ---- TestCase ----

#[pyclass(
    name = "TestCase",
    module = "newt_agent._newt_agent.eval",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyTestCase {
    pub inner: TestCase,
}

#[pymethods]
impl PyTestCase {
    /// Load a test case from `dir/case.toml`.
    #[classmethod]
    fn load_dir(_cls: &Bound<'_, PyType>, dir: PathBuf) -> PyResult<Self> {
        let inner = TestCase::load_dir(&dir)
            .map_err(|e| PyRuntimeError::new_err(format!("load_dir: {e}")))?;
        Ok(Self { inner })
    }

    /// Load every case under `cases_dir/`. Sorted by case name.
    #[classmethod]
    fn load_all(_cls: &Bound<'_, PyType>, cases_dir: PathBuf) -> PyResult<Vec<Self>> {
        let cases = crate::cases::load_all(&cases_dir)
            .map_err(|e| PyRuntimeError::new_err(format!("load_all: {e}")))?;
        Ok(cases.into_iter().map(|inner| Self { inner }).collect())
    }

    /// Conventional cases directory: `<CARGO_MANIFEST_DIR>/cases`.
    /// Exposed for parity with the Rust API; in a wheel install this
    /// path points into the build tree and isn't typically useful.
    #[classmethod]
    fn default_cases_dir(_cls: &Bound<'_, PyType>) -> String {
        crate::cases::default_cases_dir().display().to_string()
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    /// The `output_matches` expected stdout (#957), if the case declares one.
    #[getter]
    fn expected_output(&self) -> Option<&str> {
        self.inner.expected_output.as_deref()
    }

    #[getter]
    fn description(&self) -> &str {
        &self.inner.description
    }

    #[getter]
    fn language(&self) -> &str {
        &self.inner.language
    }

    #[getter]
    fn prompt(&self) -> &str {
        &self.inner.prompt
    }

    #[getter]
    fn evaluators(&self) -> Vec<String> {
        self.inner.evaluators.clone()
    }

    #[getter]
    fn expected_patterns(&self) -> Vec<String> {
        self.inner.expected_patterns.clone()
    }

    #[getter]
    fn mock_response(&self) -> PyMockResponse {
        PyMockResponse {
            inner: self.inner.mock_response.clone(),
        }
    }

    #[getter]
    fn case_dir(&self) -> String {
        self.inner.case_dir.display().to_string()
    }

    fn workspace_fixture(&self) -> String {
        self.inner.workspace_fixture().display().to_string()
    }

    fn is_rust(&self) -> bool {
        self.inner.is_rust()
    }

    fn __repr__(&self) -> String {
        format!(
            "TestCase(name='{}', language='{}', evaluators={:?})",
            self.inner.name, self.inner.language, self.inner.evaluators
        )
    }
}

// ---- EvalResult ----

#[pyclass(
    name = "EvalResult",
    module = "newt_agent._newt_agent.eval",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyEvalResult {
    pub inner: EvalResult,
}

#[pymethods]
impl PyEvalResult {
    #[new]
    fn new(evaluator: String, passed: bool, score: f64, details: String) -> Self {
        Self {
            inner: EvalResult {
                evaluator,
                passed,
                score,
                details,
            },
        }
    }

    #[classmethod]
    fn pass_(_cls: &Bound<'_, PyType>, evaluator: String, details: String) -> Self {
        Self {
            inner: EvalResult::pass(evaluator, details),
        }
    }

    #[classmethod]
    fn fail(_cls: &Bound<'_, PyType>, evaluator: String, details: String) -> Self {
        Self {
            inner: EvalResult::fail(evaluator, details),
        }
    }

    #[getter]
    fn evaluator(&self) -> &str {
        &self.inner.evaluator
    }

    #[getter]
    fn passed(&self) -> bool {
        self.inner.passed
    }

    #[getter]
    fn score(&self) -> f64 {
        self.inner.score
    }

    #[getter]
    fn details(&self) -> &str {
        &self.inner.details
    }

    fn __repr__(&self) -> String {
        format!(
            "EvalResult(evaluator='{}', passed={}, score={:.2})",
            self.inner.evaluator, self.inner.passed, self.inner.score
        )
    }
}

// ---- CaseScorecard ----

#[pyclass(
    name = "CaseScorecard",
    module = "newt_agent._newt_agent.eval",
    skip_from_py_object
)]
pub struct PyCaseScorecard {
    pub inner: CaseScorecard,
}

#[pymethods]
impl PyCaseScorecard {
    #[new]
    fn new(case_name: String, results: Vec<PyRef<'_, PyEvalResult>>) -> Self {
        Self {
            inner: CaseScorecard {
                case_name,
                results: results.into_iter().map(|r| r.inner.clone()).collect(),
            },
        }
    }

    #[getter]
    fn case_name(&self) -> &str {
        &self.inner.case_name
    }

    #[getter]
    fn results(&self) -> Vec<PyEvalResult> {
        self.inner
            .results
            .iter()
            .cloned()
            .map(|inner| PyEvalResult { inner })
            .collect()
    }

    fn all_passed(&self) -> bool {
        self.inner.all_passed()
    }

    fn mean_score(&self) -> f64 {
        self.inner.mean_score()
    }
}

// ---- Scorecard ----

#[pyclass(name = "Scorecard", module = "newt_agent._newt_agent.eval")]
pub struct PyScorecard {
    pub inner: Scorecard,
}

#[pymethods]
impl PyScorecard {
    #[new]
    fn new() -> Self {
        Self {
            inner: Scorecard::new(),
        }
    }

    fn push(&mut self, case_name: String, results: Vec<PyRef<'_, PyEvalResult>>) {
        self.inner.push(CaseScorecard {
            case_name,
            results: results.into_iter().map(|r| r.inner.clone()).collect(),
        });
    }

    fn all_passed(&self) -> bool {
        self.inner.all_passed()
    }

    /// Python keeps `render_table`; Rust calls it `table`.
    ///
    /// D3a migrated the scorecard onto `newt_core::markup::table` and left
    /// this row armed in the sprawl ratchet at 1. The needle is
    /// `fn render_table(`, and it cannot tell a table RENDERER from a
    /// one-line binding to one — A0 §4.1.4 calls this "pyo3 exposure". There
    /// was never an implementation here to migrate, so the honest fix is for
    /// the file to stop DECLARING that name while the Python API keeps it.
    ///
    /// The rename is the whole change; the bytes Python receives are the
    /// same `Scorecard` Display. If a real `fn render_table(` ever lands in
    /// this file the ratchet sees a NEW site file and trips, which is the
    /// property that made removing the row safe.
    #[pyo3(name = "render_table")]
    fn table(&self) -> String {
        self.inner.to_string()
    }

    #[getter]
    fn cases(&self) -> Vec<String> {
        self.inner
            .cases
            .iter()
            .map(|c| c.case_name.clone())
            .collect()
    }
}

impl Default for PyScorecard {
    fn default() -> Self {
        Self::new()
    }
}

// ---- EvalContext ----

#[pyclass(name = "EvalContext", module = "newt_agent._newt_agent.eval")]
pub struct PyEvalContext {
    pub inner: EvalContext,
}

#[pymethods]
impl PyEvalContext {
    #[new]
    fn new(
        case: PyRef<'_, PyTestCase>,
        workspace: PathBuf,
        baseline: PathBuf,
        reply: PyRef<'_, PyTaskReply>,
    ) -> Self {
        Self {
            inner: EvalContext {
                case: case.inner.clone(),
                workspace,
                baseline,
                reply: reply.inner.clone(),
            },
        }
    }

    #[getter]
    fn case(&self) -> PyTestCase {
        PyTestCase {
            inner: self.inner.case.clone(),
        }
    }

    #[getter]
    fn workspace(&self) -> String {
        self.inner.workspace.display().to_string()
    }

    #[getter]
    fn baseline(&self) -> String {
        self.inner.baseline.display().to_string()
    }

    #[getter]
    fn reply(&self) -> PyTaskReply {
        PyTaskReply {
            inner: self.inner.reply.clone(),
        }
    }
}

// ---- Evaluator wrappers ----

macro_rules! py_evaluator {
    ($PyName:ident, $RustName:ident, $name_lit:literal) => {
        #[doc = concat!("Python wrapper for `", stringify!($RustName), "`.")]
        #[pyclass(name = $name_lit, module = "newt_agent._newt_agent.eval")]
        pub struct $PyName {
            inner: Arc<$RustName>,
        }

        #[pymethods]
        impl $PyName {
            #[new]
            fn new() -> Self {
                Self {
                    inner: Arc::new($RustName),
                }
            }

            fn name(&self) -> &str {
                self.inner.name()
            }

            fn evaluate(&self, ctx: PyRef<'_, PyEvalContext>) -> PyEvalResult {
                PyEvalResult {
                    inner: self.inner.evaluate(&ctx.inner),
                }
            }
        }

        impl Default for $PyName {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

py_evaluator!(
    PyDiffNonemptyEvaluator,
    DiffNonemptyEvaluator,
    "DiffNonemptyEvaluator"
);
py_evaluator!(
    PyDiffAppliesEvaluator,
    DiffAppliesEvaluator,
    "DiffAppliesEvaluator"
);
py_evaluator!(
    PyRustCompilesEvaluator,
    RustCompilesEvaluator,
    "RustCompilesEvaluator"
);
py_evaluator!(
    PyTestsPassEvaluator,
    TestsPassEvaluator,
    "TestsPassEvaluator"
);
py_evaluator!(
    PyPatternMatchEvaluator,
    PatternMatchEvaluator,
    "PatternMatchEvaluator"
);

#[pyfunction]
#[pyo3(name = "default_evaluator_names")]
fn py_default_evaluator_names() -> Vec<String> {
    default_evaluators()
        .iter()
        .map(|e| e.name().to_string())
        .collect()
}

#[pyfunction]
#[pyo3(name = "evaluator_known")]
fn py_evaluator_known(name: &str) -> bool {
    evaluator_by_name(name).is_some()
}

// ---- RunnerConfig (data only) ----

#[pyclass(name = "RunnerConfig", module = "newt_agent._newt_agent.eval")]
pub struct PyRunnerConfig {
    pub inner: RunnerConfig,
}

#[pymethods]
impl PyRunnerConfig {
    #[new]
    fn new(worker_bin: PathBuf) -> Self {
        Self {
            inner: RunnerConfig::new(worker_bin),
        }
    }

    fn with_mock_endpoint(mut slf: PyRefMut<'_, Self>, url: String) -> PyRefMut<'_, Self> {
        slf.inner.mock_endpoint = Some(url);
        slf
    }

    fn with_model(mut slf: PyRefMut<'_, Self>, model: String) -> PyRefMut<'_, Self> {
        slf.inner.model_override = Some(model);
        slf
    }

    fn with_timeout_ms(mut slf: PyRefMut<'_, Self>, timeout_ms: u64) -> PyRefMut<'_, Self> {
        slf.inner.timeout = Duration::from_millis(timeout_ms);
        slf
    }

    fn with_coder_mode(mut slf: PyRefMut<'_, Self>, on: bool) -> PyRefMut<'_, Self> {
        slf.inner.coder_mode = on;
        slf
    }

    #[getter]
    fn worker_bin(&self) -> String {
        self.inner.worker_bin.display().to_string()
    }

    #[getter]
    fn mock_endpoint(&self) -> Option<String> {
        self.inner.mock_endpoint.clone()
    }

    #[getter]
    fn model_override(&self) -> Option<String> {
        self.inner.model_override.clone()
    }

    #[getter]
    fn timeout_ms(&self) -> u64 {
        self.inner.timeout.as_millis() as u64
    }

    #[getter]
    fn coder_mode(&self) -> bool {
        self.inner.coder_mode
    }
}

/// Register the `eval` submodule on the parent `_newt_agent` module.
pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(py, "eval")?;
    m.add_class::<PyMockResponse>()?;
    m.add_class::<PyTestCase>()?;
    m.add_class::<PyEvalResult>()?;
    m.add_class::<PyCaseScorecard>()?;
    m.add_class::<PyScorecard>()?;
    m.add_class::<PyEvalContext>()?;
    m.add_class::<PyDiffNonemptyEvaluator>()?;
    m.add_class::<PyDiffAppliesEvaluator>()?;
    m.add_class::<PyRustCompilesEvaluator>()?;
    m.add_class::<PyTestsPassEvaluator>()?;
    m.add_class::<PyPatternMatchEvaluator>()?;
    m.add_class::<PyRunnerConfig>()?;
    m.add_function(wrap_pyfunction!(py_default_evaluator_names, &m)?)?;
    m.add_function(wrap_pyfunction!(py_evaluator_known, &m)?)?;
    parent.add_submodule(&m)?;
    Ok(())
}
