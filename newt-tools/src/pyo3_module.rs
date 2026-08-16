//! Python bindings for `newt-tools`.
//!
//! Compiled only when the `pyo3` cargo feature is on. Exposes the four
//! tool primitives — `read`, `edit`, `search`, `apply_patch`,
//! `apply_whole_files` — plus the `Hit` result row. The Jupyter notebook
//! execution surface is gated additionally on `feature = "jupyter"`, mirroring
//! how `newt-tools` itself exposes that module; the umbrella `newt-agent-py`
//! crate turns that feature on when building the Python wheel so the bindings
//! and the underlying Rust module stay wired together (no orphan registrations
//! in pyo3-only wheels, no orphan code in jupyter-only builds).
//!
//! All surfaces are sync (no inference); `pyo3-async-runtimes` is not needed here.

#[cfg(feature = "jupyter")]
use crate::jupyter::{
    execute_notebook, get_server_status, start_server, stop_server, CellOutputSummary,
    JupyterExecuteParams, JupyterExecuteResult, JupyterServerParams, JupyterServerResult,
    JupyterServerStatus, KernelInfo,
};
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

// ---- JupyterExecuteParams ----

/// Parameters for executing a Jupyter notebook.
#[cfg(feature = "jupyter")]
#[pyclass(
    name = "JupyterExecuteParams",
    module = "newt_agent._newt_agent.tools",
    frozen,
    from_py_object
)]
#[derive(Clone)]
pub struct PyJupyterExecuteParams {
    pub inner: JupyterExecuteParams,
}

#[cfg(feature = "jupyter")]
#[pymethods]
impl PyJupyterExecuteParams {
    #[new]
    #[pyo3(signature = (notebook_path, working_dir=None, timeout_seconds=None, save_outputs=None, kernel_name=None))]
    fn new(
        notebook_path: String,
        working_dir: Option<String>,
        timeout_seconds: Option<u64>,
        save_outputs: Option<bool>,
        kernel_name: Option<String>,
    ) -> Self {
        Self {
            inner: JupyterExecuteParams {
                notebook_path,
                working_dir,
                timeout_seconds,
                save_outputs,
                kernel_name,
            },
        }
    }

    #[getter]
    fn notebook_path(&self) -> &str {
        &self.inner.notebook_path
    }

    #[getter]
    fn working_dir(&self) -> Option<&str> {
        self.inner.working_dir.as_deref()
    }

    #[getter]
    fn timeout_seconds(&self) -> Option<u64> {
        self.inner.timeout_seconds
    }

    #[getter]
    fn save_outputs(&self) -> Option<bool> {
        self.inner.save_outputs
    }

    #[getter]
    fn kernel_name(&self) -> Option<&str> {
        self.inner.kernel_name.as_deref()
    }
}

// ---- CellOutputSummary ----

/// Summary of a single cell's execution output.
#[cfg(feature = "jupyter")]
#[pyclass(
    name = "CellOutputSummary",
    module = "newt_agent._newt_agent.tools",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyCellOutputSummary {
    pub inner: CellOutputSummary,
}

#[cfg(feature = "jupyter")]
#[pymethods]
impl PyCellOutputSummary {
    #[getter]
    fn cell_index(&self) -> usize {
        self.inner.cell_index
    }

    #[getter]
    fn cell_type(&self) -> &str {
        &self.inner.cell_type
    }

    #[getter]
    fn success(&self) -> bool {
        self.inner.success
    }

    #[getter]
    fn output_count(&self) -> usize {
        self.inner.output_count
    }

    #[getter]
    fn error(&self) -> Option<&str> {
        self.inner.error.as_deref()
    }
}

// ---- JupyterExecuteResult ----

/// Result of executing a Jupyter notebook.
#[cfg(feature = "jupyter")]
#[pyclass(
    name = "JupyterExecuteResult",
    module = "newt_agent._newt_agent.tools",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyJupyterExecuteResult {
    pub inner: JupyterExecuteResult,
}

