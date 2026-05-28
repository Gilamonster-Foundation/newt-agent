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

/// Structured reply for one `prompt` turn.
///
/// # Contract
///
/// Per workspace memory `feedback_drake_patch_not_prose` and
/// `feedback_empty_diff_is_a_crash`:
///
/// - `model_id` is **mandatory**. It is a non-Option `String` so the
///   field cannot be silently omitted from the wire format. Use
///   [`TaskReply::new`] for the validated constructor that rejects an
///   empty `model_id` — foreman uses this field to attribute the
///   patch and update the model's scorecard, so a missing id is
///   non-recoverable.
/// - `empty_diff: true` means the worker produced no real edits and
///   foreman should disqualify it pre-arbiter.
/// - `diff_applied: true` means a unified diff was found in `content`
///   and `newt_tools::apply_patch` accepted it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

impl TaskReply {
    /// Validated constructor. Rejects an empty `model_id` so a buggy
    /// backend can't silently produce an unattributable reply.
    pub fn new(
        model_id: impl Into<String>,
        content: impl Into<String>,
        diff: impl Into<String>,
        diff_applied: bool,
    ) -> anyhow::Result<Self> {
        let model_id = model_id.into();
        if model_id.is_empty() {
            anyhow::bail!("TaskReply.model_id is mandatory and must not be empty");
        }
        let diff = diff.into();
        let empty_diff = crate::diff::is_empty_diff(&diff);
        Ok(Self {
            model_id,
            content: content.into(),
            diff,
            empty_diff,
            diff_applied,
        })
    }
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
    /// unified diff returned by the model, captures the post-turn
    /// workspace diff, and returns a [`TaskReply`] carrying the
    /// (mandatory) `model_id`, assistant content, captured diff, and
    /// `empty_diff` / `diff_applied` flags.
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

        let task_reply = TaskReply::new(reply.model_id, reply.content, diff, diff_applied)
            .map_err(|e| {
                // If the backend handed us an empty model_id, fail
                // loudly — foreman cannot attribute the patch.
                anyhow::anyhow!("backend returned malformed reply: {e}")
            })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_reply_rejects_empty_model_id() {
        let err = TaskReply::new("", "content", "", false).unwrap_err();
        assert!(
            err.to_string().contains("mandatory"),
            "expected mandatory-id error, got: {err}"
        );
    }

    #[test]
    fn task_reply_accepts_nonempty_model_id() {
        let r = TaskReply::new("qwen2.5-coder:32b", "hi", "", false).unwrap();
        assert_eq!(r.model_id, "qwen2.5-coder:32b");
        assert_eq!(r.content, "hi");
    }

    #[test]
    fn task_reply_sets_empty_diff_from_diff_string() {
        let r = TaskReply::new("m", "c", "", false).unwrap();
        assert!(r.empty_diff);

        let r = TaskReply::new("m", "c", "real\nchanges\n", true).unwrap();
        assert!(!r.empty_diff);
    }

    #[test]
    fn task_reply_serde_round_trip_preserves_model_id() {
        let r = TaskReply::new("m", "c", "d\n", true).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        // The wire format must always include model_id.
        assert!(json.contains("\"model_id\":\"m\""));
        let back: TaskReply = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn task_reply_deserialize_without_model_id_fails() {
        // Direct serde deserialization of a payload missing model_id
        // must fail — the field is required.
        let bad = r#"{"content":"c","diff":"","empty_diff":true,"diff_applied":false}"#;
        let err = serde_json::from_str::<TaskReply>(bad).unwrap_err();
        assert!(
            err.to_string().contains("model_id"),
            "expected missing-model_id error, got: {err}"
        );
    }

    #[test]
    fn looks_like_unified_diff_detects_headers() {
        assert!(looks_like_unified_diff(
            "--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-a\n+b\n"
        ));
        assert!(!looks_like_unified_diff("just prose"));
        assert!(!looks_like_unified_diff("--- only the old header"));
    }
}
