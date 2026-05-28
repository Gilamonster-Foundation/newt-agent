//! ACP server — minimal Agent Client Protocol implementation over stdio.
//!
//! Speaks newline-delimited JSON-RPC 2.0 so `drake-foreman` can dispatch
//! coding goals. Each ACP session pairs a workspace path with optional
//! model override; the `prompt` handler runs inference against the
//! configured backend and (in later steps) applies any unified diff the
//! model returns.
//!
//! Per workspace memory:
//! - Worker ONLY edits files. Never `git add`/`git commit`/`git push`.
//! - Empty `git diff` post-turn surfaces as `empty_diff: true` in the
//!   reply (the CLI binary translates that into a non-zero exit).
//! - `TaskReply.model_id` is mandatory.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use newt_core::SessionId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// Per-session state kept by the ACP server.
#[derive(Debug, Clone)]
pub struct Session {
    /// Workspace root for this session.
    pub workspace_path: PathBuf,
    /// Optional model override set via `set_session_model`.
    pub model_override: Option<String>,
}

/// JSON-RPC ACP server. Holds the inference backend and an in-memory
/// session map.
pub struct AcpServer {
    sessions: Arc<Mutex<HashMap<SessionId, Session>>>,
    backend: Arc<dyn newt_inference::InferenceBackend>,
}

/// Structured reply for a `prompt` turn.
///
/// `model_id` is non-Option so it cannot be silently omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskReply {
    /// MANDATORY — the model that produced this reply.
    pub model_id: String,
    /// Assistant content (typically the unified diff).
    pub content: String,
    /// Captured workspace diff after the turn.
    pub diff: String,
    /// True if the captured diff is empty (no real changes).
    pub empty_diff: bool,
    /// True if a unified diff was detected in `content` and applied
    /// successfully.
    pub diff_applied: bool,
}

impl AcpServer {
    /// Build a new server bound to `backend`.
    pub fn new(backend: Arc<dyn newt_inference::InferenceBackend>) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            backend,
        }
    }

    /// Run the server over stdin/stdout.
    pub async fn run_stdio(self) -> anyhow::Result<()> {
        self.run(tokio::io::stdin(), tokio::io::stdout()).await
    }

    /// Run the server over arbitrary async reader/writer.
    pub async fn run<R, W>(self, reader: R, mut writer: W) -> anyhow::Result<()>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let buf = BufReader::new(reader);
        let mut lines = buf.lines();

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }

            let request: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    let resp = error_response(Value::Null, -32700, &format!("Parse error: {e}"));
                    write_response(&mut writer, &resp).await?;
                    continue;
                }
            };

            let id = request.get("id").cloned().unwrap_or(Value::Null);
            let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let params = request.get("params").cloned().unwrap_or(Value::Null);

            let response = match self.handle(method, params).await {
                Ok(result) => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }),
                Err(e) => error_response(id, -32603, &e.to_string()),
            };

            write_response(&mut writer, &response).await?;
        }

        Ok(())
    }

    /// Dispatch one parsed request to the matching handler.
    async fn handle(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        match method {
            "initialize" => self.handle_initialize(params).await,
            "new_session" => self.handle_new_session(params).await,
            "set_session_model" => self.handle_set_session_model(params).await,
            "prompt" => self.handle_prompt(params).await,
            _ => anyhow::bail!("method not found: {method}"),
        }
    }

    /// `initialize` — return the protocol version and capabilities.
    async fn handle_initialize(&self, _params: Value) -> anyhow::Result<Value> {
        Ok(serde_json::json!({
            "protocolVersion": "v0.1",
            "serverInfo": {
                "name": "newt-acp-worker",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "prompting": true,
                "diff_capture": true,
            },
        }))
    }

    /// `new_session` — create a session bound to a workspace path.
    async fn handle_new_session(&self, params: Value) -> anyhow::Result<Value> {
        let workspace_path: PathBuf = params
            .get("workspace_path")
            .and_then(|p| p.as_str())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("workspace_path required"))?;

        if !workspace_path.exists() {
            anyhow::bail!(
                "workspace_path does not exist: {}",
                workspace_path.display()
            );
        }

        let session_id = SessionId::new();
        let mut sessions = self.sessions.lock().await;
        sessions.insert(
            session_id,
            Session {
                workspace_path,
                model_override: None,
            },
        );

        Ok(serde_json::json!({ "session_id": session_id.to_string() }))
    }

    /// `set_session_model` — override the model used for subsequent
    /// `prompt` turns within an existing session.
    async fn handle_set_session_model(&self, params: Value) -> anyhow::Result<Value> {
        let session_id: SessionId = params
            .get("session_id")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("session_id required"))?
            .parse()?;
        let model = params
            .get("model")
            .and_then(|m| m.as_str())
            .ok_or_else(|| anyhow::anyhow!("model required"))?
            .to_string();

        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(&session_id)
            .ok_or_else(|| anyhow::anyhow!("unknown session: {session_id}"))?;
        session.model_override = Some(model);

        Ok(serde_json::json!({ "ok": true }))
    }

    /// `prompt` — run one inference turn against the session's workspace.
    ///
    /// Sends the prompt to the configured backend, optionally applies a
    /// unified diff returned by the model, and returns a [`TaskReply`]
    /// carrying the (mandatory) `model_id` plus assistant content. Diff
    /// capture lands in Step 9.4; for Step 9.3 the returned reply has
    /// an empty `diff` field.
    async fn handle_prompt(&self, params: Value) -> anyhow::Result<Value> {
        let session_id: SessionId = params
            .get("session_id")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("session_id required"))?
            .parse()?;
        let prompt = params
            .get("prompt")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("prompt required"))?
            .to_string();

        let session = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(&session_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown session: {session_id}"))?
        };

        let req = newt_inference::ChatRequest::new()
            .system("You are a coding assistant. Respond with unified diffs only.")
            .user(prompt);

        let reply = self.backend.complete(req).await?;

        // If the reply contains a unified diff, try to apply it. We
        // accept the patch unconditionally on success; on failure we
        // log and continue so the diff text still makes it back to the
        // caller for debugging.
        let diff_applied = if looks_like_unified_diff(&reply.content) {
            match newt_tools::apply_patch(&session.workspace_path, &reply.content) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(error = %e, "patch application failed");
                    false
                }
            }
        } else {
            false
        };

        // Capture the post-turn diff. Empty diff is the deterministic
        // "the worker did nothing useful" signal — we surface it as a
        // boolean field rather than crashing the server (the CLI
        // binary can translate `empty_diff: true` into a non-zero
        // exit when running `newt worker --once`).
        let diff = crate::diff::capture_diff(&session.workspace_path)?;
        let empty_diff = crate::diff::is_empty_diff(&diff);

        let task_reply = TaskReply {
            model_id: reply.model_id,
            content: reply.content,
            diff,
            empty_diff,
            diff_applied,
        };
        Ok(serde_json::to_value(task_reply)?)
    }
}

/// True if `content` looks like a unified diff (has both `--- ` and
/// `+++ ` headers). Cheap heuristic — the real parser in
/// `newt_tools::apply_patch` is the source of truth on validity.
fn looks_like_unified_diff(content: &str) -> bool {
    content.contains("--- ") && content.contains("+++ ")
}

/// Write a JSON-RPC response as a single newline-terminated line.
async fn write_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    response: &Value,
) -> anyhow::Result<()> {
    let mut out = serde_json::to_string(response)?;
    out.push('\n');
    writer.write_all(out.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Build a JSON-RPC error response value.
fn error_response(id: Value, code: i32, message: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}
