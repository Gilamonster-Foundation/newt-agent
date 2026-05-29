//! Python bindings for `newt-tools`.
//!
//! Compiled only when the `pyo3` cargo feature is on. Exposes the four
//! tool primitives — `read`, `edit`, `search`, `apply_patch`,
//! `apply_whole_files` — plus the `Hit` result row. All surfaces are
//! sync (no inference); `pyo3-async-runtimes` is not needed here.

use crate::{apply_patch, apply_whole_files, edit, read, search, Hit};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule};
use std::path::PathBuf;

create_exception!(_newt_agent, PyToolsError, PyException);

fn tools_err_to_py(e: anyhow::Error) -> PyErr {
    PyToolsError::new_err(e.to_string())
}

// ---- Hit ----

/// One search result row.
#[pyclass(
    name = "Hit",
    module = "newt_agent._newt_agent.tools",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyHit {
    pub inner: Hit,
}

#[pymethods]
impl PyHit {
    #[getter]
    fn path(&self) -> &str {
        &self.inner.path
    }

    #[getter]
    fn line_number(&self) -> usize {
        self.inner.line_number
    }

    #[getter]
    fn line(&self) -> &str {
        &self.inner.line
    }

    fn __repr__(&self) -> String {
        format!(
            "Hit(path='{}', line_number={}, line='{}')",
            self.inner.path,
            self.inner.line_number,
            self.inner.line.escape_default(),
        )
    }
}

// ---- read ----

/// Read a UTF-8 file from disk with size + encoding validation.
#[pyfunction]
#[pyo3(name = "read")]
fn py_read(path: PathBuf) -> PyResult<String> {
    read(&path).map_err(tools_err_to_py)
}

// ---- edit ----

/// Apply a single-file unified diff to `path`.
#[pyfunction]
#[pyo3(name = "edit")]
fn py_edit(path: PathBuf, patch: &str) -> PyResult<()> {
    edit(&path, patch).map_err(tools_err_to_py)
}

// ---- search ----

/// Regex-search every text file under `root`. Returns the list of
/// hits (capped at the internal `MAX_HITS` limit).
#[pyfunction]
#[pyo3(name = "search")]
fn py_search(pattern: &str, root: PathBuf) -> PyResult<Vec<PyHit>> {
    let hits = search(pattern, &root).map_err(tools_err_to_py)?;
    Ok(hits.into_iter().map(|inner| PyHit { inner }).collect())
}

// ---- apply_patch ----

/// Apply a multi-file unified diff under `root`.
#[pyfunction]
#[pyo3(name = "apply_patch")]
fn py_apply_patch(diff: &str, root: PathBuf) -> PyResult<()> {
    apply_patch(&root, diff).map_err(tools_err_to_py)
}

// ---- apply_whole_files ----

/// Atomically write a set of `(relative_path -> contents)` pairs into
/// `workspace`. Returns the list of relative paths written.
#[pyfunction]
#[pyo3(name = "apply_whole_files")]
fn py_apply_whole_files(workspace: PathBuf, files: &Bound<'_, PyDict>) -> PyResult<Vec<String>> {
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(files.len());
    for (k, v) in files.iter() {
        let key: String = k.extract()?;
        let val: String = v.extract()?;
        pairs.push((key, val));
    }
    apply_whole_files(&workspace, pairs).map_err(tools_err_to_py)
}

/// Register the `tools` submodule on the parent `_newt_agent` module.
pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(py, "tools")?;
    m.add_class::<PyHit>()?;
    m.add_function(wrap_pyfunction!(py_read, &m)?)?;
    m.add_function(wrap_pyfunction!(py_edit, &m)?)?;
    m.add_function(wrap_pyfunction!(py_search, &m)?)?;
    m.add_function(wrap_pyfunction!(py_apply_patch, &m)?)?;
    m.add_function(wrap_pyfunction!(py_apply_whole_files, &m)?)?;
    m.add("ToolsError", py.get_type::<PyToolsError>())?;
    parent.add_submodule(&m)?;
    Ok(())
}
