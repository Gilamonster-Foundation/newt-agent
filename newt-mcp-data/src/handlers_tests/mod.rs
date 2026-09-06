use super::*;

/// Build a fully-wired McpServer over a fresh in-memory store and send a
/// single request through it via in-memory byte buffers. Hermetic: the
/// store is `open_in_memory`, never a real `~/.newt-data` path.
async fn rpc(request: &Value) -> Value {
    rpc_with(Arc::new(SqliteBackend::open_in_memory().unwrap()), request).await
}

/// Like [`rpc`], but with a caller-supplied store so a multi-step flow
/// (ingest → query → summarize → list) can share one database. The kernel
/// session starts empty (no kernel attached).
async fn rpc_with(store: Arc<SqliteBackend>, request: &Value) -> Value {
    rpc_full(store, new_kernel_session(), request).await
}

/// The fully-parameterized harness: a caller-supplied store **and** kernel
/// session, so the live-kernel tests can pre-attach a [`MockKernel`] (or
/// assert the empty-session error) and the SQL tests can share a database.
async fn rpc_full(store: Arc<SqliteBackend>, session: KernelSession, request: &Value) -> Value {
    // A throwaway plots dir; the MockKernel never writes there (it returns a
    // canned CellRun), so the path only needs to be syntactically valid.
    let plots_dir = std::env::temp_dir().join("newt-mcp-data-test-plots");
    let mut server = McpServer::new();
    register_handlers(&mut server, store, session, plots_dir);

    let input = format!("{}\n", serde_json::to_string(request).unwrap());
    let mut output: Vec<u8> = Vec::new();
    server.run(input.as_bytes(), &mut output).await.unwrap();
    let text = String::from_utf8(output).unwrap();
    serde_json::from_str(text.trim()).unwrap()
}

/// A canned [`KernelClient`] for the tool-logic tests: it returns a fixed
/// [`CellRun`] (or a fixed transport error) without any websocket — so the
/// `run_cell` / `kernel_attach` MCP envelope logic (PNG path reporting, the
/// no-kernel error, the in-band error discipline) is exercised hermetically,
/// no live Jupyter kernel required.
///
/// `seen_code` records the code of the last `run_cell` so a test can assert a
/// tool DID (or did NOT) reach the kernel — load-bearing for the
/// dataframe-introspection injection guard, which must reject a hostile name
/// *before* any kernel call (Phase 21.5).
struct MockKernel {
    run: newt_data::kernel::CellRun,
    fail: Option<String>,
    seen_code: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl KernelClient for MockKernel {
    async fn run_cell(&self, code: &str) -> anyhow::Result<newt_data::kernel::CellRun> {
        self.seen_code.lock().unwrap().push(code.to_string());
        match &self.fail {
            Some(msg) => anyhow::bail!("{msg}"),
            None => Ok(self.run.clone()),
        }
    }
}

/// A session with a pre-attached [`MockKernel`] returning `run`.
fn session_with(run: newt_data::kernel::CellRun) -> KernelSession {
    Arc::new(Mutex::new(Some(Box::new(MockKernel {
        run,
        fail: None,
        seen_code: Arc::new(std::sync::Mutex::new(Vec::new())),
    }) as Box<dyn KernelClient>)))
}

/// A session with a pre-attached [`MockKernel`] whose `run_cell` errors.
fn session_failing(msg: &str) -> KernelSession {
    Arc::new(Mutex::new(Some(Box::new(MockKernel {
        run: newt_data::kernel::CellRun::default(),
        fail: Some(msg.to_string()),
        seen_code: Arc::new(std::sync::Mutex::new(Vec::new())),
    }) as Box<dyn KernelClient>)))
}

/// A session pre-attached to a [`MockKernel`] that returns `stdout` verbatim
/// (no error), plus the shared `seen_code` log so a test can assert what code
/// — if any — actually reached the kernel. Used by the Phase 21.5
/// dataframe-introspection tests: program the canned JSON the snippet would
/// print, then assert the tool parses it (and, for the injection guard, that
/// the kernel was never invoked at all).
fn session_with_stdout(stdout: &str) -> (KernelSession, Arc<std::sync::Mutex<Vec<String>>>) {
    let seen_code = Arc::new(std::sync::Mutex::new(Vec::new()));
    let run = newt_data::kernel::CellRun {
        stdout: stdout.to_string(),
        ..Default::default()
    };
    let session = Arc::new(Mutex::new(Some(Box::new(MockKernel {
        run,
        fail: None,
        seen_code: seen_code.clone(),
    }) as Box<dyn KernelClient>)));
    (session, seen_code)
}

/// Helper: a `tools/call` request body.
fn call(id: i64, name: &str, arguments: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    })
}

// ── initialize ──────────────────────────────────────────────────────────

// ── tools/list ──────────────────────────────────────────────────────────

// ── happy-path flow: ingest → query → summarize → list ──────────────────

// ── error cases: all in-band (isError), resp["error"] null ──────────────

// ── row_cap parsing — unit tests ────────────────────────────────────────

// ── live-kernel tools (21.3) — driven by a MockKernel ────────────────────

use newt_data::kernel::{CellRun, DisplayItem, ImageOutput, KernelError};

/// A canned CellRun with stdout, a text result, a PNG image, and an
/// execution_count — the happy path `run_cell` must summarize.
fn canned_run() -> CellRun {
    CellRun {
        stdout: "hello from kernel\n".into(),
        stderr: String::new(),
        results: vec![DisplayItem {
            mime: "text/plain".into(),
            text: "42".into(),
        }],
        images: vec![ImageOutput {
            path: std::path::PathBuf::from("/ws/.newt-data/plots/cell-5-abc.png"),
            mime: "image/png".into(),
            width: Some(640),
            height: Some(480),
        }],
        error: None,
        execution_count: Some(5),
    }
}

// ── dataframe introspection (Phase 21.5) — driven by a MockKernel ────────
//
// No live kernel: the MockKernel returns the JSON the snippet *would* print,
// so the tool's parse + in-band-error logic is exercised hermetically.

// ── notebook tools (Phase 21.4) — over a tempfile .ipynb ─────────────────

// Families beside this file. Both attributes are required: rustc needs only
// the `#[path]`, but the ratchets' shared scanner resolves a child ONLY when
// a `#[cfg(test)]` immediately precedes the `mod` (#2149).
#[cfg(test)]
#[path = "arg_parsing.rs"]
mod arg_parsing;
#[cfg(test)]
#[path = "dataframes.rs"]
mod dataframes;
#[cfg(test)]
#[path = "kernel.rs"]
mod kernel;
#[cfg(test)]
#[path = "notebook.rs"]
mod notebook;
#[cfg(test)]
#[path = "protocol.rs"]
mod protocol;
#[cfg(test)]
#[path = "run_cell_persist.rs"]
mod run_cell_persist;
#[cfg(test)]
#[path = "sql_tools.rs"]
mod sql_tools;
