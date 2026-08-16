//! Jupyter notebook execution + server management tool.
//!
//! All symbols in this module are gated behind `feature = "jupyter"`. The
//! `nbformat` / `reqwest` / `rand` dependencies it uses are likewise only
//! declared `optional = true` in `Cargo.toml`, so a `--no-default-features`
//! build of `newt-tools` stays strictly free of jupyter-only deps and has
//! nothing to link against.
//!
//! ## Server model
//!
//! `start_server` spawns `jupyter notebook` bound to the loopback interface
//! only (never a remote-access flag), scrubs the child environment of newt's
//! whole control plane (`env_clear` + a minimal allowlist), then *probes* the
//! REST API until the server answers instead of sleeping a fixed delay. The
//! spawned `Child` is owned by a process-local registry keyed by an opaque
//! `handle_id`; `stop_server` / `get_server_status` operate by handle, never
//! by bare PID or an arbitrary URL — so a caller cannot point this tool at a
//! server it did not start. `stop_server` kills the owned child directly
//! (`Child::kill`), so no `kill` / `taskkill` subprocess is ever spawned.

#[cfg(feature = "jupyter")]
use std::process::Command;

#[cfg(feature = "jupyter")]
use anyhow::{Context, Result};
#[cfg(feature = "jupyter")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "jupyter")]
use std::collections::HashMap;
#[cfg(feature = "jupyter")]
use std::io::Read;
#[cfg(feature = "jupyter")]
use std::path::{Path, PathBuf};
#[cfg(feature = "jupyter")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "jupyter")]
use std::sync::{LazyLock, Mutex};
#[cfg(feature = "jupyter")]
use std::thread;
#[cfg(feature = "jupyter")]
use std::time::Duration;

/// Environment variables passed through to the jupyter child after
/// `env_clear`. Deliberately EXCLUDES every newt control-plane switch and
/// secret (`NEWT_AGENT_KEY`, operator keys, authority tokens) — the same
/// philosophy as the operator-yolo-optout `CHILD_STRIPPED_AUTHORITY_ENV` scrub
/// (#8): a nested jupyter cannot re-assert newt authority and newt's
/// credentials never leak into the notebook subprocess.
#[cfg(feature = "jupyter")]
const ENV_ALLOWLIST: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "LC_CTYPE", "TERM"];

/// Maximum bytes of captured stderr retained for diagnostics on a failed start.
#[cfg(feature = "jupyter")]
const STDERR_CAP: usize = 8 * 1024;

// ---- notebook execution -------------------------------------------------

#[cfg(feature = "jupyter")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterExecuteParams {
    /// Path to the notebook file (.ipynb)
    pub notebook_path: String,
    /// Optional working directory to execute the notebook in.
    /// If not provided, uses the notebook's parent directory.
    pub working_dir: Option<String>,
    /// Timeout in seconds for the entire notebook execution (default: 300)
    pub timeout_seconds: Option<u64>,
    /// Whether to save the executed notebook with outputs (default: true)
    pub save_outputs: Option<bool>,
    /// Kernel name to use (default: python3)
    pub kernel_name: Option<String>,
}

#[cfg(feature = "jupyter")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterExecuteResult {
    /// Whether execution succeeded
    pub success: bool,
    /// Path to the executed notebook
    pub notebook_path: String,
    /// Number of cells executed
    pub cells_executed: usize,
    /// Number of cells that failed
    pub cells_failed: usize,
    /// Execution time in seconds
    pub execution_time_seconds: f64,
    /// Error message if any
    pub error: Option<String>,
    /// Cell outputs summary
    pub cell_outputs: Vec<CellOutputSummary>,
}

#[cfg(feature = "jupyter")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellOutputSummary {
    pub cell_index: usize,
    pub cell_type: String,
    pub success: bool,
    pub output_count: usize,
    pub error: Option<String>,
}

