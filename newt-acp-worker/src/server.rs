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
    /// Whether this session uses the `newt-coder` plugin
    /// (whole-file emit + server-side diff normalization).
    /// Activated per-session via the `coder: true` field on
    /// `new_session` params, or process-wide via `NEWT_CODER=1`.
    /// See the failure-mode taxonomy in
    /// `~/workspaces/knowledge/board/drake/2026-05-29_newt-coder-failure-mode-taxonomy.md`.
    pub coder_enabled: bool,
}

/// JSON-RPC ACP server. Holds the inference backend and an in-memory
/// session map.
pub struct AcpServer {
    sessions: Arc<Mutex<HashMap<SessionId, Session>>>,
    backend: Arc<dyn newt_inference::InferenceBackend>,
    /// Optional Prometheus metrics registry. When `Some`, every completed
    /// `prompt` turn records timing, token counts, and cost observations.
    metrics: Option<Arc<crate::prom::NewtMetrics>>,
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
///
/// `PartialEq` excludes `metrics` — test assertions compare business logic,
/// not telemetry values that vary per run.
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
    /// Set by the newt-coder plugin: "whole_files", "unified_diff",
    /// or "prose" (the wire-stable constants in
    /// `plugins_protocol::emission_shape`). `None` when the legacy
    /// newt-flat path produced the reply.
    ///
    /// Lets the foreman's scorecard distinguish failure modes T0a /
    /// T0b / T0c instead of lumping them together as "empty diff".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emission_shape: Option<String>,
    /// The model's first raw emission — newt-coder's
    /// `CoderRun.first_emission`, or the flat path's reply content.
    ///
    /// The eval `diff_applies` evaluator runs `git apply --check` against
    /// this (when it is diff-shaped) rather than the post-hoc captured
    /// diff, so a model that emits an unappliable diff the fuzzy worker
    /// only rescued is scored honestly (#30B). `None` for legacy payloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_emission: Option<String>,

    /// Inference telemetry for this turn (timing, token counts, cost).
    /// `None` for legacy/partial responses. Foreman code that does not read
    /// this field is unaffected — the field is skipped when serializing `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<newt_core::TurnMetrics>,
}

impl PartialEq for TaskReply {
    fn eq(&self, other: &Self) -> bool {
        // Intentionally excludes `metrics` — test assertions compare
        // business logic; telemetry values vary per run.
        self.model_id == other.model_id
            && self.content == other.content
            && self.diff == other.diff
            && self.empty_diff == other.empty_diff
            && self.diff_applied == other.diff_applied
            && self.emission_shape == other.emission_shape
            && self.raw_emission == other.raw_emission
    }
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
            emission_shape: None,
            raw_emission: None,
            metrics: None,
        })
    }

    /// Builder: attach the emission shape label the newt-coder plugin
    /// produced. The legacy newt-flat path leaves this `None`.
    #[must_use]
    pub fn with_emission_shape(mut self, shape: impl Into<String>) -> Self {
        self.emission_shape = Some(shape.into());
        self
    }

    /// Builder: attach the model's first raw emission so the eval
    /// `diff_applies` evaluator can judge it with the strict oracle (#30B).
    #[must_use]
    pub fn with_raw_emission(mut self, raw: impl Into<String>) -> Self {
        self.raw_emission = Some(raw.into());
        self
    }

    pub fn with_metrics(mut self, m: newt_core::TurnMetrics) -> Self {
        self.metrics = Some(m);
        self
    }
}

