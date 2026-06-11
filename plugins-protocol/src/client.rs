use std::process::Stdio;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::{
    CompleteRequest, CompleteResponse, InitializeRequest, InitializeResponse, ListModelsResponse,
    RpcErrorObject,
};
use crate::{Error, Result};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct PluginClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl PluginClient {
    pub fn spawn(command: &str, env_pass: &[String]) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for name in env_pass {
            if let Ok(value) = std::env::var(name) {
                cmd.env(name, value);
            }
        }
        preserve_platform_process_env(&mut cmd);
        Self::spawn_command(cmd)
    }

    pub fn spawn_command(mut command: Command) -> Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = spawn_with_etxtbsy_retry(&mut command)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Protocol("plugin stdin was not piped".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Protocol("plugin stdout was not piped".to_string()))?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    pub async fn initialize(&mut self, req: InitializeRequest) -> Result<InitializeResponse> {
        self.request("initialize", req).await
    }

    pub async fn list_models(&mut self) -> Result<ListModelsResponse> {
        self.request("list_models", serde_json::json!({})).await
    }

    pub async fn complete(&mut self, req: CompleteRequest) -> Result<CompleteResponse> {
        self.request("complete", req).await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        let _: Value = self.request("shutdown", serde_json::json!({})).await?;
        Ok(())
    }

    async fn request<P, R>(&mut self, method: &str, params: P) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let id = self.next_id;
        self.next_id += 1;
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut encoded = serde_json::to_vec(&frame)?;
        encoded.push(b'\n');
        self.stdin.write_all(&encoded).await?;
        self.stdin.flush().await?;

        loop {
            let mut line = String::new();
            let read = tokio::time::timeout(REQUEST_TIMEOUT, self.stdout.read_line(&mut line))
                .await
                .map_err(|_| Error::Timeout {
                    method: method.to_string(),
                })??;
            if read == 0 {
                return Err(Error::Protocol(format!(
                    "plugin exited before responding to {method}"
                )));
            }
            let response: RpcResponse = serde_json::from_str(&line)?;
            if response.id != Some(id) {
                continue;
            }
            if let Some(error) = response.error {
                return Err(Error::Rpc {
                    code: error.code,
                    message: error.message,
                });
            }
            let result = response.result.ok_or_else(|| {
                Error::Protocol(format!("plugin response to {method} missing result"))
            })?;
            return Ok(serde_json::from_value(result)?);
        }
    }
}

impl Drop for PluginClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[derive(Debug, serde::Deserialize)]
struct RpcResponse {
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcErrorObject>,
}

#[cfg(windows)]
fn preserve_platform_process_env(cmd: &mut Command) {
    for name in ["ComSpec", "SystemRoot", "PATHEXT"] {
        if let Ok(value) = std::env::var(name) {
            cmd.env(name, value);
        }
    }
}

#[cfg(not(windows))]
fn preserve_platform_process_env(_cmd: &mut Command) {}

/// Spawns the plugin process, retrying briefly on `ETXTBSY`.
///
/// On Unix, exec of an executable fails with "Text file busy" (os error 26)
/// while ANY process holds a write fd on it. A freshly written plugin binary
/// hits this transiently — e.g. a concurrent thread `fork()`s between the
/// writer closing and our exec, and the forked child inherits the write fd
/// for the instant before its own exec closes it (CLOEXEC). The standard
/// remedy (used by cargo and rustup) is a short bounded retry: total worst
/// case here is ~0.5 s before the error is surfaced unchanged.
fn spawn_with_etxtbsy_retry(command: &mut Command) -> std::io::Result<Child> {
    const MAX_ATTEMPTS: u32 = 10;
    let mut delay = Duration::from_millis(5);
    for _ in 1..MAX_ATTEMPTS {
        match command.spawn() {
            Err(e) if is_etxtbsy(&e) => {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(100));
            }
            other => return other,
        }
    }
    command.spawn()
}

#[cfg(unix)]
fn is_etxtbsy(e: &std::io::Error) -> bool {
    // ETXTBSY is 26 on Linux and macOS. (`ErrorKind::ExecutableFileBusy`
    // needs Rust 1.83; the workspace MSRV is 1.75.)
    e.raw_os_error() == Some(26)
}

#[cfg(not(unix))]
fn is_etxtbsy(_e: &std::io::Error) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Regression for the `ETXTBSY` spawn flake (newt-agent CI, seen on
    /// `complete_round_trips_through_mock_plugin`): exec of a plugin
    /// executable fails with "Text file busy" (os error 26) while ANY
    /// process holds a write fd on it. Freshly written plugin scripts hit
    /// this transiently when a concurrent test thread `fork()`s between the
    /// writer closing and our exec — the forked child inherits the write fd
    /// for the instant before its own exec closes it (CLOEXEC).
    ///
    /// This reproduces the race DETERMINISTICALLY: hold an append fd on the
    /// script ourselves, release it ~50 ms later from another thread, and
    /// spawn. Without retry the spawn fails immediately with ETXTBSY;
    /// with the bounded retry it succeeds once the fd drops.
    #[tokio::test]
    async fn spawn_retries_past_transient_etxtbsy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("busy-plugin");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");

        // Hold a write fd on the script — exec now fails with ETXTBSY.
        let held = std::fs::OpenOptions::new()
            .append(true)
            .open(&script)
            .expect("hold write fd");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(held);
        });

        let mut cmd = Command::new(&script);
        cmd.env_clear();
        let client = PluginClient::spawn_command(cmd);
        releaser.join().expect("releaser thread");

        assert!(
            client.is_ok(),
            "spawn must retry past a transient ETXTBSY, got: {:?}",
            client.err()
        );
    }
}