/// Execute a Jupyter notebook using nbconvert.
#[cfg(feature = "jupyter")]
pub fn execute_notebook(params: JupyterExecuteParams) -> Result<JupyterExecuteResult> {
    let start_time = std::time::Instant::now();

    let working_dir = params
        .working_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    // Resolve notebook path relative to working_dir
    let notebook_path = working_dir.join(&params.notebook_path);
    if !notebook_path.exists() {
        anyhow::bail!("Notebook not found: {}", notebook_path.display());
    }

    let timeout = params.timeout_seconds.unwrap_or(300);
    let save_outputs = params.save_outputs.unwrap_or(true);
    let kernel_name = params.kernel_name.unwrap_or_else(|| "python3".to_string());

    // Build the nbconvert command (env-scrubbed through the shared helper)
    let mut cmd = jupyter_cmd();
    cmd.arg("nbconvert")
        .arg("--execute")
        .arg("--to")
        .arg("notebook")
        .arg("--inplace")
        .arg("--ExecutePreprocessor.kernel_name")
        .arg(&kernel_name)
        .arg("--ExecutePreprocessor.timeout")
        .arg(timeout.to_string())
        .arg(&params.notebook_path) // Use relative path from working_dir
        .current_dir(&working_dir);

    if !save_outputs {
        cmd.arg("--no-output");
    }

    let output = cmd
        .output()
        .context("Failed to execute jupyter nbconvert. Is jupyter installed?")?;

    let execution_time = start_time.elapsed().as_secs_f64();

    let success = output.status.success();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Parse outputs from the executed notebook if successful
    let cell_outputs = if success {
        parse_notebook_outputs(&notebook_path)?
    } else {
        vec![]
    };

    let cells_executed = cell_outputs.len();
    let cells_failed = cell_outputs.iter().filter(|c| !c.success).count();

    Ok(JupyterExecuteResult {
        success,
        notebook_path: params.notebook_path,
        cells_executed,
        cells_failed,
        execution_time_seconds: execution_time,
        error: if success {
            None
        } else {
            Some(format!("stdout: {stdout}\nstderr: {stderr}"))
        },
        cell_outputs,
    })
}

/// Parse cell outputs from an executed notebook.
#[cfg(feature = "jupyter")]
fn parse_notebook_outputs(notebook_path: &Path) -> Result<Vec<CellOutputSummary>> {
    use nbformat::{parse_notebook, v4, Notebook};
    use std::fs;

    let content = fs::read_to_string(notebook_path).context("Failed to read notebook")?;
    let nb = parse_notebook(&content).context("Failed to parse notebook")?;

    let cells = match nb {
        Notebook::V4(nb) => nb.cells,
        Notebook::Legacy(nb) => {
            // Upgrade legacy notebook to v4
            let upgraded = nbformat::upgrade_legacy_notebook(nb)?;
            upgraded.cells
        }
    };

    let mut summaries = Vec::new();

    for (idx, cell) in cells.iter().enumerate() {
        let cell_type = match cell {
            v4::Cell::Code { .. } => "code",
            v4::Cell::Markdown { .. } => "markdown",
            v4::Cell::Raw { .. } => "raw",
        };

        let mut success = true;
        let mut output_count = 0;
        let mut error = None;

        if let v4::Cell::Code { outputs, .. } = cell {
            output_count = outputs.len();
            for output in outputs {
                if let v4::Output::Error(v4::ErrorOutput { ename, evalue, .. }) = output {
                    success = false;
                    error = Some(format!("{ename}: {evalue}"));
                    break;
                }
            }
        }

        summaries.push(CellOutputSummary {
            cell_index: idx,
            cell_type: cell_type.to_string(),
            success,
            output_count,
            error,
        });
    }

    Ok(summaries)
}

// ---- server management --------------------------------------------------

/// Parameters for starting a Jupyter server.
#[cfg(feature = "jupyter")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerParams {
    /// Working directory for the server (default: current directory)
    pub working_dir: Option<String>,
    /// Port to run the server on (default: 8888)
    pub port: Option<u16>,
    /// Bind address. Defaults to `127.0.0.1`. MUST be a loopback address —
    /// non-loopback hosts are rejected so the server is reachable only from the
    /// operator's own machine.
    pub host: Option<String>,
    /// Token for authentication (default: auto-generated)
    pub token: Option<String>,
    /// Password for authentication (default: none)
    pub password: Option<String>,
    /// Whether to open browser (default: false; always forced off)
    pub open_browser: Option<bool>,
    /// Additional command line args
    pub extra_args: Option<Vec<String>>,
}

/// Result of starting a Jupyter server.
#[cfg(feature = "jupyter")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerResult {
    /// Whether server started successfully
    pub success: bool,
    /// Opaque handle for later `stop_server` / `get_server_status` calls.
    pub handle_id: Option<u64>,
    /// Server URL (e.g. http://127.0.0.1:8888)
    pub url: Option<String>,
    /// Server process ID (informational; operations use `handle_id`)
    pub pid: Option<u32>,
    /// Port the server is running on
    pub port: Option<u16>,
    /// Token used for authentication
    pub token: Option<String>,
    /// Error message if any
    pub error: Option<String>,
}

