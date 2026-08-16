//! Jupyter notebook execution tool.
//!
//! All symbols in this module are gated behind `feature = "jupyter"`. The
//! `nbformat` / `reqwest` / `rand` dependencies it uses are likewise only
//! declared `optional = true` in `Cargo.toml`, so a `--no-default-features`
//! build of `newt-tools` stays strictly free of jupyter-only deps and has
//! nothing to link against.

#[cfg(feature = "jupyter")]
use std::process::Command;

#[cfg(feature = "jupyter")]
use anyhow::{Context, Result};
#[cfg(feature = "jupyter")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "jupyter")]
use std::path::{Path, PathBuf};

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

/// Execute a Jupyter notebook using nbconvert
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

    // Build the nbconvert command
    let mut cmd = Command::new("jupyter");
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

/// Parse cell outputs from an executed notebook
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

/// Parameters for starting a Jupyter server
#[cfg(feature = "jupyter")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerParams {
    /// Working directory for the server (default: current directory)
    pub working_dir: Option<String>,
    /// Port to run the server on (default: 8888)
    pub port: Option<u16>,
    /// Host to bind to (default: localhost)
    pub host: Option<String>,
    /// Token for authentication (default: auto-generated)
    pub token: Option<String>,
    /// Password for authentication (default: none)
    pub password: Option<String>,
    /// Whether to open browser (default: false)
    pub open_browser: Option<bool>,
    /// Additional command line args
    pub extra_args: Option<Vec<String>>,
}

/// Result of starting a Jupyter server
#[cfg(feature = "jupyter")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerResult {
    /// Whether server started successfully
    pub success: bool,
    /// Server URL (e.g., http://localhost:8888)
    pub url: Option<String>,
    /// Server process ID
    pub pid: Option<u32>,
    /// Port the server is running on
    pub port: Option<u16>,
    /// Token used for authentication
    pub token: Option<String>,
    /// Error message if any
    pub error: Option<String>,
}

/// Status of a Jupyter server
#[cfg(feature = "jupyter")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JupyterServerStatus {
    /// Whether a server is running
    pub running: bool,
    /// Server URL if running
    pub url: Option<String>,
    /// Process ID if running
    pub pid: Option<u32>,
    /// Port if running
    pub port: Option<u16>,
    /// List of running kernels
    pub kernels: Vec<KernelInfo>,
}

/// Information about a running kernel
#[cfg(feature = "jupyter")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelInfo {
    pub id: String,
    pub name: String,
    pub last_activity: String,
    pub execution_state: String,
    pub connections: usize,
}

/// Start a Jupyter server in the background
#[cfg(feature = "jupyter")]
pub fn start_server(params: JupyterServerParams) -> Result<JupyterServerResult> {
    let working_dir = params
        .working_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let port = params.port.unwrap_or(8888);
    let host = params.host.unwrap_or_else(|| "localhost".to_string());
    let token = params.token.unwrap_or_else(|| {
        use rand::Rng;
        rand::thread_rng()
            .sample_iter(&rand::distributions::Alphanumeric)
            .take(32)
            .map(char::from)
            .collect()
    });

    // Build the jupyter notebook command
    let mut cmd = Command::new("jupyter");
    cmd.arg("notebook")
        .arg("--no-browser")
        .arg("--port")
        .arg(port.to_string())
        .arg("--ip")
        .arg(&host)
        .arg("--NotebookApp.token")
        .arg(&token)
        .arg("--NotebookApp.allow_origin")
        .arg("*")
        .arg("--NotebookApp.allow_remote_access")
        .arg("True")
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

    // Give it a moment to start up
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // Check if process is still alive
    match child.try_wait() {
        Ok(Some(status)) => {
            let stderr = child
                .stderr
                .take()
                .map(|mut s| {
                    use std::io::Read;
                    let mut buf = String::new();
                    s.read_to_string(&mut buf).unwrap_or_default();
                    buf
                })
                .unwrap_or_default();
            Ok(JupyterServerResult {
                success: false,
                url: None,
                pid: None,
                port: None,
                token: None,
                error: Some(format!("Server exited with status: {status}\n{stderr}")),
            })
        }
        Ok(None) => {
            // Server is running, detach the child so it keeps running
            // We intentionally leak the child handle to keep the server running
            std::mem::forget(child);
            Ok(JupyterServerResult {
                success: true,
                url: Some(url),
                pid: Some(pid),
                port: Some(port),
                token: Some(token),
                error: None,
            })
        }
        Err(e) => Ok(JupyterServerResult {
            success: false,
            url: None,
            pid: None,
            port: None,
            token: None,
            error: Some(format!("Failed to check server status: {e}")),
        }),
    }
}

/// Stop a Jupyter server by PID
#[cfg(feature = "jupyter")]
pub fn stop_server(pid: u32) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::process::Command;
        let output = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .output()?;
        Ok(output.status.success())
    }
    #[cfg(windows)]
    {
        use std::process::Command;
        let output = Command::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .arg("/F")
            .output()?;
        Ok(output.status.success())
    }
}

/// Get status of a Jupyter server by querying its API
#[cfg(feature = "jupyter")]
pub fn get_server_status(url: &str, token: Option<&str>) -> Result<JupyterServerStatus> {
    let client = reqwest::blocking::Client::new();
    let mut request = client.get(format!("{}/api/kernels", url.trim_end_matches('/')));

    if let Some(token) = token {
        request = request.header("Authorization", format!("token {token}"));
    }

    match request.send() {
        Ok(response) if response.status().is_success() => {
            let kernels: Vec<KernelInfo> = response.json().unwrap_or_default();
            Ok(JupyterServerStatus {
                running: true,
                url: Some(url.to_string()),
                pid: None,
                port: None,
                kernels,
            })
        }
        Ok(_) => Ok(JupyterServerStatus {
            running: false,
            url: Some(url.to_string()),
            pid: None,
            port: None,
            kernels: vec![],
        }),
        Err(_) => Ok(JupyterServerStatus {
            running: false,
            url: Some(url.to_string()),
            pid: None,
            port: None,
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
}
