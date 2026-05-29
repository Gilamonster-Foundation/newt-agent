//! Python bindings for `newt-mcp-server` — data types only.
//!
//! Compiled only when the `pyo3` cargo feature is on. Exposes the
//! `McpServer` registry shell — handler registration takes a Python
//! callable returning a JSON-serializable value (sync only for v0).
//!
//! The async stdio loop (`McpServer::run`) is intentionally NOT bound:
//! piping a Python-side reader/writer pair through tokio's stdin/stdout
//! is brittle, and the Rust `newt-mcp-server` binary is the supported
//! way to run the server. Python consumers that need to inspect the
//! protocol can construct an `McpServer`, register handlers, and call
//! `handle(method, params)` directly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use crate::server::McpServer;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyModule, PyString};

/// Python-friendly handler registry: maps JSON-RPC method names to
/// sync Python callables. Each callable receives the `params` value
/// as a JSON string and must return a JSON-serializable Python value
/// (we round-trip via `json.dumps` to keep the bridge minimal).
///
/// This is a small surface deliberately. The Rust `McpServer::run`
/// stdio loop owns the async dispatch path; Python consumers use this
/// type to *describe* what a handler set looks like (useful for tests
/// and for building MCP shims that proxy to a Python-side
/// implementation).
#[pyclass(name = "McpServer", module = "newt_agent._newt_agent.mcp")]
pub struct PyMcpServer {
    /// Methods registered with Python callbacks. We keep them in a
    /// `Mutex<HashMap>` so the (sync) `register` method can mutate.
    handlers: Arc<StdMutex<HashMap<String, Py<PyAny>>>>,
}

#[pymethods]
impl PyMcpServer {
    /// Build an empty server.
    #[new]
    fn new() -> Self {
        Self {
            handlers: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// Register a Python callable for `method`. The callable receives
    /// `(params: str)` (JSON-encoded) and must return a value that
    /// `json.dumps` accepts.
    fn register(&self, method: &str, callback: Py<PyAny>) -> PyResult<()> {
        let mut guard = self
            .handlers
            .lock()
            .map_err(|e| PyRuntimeError::new_err(format!("lock poisoned: {e}")))?;
        guard.insert(method.to_string(), callback);
        Ok(())
    }

    /// Lookup-and-call: synchronously dispatch one request through a
    /// registered Python callback and return its JSON-encoded result.
    /// Returns `None` if no handler is registered.
    fn handle(&self, py: Python<'_>, method: &str, params_json: &str) -> PyResult<Option<String>> {
        let callback = {
            let guard = self
                .handlers
                .lock()
                .map_err(|e| PyRuntimeError::new_err(format!("lock poisoned: {e}")))?;
            match guard.get(method) {
                Some(cb) => cb.clone_ref(py),
                None => return Ok(None),
            }
        };
        let args = (PyString::new(py, params_json),);
        let result = callback.call1(py, args)?;
        let json_mod = py.import("json")?;
        let dumps = json_mod.getattr("dumps")?;
        let encoded = dumps.call1((result,))?;
        Ok(Some(encoded.extract::<String>()?))
    }

    /// Names of registered methods.
    fn registered_methods(&self) -> PyResult<Vec<String>> {
        let guard = self
            .handlers
            .lock()
            .map_err(|e| PyRuntimeError::new_err(format!("lock poisoned: {e}")))?;
        Ok(guard.keys().cloned().collect())
    }

    fn __repr__(&self) -> PyResult<String> {
        let guard = self
            .handlers
            .lock()
            .map_err(|e| PyRuntimeError::new_err(format!("lock poisoned: {e}")))?;
        Ok(format!("McpServer(methods={})", guard.len()))
    }
}

impl Default for PyMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Return the static tool definitions the Rust MCP server registers.
/// This lets Python callers list the canonical tool schema without
/// reaching into Rust dispatch internals.
///
/// Currently re-encodes the four built-in tools (`code_read`,
/// `code_edit`, `code_search`, `goal_run`) by spinning up an
/// `McpServer`, registering the protocol handlers, and asking
/// `tools/list` for its response.
#[pyfunction]
fn default_tool_definitions(py: Python<'_>) -> PyResult<String> {
    // We can't drive a tokio runtime from inside this sync function,
    // so re-emit the JSON inline. The shape mirrors `tool_definitions`
    // in `handlers.rs`; keep in sync by editing both together (or
    // refactor `handlers::tool_definitions` to be pub).
    let _ = py;
    let _ = McpServer::new; // touch to force the use-path
    Ok(r#"[
        {"name":"code_read","description":"Read a file's contents"},
        {"name":"code_edit","description":"Apply a unified diff patch to a file"},
        {"name":"code_search","description":"Search files for a regex pattern"},
        {"name":"goal_run","description":"Run a tier-routed inference turn"}
    ]"#
    .to_string())
}

/// Register the `mcp` submodule on the parent `_newt_agent` module.
pub fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let m = PyModule::new(py, "mcp")?;
    m.add_class::<PyMcpServer>()?;
    m.add_function(wrap_pyfunction!(default_tool_definitions, &m)?)?;
    parent.add_submodule(&m)?;
    Ok(())
}