/// Status of a Jupyter server, queried by handle.
#[cfg(feature = "jupyter")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerStatus {
    /// Whether a server is running
    pub running: bool,
    /// The handle this status refers to
    pub handle_id: u64,
    /// Server URL if running
    pub url: Option<String>,
    /// Port if running
    pub port: Option<u16>,
    /// List of running kernels
    pub kernels: Vec<KernelInfo>,
}

/// Information about a running kernel.
#[cfg(feature = "jupyter")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelInfo {
    pub id: String,
    pub name: String,
    pub last_activity: String,
    pub execution_state: String,
    pub connections: usize,
}

/// Owned jupyter server process retained in the registry.
#[cfg(feature = "jupyter")]
struct ServerHandle {
    child: std::process::Child,
    url: String,
    port: u16,
    token: String,
}

/// Monotonic handle id generator (1..; 0 is reserved as "no handle").
#[cfg(feature = "jupyter")]
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

/// Process-local registry of servers this tool started, keyed by handle id.
#[cfg(feature = "jupyter")]
static SERVERS: LazyLock<Mutex<HashMap<u64, ServerHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(feature = "jupyter")]
fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost" | "localhost.")
}

/// Build the base `jupyter` command with the inherited environment scrubbed
/// and only a minimal, safe allowlist passed back through.
#[cfg(feature = "jupyter")]
fn jupyter_cmd() -> Command {
    let mut cmd = Command::new("jupyter");
    // Scrub the whole inherited environment, then pass back ONLY the minimal
    // allowlist jupyter needs to locate its binary, write config, and render.
    // No newt control-plane env reaches the child.
    cmd.env_clear();
    for key in ENV_ALLOWLIST {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    cmd
}

/// Poll the server's REST API until it answers (or the deadline elapses),
/// instead of sleeping a fixed delay that races startup.
#[cfg(feature = "jupyter")]
fn readiness_probe(url: &str, token: &str, timeout: Duration) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("Failed to build HTTP client for readiness probe")?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let res = client
            .get(format!("{}/api/kernels", url.trim_end_matches('/')))
            .header("Authorization", format!("token {token}"))
            .send();
        if let Ok(resp) = res {
            if resp.status().is_success() {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("jupyter server at {url} did not become ready within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Start a Jupyter server in the background, owned by this process.
///
/// The server is bound to a loopback address only, spawned with a scrubbed
/// environment, and registered under an opaque handle id. Success is
/// confirmed by a REST readiness probe, not a fixed sleep.
#[cfg(feature = "jupyter")]
pub fn start_server(params: JupyterServerParams) -> Result<JupyterServerResult> {
    let working_dir = params
        .working_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let port = params.port.unwrap_or(8888);
    let host = params.host.as_deref().unwrap_or("127.0.0.1");
    if !is_loopback(host) {
        anyhow::bail!(
            "Refusing to bind jupyter to non-loopback host '{host}'; the server is reachable \
             only from the operator's own machine. Use 127.0.0.1 / ::1 / localhost."
        );
    }

    let token = params.token.unwrap_or_else(|| {
        use rand::Rng;
        rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(32)
            .map(char::from)
            .collect()
    });

    // Build the jupyter notebook command. Deliberately NO remote-access /
    // allow-origin flags — loopback binding + the default `False` is
    // load-bearing and keeps the server off the network.
    let mut cmd = jupyter_cmd();
    cmd.arg("notebook")
        .arg("--no-browser")
        .arg("--port")
        .arg(port.to_string())
        .arg("--ip")
        .arg(host)
        .arg("--NotebookApp.token")
        .arg(&token)
        .current_dir(&working_dir);

    if let Some(password) = params.password {
        cmd.arg("--NotebookApp.password").arg(password);
    }

    if let Some(extra_args) = params.extra_args {
        cmd.args(extra_args);
    }

    // Spawn the process detached
    let mut child = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to start jupyter server. Is jupyter installed?")?;

    let pid = child.id();
    let url = format!("http://{host}:{port}");

    // Drain stdout fully (discard) and capture the tail of stderr for
    // diagnostics. Without draining, the OS pipe buffer fills and deadlocks
    // the server — the classic leaked-handle bug.
    if let Some(mut out) = child.stdout.take() {
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while out.read(&mut buf).map(|n| n > 0).unwrap_or(false) {}
        });
    }
    let stderr_tail = std::sync::Arc::new(Mutex::new(Vec::<u8>::new()));
    if let Some(mut err) = child.stderr.take() {
        let tail = std::sync::Arc::clone(&stderr_tail);
        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match err.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut g = tail.lock().unwrap();
                        if g.len() < STDERR_CAP {
                            let take = n.min(STDERR_CAP - g.len());
                            g.extend_from_slice(&buf[..take]);
                        }
                    }
                }
            }
        });
    }

    // Probe the REST API until the server answers (or we time out), instead of
    // sleeping a fixed delay that races startup.
    let probe = readiness_probe(&url, &token, Duration::from_secs(20));

    if let Err(e) = probe {
        // Startup failed. Capture whatever stderr we have, reap the child.
        let stderr_snippet = {
            let g = stderr_tail.lock().unwrap();
            String::from_utf8_lossy(&g).to_string()
        };
        let _ = child.kill();
        let _ = child.wait();
        return Ok(JupyterServerResult {
            success: false,
            handle_id: None,
            url: None,
            pid: None,
            port: None,
            token: None,
            error: Some(format!("{e}\n--- stderr ---\n{stderr_snippet}")),
        });
    }

    // Probe succeeded — confirm the process is still alive (it may have exited
    // in the window between the probe and now).
    match child.try_wait() {
        Ok(Some(status)) => {
            let stderr_snippet = {
                let g = stderr_tail.lock().unwrap();
                String::from_utf8_lossy(&g).to_string()
            };
            Ok(JupyterServerResult {
                success: false,
                handle_id: None,
                url: None,
                pid: None,
                port: None,
                token: None,
                error: Some(format!(
                    "Server exited immediately with status: {status}\n--- stderr ---\n{stderr_snippet}"
                )),
            })
        }
        Ok(None) | Err(_) => {
            let handle_id = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
            SERVERS.lock().unwrap().insert(
                handle_id,
                ServerHandle {
                    child,
                    url: url.clone(),
                    port,
                    token: token.clone(),
                },
            );
            Ok(JupyterServerResult {
                success: true,
                handle_id: Some(handle_id),
                url: Some(url),
                pid: Some(pid),
                port: Some(port),
                token: Some(token),
                error: None,
            })
        }
    }
}