impl AcpServer {
    /// Build a new server bound to `backend`.
    pub fn new(backend: Arc<dyn newt_inference::InferenceBackend>) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            backend,
            metrics: None,
        }
    }

    /// Attach a Prometheus metrics registry. Turns become observable.
    pub fn with_metrics(mut self, metrics: Option<Arc<crate::prom::NewtMetrics>>) -> Self {
        self.metrics = metrics;
        self
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
    ///
    /// Optional params:
    /// - `coder: true` — opt this session into the `newt-coder`
    ///   plugin (whole-file emit + server-side diff normalization).
    ///   The `NEWT_CODER=1` process-wide env opts every session in;
    ///   this per-session field is the finer-grained switch.
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

        let env_coder = std::env::var("NEWT_CODER")
            .map(|v| v == "1")
            .unwrap_or(false);
        let param_coder = params
            .get("coder")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let coder_enabled = env_coder || param_coder;

        let session_id = SessionId::new();
        let mut sessions = self.sessions.lock().await;
        sessions.insert(
            session_id,
            Session {
                workspace_path,
                model_override: None,
                coder_enabled,
            },
        );

        Ok(serde_json::json!({
            "session_id": session_id.to_string(),
            "coder": coder_enabled,
        }))
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
    /// Two dispatch paths:
    ///
    /// - **newt-flat (default).** Sends the operator's prompt verbatim
    ///   with the "respond with unified diffs only" directive; if the
    ///   reply looks like a diff, tries to apply it. This is the
    ///   minimal path that hits failure mode T0b on most local Ollama
    ///   models (see the taxonomy card).
    ///
    /// - **newt-coder.** Activated when `session.coder_enabled` is
    ///   true (via `NEWT_CODER=1` env or `coder: true` on
    ///   `new_session`). Delegates to [`newt_coder::Coder`]: scans
    ///   the workspace for referenced files, injects their contents
    ///   into the prompt, asks the model for whole-file emit, and
    ///   writes the result back to the workspace. The captured
    ///   `git diff` then represents real edits the model actually
    ///   made — closing T0b.
    ///
    /// Both paths capture the post-turn `git diff` and return a
    /// [`TaskReply`] with the mandatory `model_id`, the assistant
    /// content, the diff, `empty_diff` / `diff_applied` flags, and
    /// (newt-coder only) the `emission_shape` label.
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

        let task_reply = if session.coder_enabled {
            self.handle_prompt_coder(&session, &prompt).await?
        } else {
            self.handle_prompt_flat(&session, &prompt).await?
        };

        // Record Prometheus observations — best-effort, never blocks.
        if let (Some(m), Some(ref metrics)) = (&task_reply.metrics, &self.metrics) {
            metrics.record(m);
        }

        Ok(serde_json::to_value(task_reply)?)
    }

    /// Legacy newt-flat path: verbatim prompt + "unified diffs only"
    /// directive. Kept for callers (and the existing eval corpus) that
    /// rely on the minimal-prompt contract.
    async fn handle_prompt_flat(
        &self,
        session: &Session,
        prompt: &str,
    ) -> anyhow::Result<TaskReply> {
        let req = newt_inference::ChatRequest::new()
            .system("You are a coding assistant. Respond with unified diffs only.")
            .user(prompt.to_string());

        let t0 = std::time::Instant::now();
        let reply = self.backend.complete(req).await?;
        let elapsed_ms = t0.elapsed().as_millis() as u64;

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

        let diff = crate::diff::capture_diff(&session.workspace_path)?;
        let raw_emission = reply.content.clone();

        let pricing = newt_core::Config::resolve()
            .ok()
            .and_then(|c| c.pricing)
            .unwrap_or_default();
        let metrics = newt_core::TurnMetrics {
            elapsed_ms,
            usage: reply.usage,
            cost_usd: pricing.estimate_cost(&reply.model_id, reply.usage.as_ref()),
            model_id: reply.model_id.clone(),
            endpoint: self.backend.endpoint().unwrap_or("unknown").to_string(),
        };

        TaskReply::new(reply.model_id, reply.content, diff, diff_applied)
            .map(|r| r.with_raw_emission(raw_emission).with_metrics(metrics))
            .map_err(|e| anyhow::anyhow!("backend returned malformed reply: {e}"))
    }

    /// newt-coder path: whole-file emit + server-side diff normalization.
    /// Closes failure mode T0b on local Ollama coder models.
    async fn handle_prompt_coder(
        &self,
        session: &Session,
        prompt: &str,
    ) -> anyhow::Result<TaskReply> {
        let coder = newt_coder::Coder::new(Arc::clone(&self.backend));
        // 35b: every Coder::run dispatch is gated on a Caveats value.
        // The ACP worker has no peer cert today — that's the 35c handoff
        // (newt-mesh extracts caveats from the verified peer cert and
        // hands them in here). Until then we pass top (= the user's full
        // authority), preserving pre-35b behavior; the enforcement
        // machinery is wired so 35c only needs to swap the argument.
        let caveats = newt_core::Caveats::top();
        let t0 = std::time::Instant::now();
        let run = coder
            .run(&session.workspace_path, prompt, &caveats)
            .await
            .map_err(|e| anyhow::anyhow!("newt-coder run failed: {e}"))?;
        let elapsed_ms = t0.elapsed().as_millis() as u64;

        // newt-coder already wrote any whole-file or unified-diff
        // edits to the workspace; capture the resulting real diff.
        let diff = crate::diff::capture_diff(&session.workspace_path)?;
        let diff_applied = !run.files_written.is_empty() || !diff.trim().is_empty();

        let content = format!(
            "[newt-coder] {} file(s) written via {}",
            run.files_written.len(),
            run.emission_shape,
        );

        let pricing = newt_core::Config::resolve()
            .ok()
            .and_then(|c| c.pricing)
            .unwrap_or_default();
        let coder_metrics = newt_core::TurnMetrics {
            elapsed_ms,
            usage: None, // newt-coder doesn't yet propagate per-turn usage
            cost_usd: None,
            model_id: run.model_id.clone(),
            endpoint: self.backend.endpoint().unwrap_or("unknown").to_string(),
        };
        let _ = pricing; // suppress unused warning until token usage is wired

        Ok(TaskReply::new(run.model_id, content, diff, diff_applied)
            .map_err(|e| anyhow::anyhow!("newt-coder returned malformed reply: {e}"))?
            .with_emission_shape(run.emission_shape)
            .with_raw_emission(run.first_emission)
            .with_metrics(coder_metrics))
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
    fn task_reply_emission_shape_defaults_none() {
        let r = TaskReply::new("m", "c", "", false).unwrap();
        assert_eq!(r.emission_shape, None);
    }

    #[test]
    fn task_reply_with_emission_shape_builder() {
        let r = TaskReply::new("m", "c", "", false)
            .unwrap()
            .with_emission_shape("whole_files");
        assert_eq!(r.emission_shape.as_deref(), Some("whole_files"));
    }

    #[test]
    fn task_reply_omits_null_emission_shape_from_wire() {
        // The legacy newt-flat path produces None; the wire format
        // should not carry a `"emission_shape": null` key (downstream
        // consumers can pre-date the field).
        let r = TaskReply::new("m", "c", "", false).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        assert!(
            !json.contains("emission_shape"),
            "expected emission_shape omitted when None, got: {json}"
        );
    }

    #[test]
    fn task_reply_carries_emission_shape_on_wire_when_set() {
        let r = TaskReply::new("m", "c", "", true)
            .unwrap()
            .with_emission_shape("whole_files");
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"emission_shape\":\"whole_files\""));
        let back: TaskReply = serde_json::from_str(&json).unwrap();
        assert_eq!(back.emission_shape.as_deref(), Some("whole_files"));
    }

    #[test]
    fn task_reply_old_wire_without_emission_shape_still_parses() {
        // A producer that pre-dates this field must still deserialize
        // cleanly — `emission_shape` is `serde(default)`.
        let old =
            r#"{"model_id":"m","content":"c","diff":"","empty_diff":true,"diff_applied":false}"#;
        let r: TaskReply = serde_json::from_str(old).unwrap();
        assert_eq!(r.model_id, "m");
        assert_eq!(r.emission_shape, None);
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
