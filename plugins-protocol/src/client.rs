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
        let mut child = command.spawn()?;
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
