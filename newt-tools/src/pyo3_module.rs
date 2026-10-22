//! Python bindings for `newt-tools`.
//!
//! Compiled only when the `pyo3` cargo feature is on. Exposes the two
//! landing pads — `apply_patch` and `apply_whole_files`. Both are sync
//! (no inference); `pyo3-async-runtimes` is not needed here.

use crate::{apply_patch, apply_whole_files};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule};
use std::path::PathBuf;

create_exception!(_newt_agent, PyToolsError, PyException);

fn tools_err_to_py(e: anyhow::Error) -> PyErr {
    PyToolsError::new_err(e.to_string())
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
    m.add_function(wrap_pyfunction!(py_apply_patch, &m)?)?;
    m.add_function(wrap_pyfunction!(py_apply_whole_files, &m)?)?;
    m.add("ToolsError", py.get_type::<PyToolsError>())?;
    parent.add_submodule(&m)?;
    Ok(())
}