#[cfg(feature = "jupyter")]
#[pymethods]
impl PyJupyterExecuteResult {
    #[getter]
    fn success(&self) -> bool {
        self.inner.success
    }

    #[getter]
    fn notebook_path(&self) -> &str {
        &self.inner.notebook_path
    }

    #[getter]
    fn cells_executed(&self) -> usize {
        self.inner.cells_executed
    }

    #[getter]
    fn cells_failed(&self) -> usize {
        self.inner.cells_failed
    }

    #[getter]
    fn execution_time_seconds(&self) -> f64 {
        self.inner.execution_time_seconds
    }

    #[getter]
    fn error(&self) -> Option<&str> {
        self.inner.error.as_deref()
    }

    #[getter]
    fn cell_outputs(&self) -> Vec<PyCellOutputSummary> {
        self.inner
            .cell_outputs
            .iter()
            .map(|c| PyCellOutputSummary { inner: c.clone() })
            .collect()
    }
}

// ---- execute_notebook ----

/// Execute a Jupyter notebook using nbconvert.
#[cfg(feature = "jupyter")]
#[pyfunction]
#[pyo3(name = "execute_notebook")]
fn py_execute_notebook(params: PyJupyterExecuteParams) -> PyResult<PyJupyterExecuteResult> {
    let result = execute_notebook(params.inner).map_err(tools_err_to_py)?;
    Ok(PyJupyterExecuteResult { inner: result })
}

// ---- JupyterServerParams ----

/// Parameters for starting a Jupyter server.
#[cfg(feature = "jupyter")]
#[pyclass(
    name = "JupyterServerParams",
    module = "newt_agent._newt_agent.tools",
    frozen,
    from_py_object
)]
#[derive(Clone)]
pub struct PyJupyterServerParams {
    pub inner: JupyterServerParams,
}

#[cfg(feature = "jupyter")]
#[pymethods]
impl PyJupyterServerParams {
    #[new]
    #[pyo3(signature = (working_dir=None, port=None, host=None, token=None, password=None, open_browser=None, extra_args=None))]
    fn new(
        working_dir: Option<String>,
        port: Option<u16>,
        host: Option<String>,
        token: Option<String>,
        password: Option<String>,
        open_browser: Option<bool>,
        extra_args: Option<Vec<String>>,
    ) -> Self {
        Self {
            inner: JupyterServerParams {
                working_dir,
                port,
                host,
                token,
                password,
                open_browser,
                extra_args,
            },
        }
    }

    #[getter]
    fn working_dir(&self) -> Option<&str> {
        self.inner.working_dir.as_deref()
    }

    #[getter]
    fn port(&self) -> Option<u16> {
        self.inner.port
    }

    #[getter]
    fn host(&self) -> Option<&str> {
        self.inner.host.as_deref()
    }

    #[getter]
    fn token(&self) -> Option<&str> {
        self.inner.token.as_deref()
    }

    #[getter]
    fn password(&self) -> Option<&str> {
        self.inner.password.as_deref()
    }

    #[getter]
    fn open_browser(&self) -> Option<bool> {
        self.inner.open_browser
    }

    #[getter]
    fn extra_args(&self) -> Option<Vec<String>> {
        self.inner.extra_args.clone()
    }
}

// ---- KernelInfo ----

/// Information about a running Jupyter kernel.
#[cfg(feature = "jupyter")]
#[pyclass(
    name = "KernelInfo",
    module = "newt_agent._newt_agent.tools",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyKernelInfo {
    pub inner: KernelInfo,
}

#[cfg(feature = "jupyter")]
#[pymethods]
impl PyKernelInfo {
    #[getter]
    fn id(&self) -> &str {
        &self.inner.id
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[getter]
    fn last_activity(&self) -> &str {
        &self.inner.last_activity
    }

    #[getter]
    fn execution_state(&self) -> &str {
        &self.inner.execution_state
    }

    #[getter]
    fn connections(&self) -> usize {
        self.inner.connections
    }
}

// ---- JupyterServerStatus ----

/// Status of a Jupyter server.
#[cfg(feature = "jupyter")]
#[pyclass(
    name = "JupyterServerStatus",
    module = "newt_agent._newt_agent.tools",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyJupyterServerStatus {
    pub inner: JupyterServerStatus,
}

#[cfg(feature = "jupyter")]
#[pymethods]
impl PyJupyterServerStatus {
    #[getter]
    fn running(&self) -> bool {
        self.inner.running
    }

