//! Python bindings for `newt-acp-worker` — data types only.
//!
//! Compiled only when the `pyo3` cargo feature is on. Exposes the
//! wire-format types (`TaskReply`, `Session`) so Python consumers can
//! build / inspect ACP traffic without re-implementing the schema.
//!
//! The async `AcpServer.run` stdio loop is intentionally NOT bound:
//! piping a Python-side reader/writer pair through tokio's stdin/stdout
//! is brittle, and the Rust `newt worker` binary is the supported way
//! to run the server. Python consumers that want to drive an ACP
//! conversation typically do so by spawning the binary as a subprocess
//! and exchanging newline-delimited JSON on its stdio — exactly the
//! pattern the `newt-eval` runner uses.

use std::path::PathBuf;

use crate::{capture_diff, is_empty_diff, Session, TaskReply};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyModule, PyType};

// ---- Session ----

/// Per-session ACP state.
#[pyclass(
    name = "Session",
    module = "newt_agent._newt_agent.acp_worker",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PySession {
    pub inner: Session,
}

#[pymethods]
impl PySession {
    #[new]
    #[pyo3(signature = (workspace_path, coder_enabled = false, model_override = None))]
    fn new(workspace_path: PathBuf, coder_enabled: bool, model_override: Option<String>) -> Self {
        Self {
            inner: Session {
                workspace_path,
                model_override,
                coder_enabled,
            },
        }
    }

    #[getter]
    fn workspace_path(&self) -> String {
        self.inner.workspace_path.display().to_string()
    }

    #[getter]
    fn coder_enabled(&self) -> bool {
        self.inner.coder_enabled
    }

    #[getter]
    fn model_override(&self) -> Option<String> {
        self.inner.model_override.clone()
    }

    fn __repr__(&self) -> String {
        format!(
            "Session(workspace_path='{}', coder_enabled={}, model_override={:?})",
            self.inner.workspace_path.display(),
            self.inner.coder_enabled,
            self.inner.model_override,
        )
    }
}

// ---- TaskReply ----

/// One ACP `prompt` reply. `model_id` is mandatory — the constructor
/// raises if it's empty.
#[pyclass(
    name = "TaskReply",
    module = "newt_agent._newt_agent.acp_worker",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyTaskReply {
    pub inner: TaskReply,
}

#[pymethods]
impl PyTaskReply {
    #[new]
    #[pyo3(signature = (model_id, content, diff, diff_applied, emission_shape = None))]
    fn new(
        model_id: String,
        content: String,
        diff: String,
        diff_applied: bool,
        emission_shape: Option<String>,
    ) -> PyResult<Self> {
        let mut reply = TaskReply::new(model_id, content, diff, diff_applied)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        if let Some(shape) = emission_shape {
            reply = reply.with_emission_shape(shape);
        }
        Ok(Self { inner: reply })
    }

    #[getter]
    fn model_id(&self) -> &str {
        &self.inner.model_id
    }

    #[getter]
    fn content(&self) -> &str {
        &self.inner.content
    }

    #[getter]
    fn diff(&self) -> &str {
        &self.inner.diff
    }

    #[getter]
    fn empty_diff(&self) -> bool {
        self.inner.empty_diff
    }

    #[getter]
    fn diff_applied(&self) -> bool {
        self.inner.diff_applied
    }

    #[getter]
    fn emission_shape(&self) -> Option<String> {
        self.inner.emission_shape.clone()
    }

    /// Serialize to a JSON string (the ACP wire format).
    fn to_json(&self) -> PyResult<String> {
        serde_json::to_string(&self.inner)
            .map_err(|e| PyRuntimeError::new_err(format!("encode: {e}")))
    }

    /// Parse a JSON string in ACP wire format back into a `TaskReply`.
    #[classmethod]
    fn from_json(_cls: &Bound<'_, PyType>, s: &str) -> PyResult<Self> {
        let inner: TaskReply =
            serde_json::from_str(s).map_err(|e| PyRuntimeError::new_err(format!("decode: {e}")))?;
        Ok(Self { inner })
    }

    fn __repr__(&self) -> String {
        format!(
            "TaskReply(model_id='{}', empty_diff={}, diff_applied={}, emission_shape={:?})",
            self.inner.model_id,
            self.inner.empty_diff,
            self.inner.diff_applied,
            self.inner.emission_shape,
        )
    }
}

// ---- module-level helpers ----

/// True if `diff` is empty per the worker's definition.
#[pyfunction]
#[pyo3(name = "is_empty_diff")]
fn py_is_empty_diff(diff: &str) -> bool {
    is_empty_diff(diff)
}

/// Capture `git diff --no-color` at `workspace`. Mirrors what the ACP
/// worker does post-turn.
#[pyfunction]
#[pyo3(name = "capture_diff")]
fn py_capture_diff(workspace: PathBuf) -> PyResult<String> {
    capture_diff(&workspace).map_err(|e| PyRuntimeError::new_err(format!("capture_diff: {e}")))
}

/// Register the `acp_worker` submodule on the parent `_newt_agent` module.
pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(py, "acp_worker")?;
    m.add_class::<PySession>()?;
    m.add_class::<PyTaskReply>()?;
    m.add_function(wrap_pyfunction!(py_is_empty_diff, &m)?)?;
    m.add_function(wrap_pyfunction!(py_capture_diff, &m)?)?;
    parent.add_submodule(&m)?;
    Ok(())
}