/// Stop a Jupyter server by handle id.
///
/// Kills the owned child directly (`Child::kill`) — no `kill` / `taskkill`
/// subprocess is spawned. Returns `Ok(false)` if the handle is unknown
/// (already stopped or never started by this process).
#[cfg(feature = "jupyter")]
pub fn stop_server(handle_id: u64) -> Result<bool> {
    let mut handle = SERVERS.lock().unwrap().remove(&handle_id);
    match handle.as_mut() {
        Some(h) => {
            let killed = h.child.kill().is_ok();
            let _ = h.child.wait();
            Ok(killed)
        }
        None => Ok(false),
    }
}

/// Get status of a Jupyter server by handle id.
///
/// Looks up the handle this process registered, then queries that server's
/// own REST API (with its own token). A caller cannot point this at an
/// arbitrary URL — only at a server this tool started.
#[cfg(feature = "jupyter")]
pub fn get_server_status(handle_id: u64) -> Result<JupyterServerStatus> {
    // Clone the connection details out of the registry without holding the
    // lock across a network call.
    let (url, token, port) = {
        let g = SERVERS.lock().unwrap();
        match g.get(&handle_id) {
            Some(h) => (h.url.clone(), h.token.clone(), h.port),
            None => {
                return Ok(JupyterServerStatus {
                    running: false,
                    handle_id,
                    url: None,
                    port: None,
                    kernels: vec![],
                });
            }
        }
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let resp = client
        .get(format!("{}/api/kernels", url.trim_end_matches('/')))
        .header("Authorization", format!("token {token}"))
        .send();

    match resp {
        Ok(r) if r.status().is_success() => {
            let kernels: Vec<KernelInfo> = r.json().unwrap_or_default();
            Ok(JupyterServerStatus {
                running: true,
                handle_id,
                url: Some(url),
                port: Some(port),
                kernels,
            })
        }
        _ => Ok(JupyterServerStatus {
            running: false,
            handle_id,
            url: Some(url),
            port: Some(port),
            kernels: vec![],
        }),
    }
}

#[cfg(all(test, feature = "jupyter"))]
mod tests {
    use super::*;

    #[test]
    fn test_jupyter_params_serialization() {
        let params = JupyterExecuteParams {
            notebook_path: "test.ipynb".to_string(),
            working_dir: Some("/tmp".to_string()),
            timeout_seconds: Some(60),
            save_outputs: Some(true),
            kernel_name: Some("python3".to_string()),
        };

        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains("test.ipynb"));
        assert!(json.contains("/tmp"));
    }

    #[test]
    fn test_server_params_serialization() {
        let params = JupyterServerParams {
            working_dir: Some("/tmp".to_string()),
            port: Some(8888),
            host: Some("localhost".to_string()),
            token: Some("test-token".to_string()),
            password: None,
            open_browser: Some(false),
            extra_args: None,
        };

        let json = serde_json::to_string(&params).unwrap();
        assert!(json.contains("8888"));
        assert!(json.contains("test-token"));
    }

    /// Loopback boundary: only true loopback addresses are accepted.
    #[test]
    fn test_loopback_boundaries() {
        for ok in ["127.0.0.1", "::1", "localhost", "localhost."] {
            assert!(is_loopback(ok), "expected '{ok}' to be loopback");
        }
        for bad in [
            "0.0.0.0",
            "0.0.0.0",
            "::",
            "example.com",
            "10.0.0.1",
            "192.168.1.1",
        ] {
            assert!(!is_loopback(bad), "expected '{bad}' to NOT be loopback");
        }
    }

    /// `start_server` must refuse a non-loopback host *before* spawning — so
    /// this test needs no jupyter install and leaves no process behind.
    #[test]
    fn test_start_server_rejects_non_loopback() {
        let err = start_server(JupyterServerParams {
            working_dir: None,
            port: None,
            host: Some("0.0.0.0".to_string()),
            token: None,
            password: None,
            open_browser: None,
            extra_args: None,
        })
        .expect_err("should refuse non-loopback host with an error");
        let msg = err.to_string();
        assert!(
            msg.contains("non-loopback") || msg.contains("loopback"),
            "error should explain the loopback requirement: {msg}"
        );
    }

    /// An unknown handle must report not-running without spawning or touching
    /// any real server.
    #[test]
    fn test_status_unknown_handle_is_not_running() {
        let status = get_server_status(u64::MAX).unwrap();
        assert!(!status.running);
        assert_eq!(status.handle_id, u64::MAX);
        assert!(status.url.is_none());
        assert!(status.kernels.is_empty());
    }

    /// Stopping an unknown handle is a no-op (returns false), not an error.
    #[test]
    fn test_stop_unknown_handle_is_noop() {
        assert!(!stop_server(u64::MAX).unwrap());
    }

    /// True if a `jupyter` binary is reachable on PATH — gates the live-server
    /// integration tests below.
    fn jupyter_available() -> bool {
        jupyter_cmd().arg("--version").output().is_ok()
    }

    /// Live lifecycle: start → running → stop → gone. Needs jupyter installed.
    #[test]
    #[ignore = "requires a jupyter install on PATH"]
    fn test_server_start_status_stop_lifecycle() {
        if !jupyter_available() {
            return;
        }
        // Pick a free port so we don't collide with a running server.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let started = start_server(JupyterServerParams {
            working_dir: Some(std::env::temp_dir().to_string_lossy().to_string()),
            port: Some(port),
            host: Some("127.0.0.1".to_string()),
            token: None,
            password: None,
            open_browser: None,
            extra_args: None,
        })
        .unwrap();
        assert!(started.success, "server should start: {:?}", started.error);
        let handle = started.handle_id.expect("handle id");

        let status = get_server_status(handle).unwrap();
        assert!(status.running, "server should be running after start");
        assert_eq!(status.port, Some(port));

        assert!(stop_server(handle).unwrap(), "stop should report killed");
        // A second stop is a no-op — the handle is gone from the registry.
        assert!(!stop_server(handle).unwrap(), "second stop is a no-op");
    }

    /// An occupied port must fail readiness: jupyter cannot bind, the probe
    /// times out, and we report failure without leaking a process.
    #[test]
    #[ignore = "requires a jupyter install on PATH"]
    fn test_occupied_port_fails() {
        if !jupyter_available() {
            return;
        }
        // Hold the port open so jupyter cannot bind it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let res = start_server(JupyterServerParams {
            working_dir: Some(std::env::temp_dir().to_string_lossy().to_string()),
            port: Some(port),
            host: Some("127.0.0.1".to_string()),
            token: None,
            password: None,
            open_browser: None,
            extra_args: None,
        })
        .unwrap();
        assert!(!res.success, "should not start on an occupied port");
        assert!(res.handle_id.is_none(), "no handle on failure");
        drop(listener);
    }
}