    #[getter]
    fn url(&self) -> Option<&str> {
        self.inner.url.as_deref()
    }

    #[getter]
    fn pid(&self) -> Option<u32> {
        self.inner.pid
    }

    #[getter]
    fn port(&self) -> Option<u16> {
        self.inner.port
    }

    #[getter]
    fn kernels(&self) -> Vec<PyKernelInfo> {
        self.inner
            .kernels
            .iter()
            .map(|k| PyKernelInfo { inner: k.clone() })
            .collect()
    }
}

// ---- JupyterServerResult ----

/// Result of starting a Jupyter server.
#[cfg(feature = "jupyter")]
#[pyclass(
    name = "JupyterServerResult",
    module = "newt_agent._newt_agent.tools",
    frozen,
    skip_from_py_object
)]
#[derive(Clone)]
pub struct PyJupyterServerResult {
    pub inner: JupyterServerResult,
}

#[cfg(feature = "jupyter")]
#[pymethods]
impl PyJupyterServerResult {
    #[getter]
    fn success(&self) -> bool {
        self.inner.success
    }

    #[getter]
    fn url(&self) -> Option<&str> {
        self.inner.url.as_deref()
    }

    #[getter]
    fn pid(&self) -> Option<u32> {
        self.inner.pid
    }

    #[getter]
    fn port(&self) -> Option<u16> {
        self.inner.port
    }

    #[getter]
    fn token(&self) -> Option<&str> {
        self.inner.token.as_deref()
    }

    #[getter]
    fn error(&self) -> Option<&str> {
        self.inner.error.as_deref()
    }
}

// ---- start_server ----

/// Start a Jupyter notebook server.
#[cfg(feature = "jupyter")]
#[pyfunction]
#[pyo3(name = "start_jupyter_server")]
fn py_start_server(params: PyJupyterServerParams) -> PyResult<PyJupyterServerResult> {
    let result = start_server(params.inner).map_err(tools_err_to_py)?;
    Ok(PyJupyterServerResult { inner: result })
}

// ---- stop_server ----

/// Stop a Jupyter server by PID.
#[cfg(feature = "jupyter")]
#[pyfunction]
#[pyo3(name = "stop_jupyter_server")]
fn py_stop_server(pid: u32) -> PyResult<bool> {
    stop_server(pid).map_err(tools_err_to_py)
}

// ---- get_server_status ----

/// Get status of a Jupyter server.
#[cfg(feature = "jupyter")]
#[pyfunction]
#[pyo3(name = "get_jupyter_server_status")]
fn py_get_server_status(url: String, token: Option<String>) -> PyResult<PyJupyterServerStatus> {
    let result = get_server_status(&url, token.as_deref()).map_err(tools_err_to_py)?;
    Ok(PyJupyterServerStatus { inner: result })
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
    #[cfg(feature = "jupyter")]
    {
        m.add_class::<PyJupyterExecuteParams>()?;
        m.add_class::<PyCellOutputSummary>()?;
        m.add_class::<PyJupyterExecuteResult>()?;
        m.add_class::<PyJupyterServerParams>()?;
        m.add_class::<PyKernelInfo>()?;
        m.add_class::<PyJupyterServerStatus>()?;
        m.add_class::<PyJupyterServerResult>()?;
        m.add_function(wrap_pyfunction!(py_execute_notebook, &m)?)?;
        m.add_function(wrap_pyfunction!(py_start_server, &m)?)?;
        m.add_function(wrap_pyfunction!(py_stop_server, &m)?)?;
        m.add_function(wrap_pyfunction!(py_get_server_status, &m)?)?;
    }
    m.add("ToolsError", py.get_type::<PyToolsError>())?;
    parent.add_submodule(&m)?;
    Ok(())
}
