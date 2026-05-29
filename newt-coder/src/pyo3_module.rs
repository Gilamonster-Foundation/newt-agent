//! Python bindings for `newt-coder` — sync surfaces only.
//!
//! Compiled only when the `pyo3` cargo feature is on. Exposes:
//! - `Emission` enum (`whole_files` / `unified_diff` / `prose`)
//! - `normalize_emission(raw) -> Emission`
//! - `build_prompt(workspace, task) -> CoderPrompt`
//! - `scan_workspace_for_files(workspace, task) -> list[str]`
//! - `CoderError` Python exception.
//!
//! The async `Coder::run` is intentionally NOT bound — it requires an
//! `InferenceBackend` trait object, which Python can't construct
//! without a bridge from PyAny → Rust impl. The sync helpers above
//! cover the high-value surface (prompt construction + emission
//! parsing) without that complexity.

use std::collections::BTreeMap;

use crate::{
    build_prompt, normalize_emission, scan_workspace_for_files, CoderError, CoderPrompt, Emission,
    DEFAULT_CONTEXT_CAP_CHARS, WHOLE_FILE_SYSTEM_PROMPT,
};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule};
use std::path::PathBuf;

create_exception!(_newt_agent, PyCoderError, PyException);

fn coder_err_to_py(e: CoderError) -> PyErr {
    PyCoderError::new_err(e.to_string())
}

// ---- Emission ----

/// Classified emission shape from `normalize_emission`. Wraps the
/// Rust `Emission` enum.
///
/// Use the `kind` getter to distinguish; payloads are exposed via
/// `as_whole_files() -> dict[str, str] | None`, `as_unified_diff()
/// -> str | None`, `as_prose() -> str | None`.
#[pyclass(
    name = "Emission",
    module = "newt_agent._newt_agent.coder",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyEmission {
    pub inner: Emission,
}

#[pymethods]
impl PyEmission {
    /// One of `"whole_files"`, `"unified_diff"`, `"prose"`.
    #[getter]
    fn kind(&self) -> &'static str {
        self.inner.shape_label()
    }

    /// True iff this is `Emission::WholeFiles`.
    fn is_whole_files(&self) -> bool {
        matches!(self.inner, Emission::WholeFiles(_))
    }

    /// True iff this is `Emission::UnifiedDiff`.
    fn is_unified_diff(&self) -> bool {
        matches!(self.inner, Emission::UnifiedDiff(_))
    }

    /// True iff this is `Emission::Prose`.
    fn is_prose(&self) -> bool {
        matches!(self.inner, Emission::Prose(_))
    }

    /// Return the `{relative_path: contents}` map for a whole-files
    /// emission, or `None` for the other shapes.
    fn as_whole_files<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        match &self.inner {
            Emission::WholeFiles(map) => {
                let d = PyDict::new(py);
                for (k, v) in map {
                    d.set_item(k, v)?;
                }
                Ok(Some(d))
            }
            _ => Ok(None),
        }
    }

    /// Return the diff text for a unified-diff emission, or `None`.
    fn as_unified_diff(&self) -> Option<String> {
        if let Emission::UnifiedDiff(d) = &self.inner {
            Some(d.clone())
        } else {
            None
        }
    }

    /// Return the prose text for a prose emission, or `None`.
    fn as_prose(&self) -> Option<String> {
        if let Emission::Prose(p) = &self.inner {
            Some(p.clone())
        } else {
            None
        }
    }

    fn __repr__(&self) -> String {
        match &self.inner {
            Emission::WholeFiles(m) => format!("Emission.whole_files({} files)", m.len()),
            Emission::UnifiedDiff(d) => format!("Emission.unified_diff({} chars)", d.len()),
            Emission::Prose(p) => format!("Emission.prose({} chars)", p.len()),
        }
    }
}

// ---- CoderPrompt ----

/// Built prompt: `system` + `user` strings ready for inference, plus
/// the list of files actually injected.
#[pyclass(
    name = "CoderPrompt",
    module = "newt_agent._newt_agent.coder",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyCoderPrompt {
    pub inner: CoderPrompt,
}

#[pymethods]
impl PyCoderPrompt {
    #[getter]
    fn system(&self) -> &str {
        &self.inner.system
    }

    #[getter]
    fn user(&self) -> &str {
        &self.inner.user
    }

    #[getter]
    fn included_files(&self) -> Vec<String> {
        self.inner
            .included_files
            .iter()
            .map(|p| p.display().to_string())
            .collect()
    }

    fn __repr__(&self) -> String {
        format!(
            "CoderPrompt(system={} chars, user={} chars, included={})",
            self.inner.system.len(),
            self.inner.user.len(),
            self.inner.included_files.len(),
        )
    }
}

// ---- Module-level functions ----

/// Parse a raw model reply into an [`Emission`].
#[pyfunction]
#[pyo3(name = "normalize_emission")]
fn py_normalize_emission(raw: &str) -> PyResult<PyEmission> {
    let inner = normalize_emission(raw).map_err(coder_err_to_py)?;
    Ok(PyEmission { inner })
}

/// Build a coder prompt for `task` against `workspace`. Scans for
/// relevant files and injects them under `FILE:`/`END-FILE` markers.
#[pyfunction]
#[pyo3(name = "build_prompt")]
fn py_build_prompt(workspace: PathBuf, task: &str) -> PyResult<PyCoderPrompt> {
    let inner = build_prompt(&workspace, task).map_err(coder_err_to_py)?;
    Ok(PyCoderPrompt { inner })
}

/// Scan `workspace` for files relevant to `task`. Returns relative
/// path strings.
#[pyfunction]
#[pyo3(name = "scan_workspace_for_files")]
fn py_scan_workspace_for_files(workspace: PathBuf, task: &str) -> PyResult<Vec<String>> {
    let files = scan_workspace_for_files(&workspace, task).map_err(coder_err_to_py)?;
    Ok(files.into_iter().map(|p| p.display().to_string()).collect())
}

// ---- Convenience constructor for testing whole-files emissions ----

/// Build an `Emission::WholeFiles` from a `{path: contents}` dict.
/// Useful for tests that want to assert apply behavior.
#[pyfunction]
#[pyo3(name = "whole_files_emission")]
fn py_whole_files_emission(files: &Bound<'_, PyDict>) -> PyResult<PyEmission> {
    let mut map = BTreeMap::new();
    for (k, v) in files.iter() {
        let key: String = k.extract()?;
        let val: String = v.extract()?;
        map.insert(key, val);
    }
    Ok(PyEmission {
        inner: Emission::WholeFiles(map),
    })
}

/// Register the `coder` submodule on the parent `_newt_agent` module.
pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(py, "coder")?;
    m.add_class::<PyEmission>()?;
    m.add_class::<PyCoderPrompt>()?;
    m.add_function(wrap_pyfunction!(py_normalize_emission, &m)?)?;
    m.add_function(wrap_pyfunction!(py_build_prompt, &m)?)?;
    m.add_function(wrap_pyfunction!(py_scan_workspace_for_files, &m)?)?;
    m.add_function(wrap_pyfunction!(py_whole_files_emission, &m)?)?;
    m.add("CoderError", py.get_type::<PyCoderError>())?;
    m.add("DEFAULT_CONTEXT_CAP_CHARS", DEFAULT_CONTEXT_CAP_CHARS)?;
    m.add("WHOLE_FILE_SYSTEM_PROMPT", WHOLE_FILE_SYSTEM_PROMPT)?;
    parent.add_submodule(&m)?;
    Ok(())
}
