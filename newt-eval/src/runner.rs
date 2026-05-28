//! Subprocess runner that drives a `newt worker` over ACP stdio JSON-RPC.
//!
//! For each [`TestCase`] the runner:
//!
//! 1. Copies the case's `workspace/` fixture into a fresh tempdir,
//!    `git init`s it, and commits the baseline so post-turn diffs are
//!    meaningful.
//! 2. Copies the same fixture into a second `baseline/` tempdir for the
//!    evaluators that need a pre-state to compare against.
//! 3. Spawns `newt worker` as a child process with `OLLAMA_HOST` set
//!    (mock URL in mock mode, unset in live mode so the worker uses its
//!    default endpoint search).
//! 4. Drives the ACP protocol over stdin/stdout: `initialize` →
//!    `new_session` → optional `set_session_model` → `prompt`.
//! 5. Returns the [`TaskReply`] plus the post-worker workspace path for
//!    evaluators to inspect.
//!
//! Each case is **completely isolated** — fresh tempdirs, fresh git init,
//! fresh subprocess. No state crosses between cases.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use newt_acp_worker::TaskReply;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::cases::TestCase;

/// What the runner returns after one full case execution.
///
/// `workspace` and `baseline` are tempdir paths kept alive by the embedded
/// `_*_guard` fields until the outcome is dropped.
#[derive(Debug)]
pub struct RunOutcome {
    pub case: TestCase,
    pub reply: TaskReply,
    pub workspace: PathBuf,
    pub baseline: PathBuf,
    /// Kept alive so the temp dirs aren't pruned out from under the
    /// evaluators. Dropped when the outcome is dropped.
    pub _workspace_guard: tempfile::TempDir,
    pub _baseline_guard: tempfile::TempDir,
}

/// How to run the worker. `mock_endpoint` is set in mock mode (the
/// wiremock URL); `None` in live mode (the worker discovers Ollama via
/// `OLLAMA_HOST` or its default endpoint list).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    pub worker_bin: PathBuf,
    pub mock_endpoint: Option<String>,
    pub model_override: Option<String>,
    /// Hard upper bound on a single case. 60s is roomy for mock and
    /// short enough that a hung live model doesn't stall CI forever.
    pub timeout: Duration,
}

impl RunnerConfig {
    pub fn new(worker_bin: impl Into<PathBuf>) -> Self {
        Self {
            worker_bin: worker_bin.into(),
            mock_endpoint: None,
            model_override: None,
            timeout: Duration::from_secs(60),
        }
    }

    pub fn with_mock_endpoint(mut self, url: impl Into<String>) -> Self {
        self.mock_endpoint = Some(url.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model_override = Some(model.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Run one case end-to-end. See module docs for the protocol.
pub async fn run_case(case: &TestCase, config: &RunnerConfig) -> anyhow::Result<RunOutcome> {
    if !config.worker_bin.exists() {
        anyhow::bail!(
            "newt worker binary not found at {} — run `cargo build --bin newt`",
            config.worker_bin.display()
        );
    }

    let workspace_guard = tempfile::tempdir()?;
    let baseline_guard = tempfile::tempdir()?;
    let workspace = workspace_guard.path().to_path_buf();
    let baseline = baseline_guard.path().to_path_buf();

    copy_fixture(&case.workspace_fixture(), &workspace)?;
    copy_fixture(&case.workspace_fixture(), &baseline)?;
    init_baseline_git(&workspace)?;

    let mut child = spawn_worker(config)?;

    let result = tokio::time::timeout(
        config.timeout,
        drive_acp(
            &mut child,
            &workspace,
            &case.prompt,
            config.model_override.as_deref(),
        ),
    )
    .await;

    // Always try to clean up the child whether the conversation
    // succeeded or not — `Child::kill_on_drop(true)` handles it on drop,
    // but eagerly waiting here keeps tests deterministic.
    let _ = child.start_kill();
    let _ = child.wait().await;

    let reply = match result {
        Ok(Ok(reply)) => reply,
        Ok(Err(e)) => return Err(e),
        Err(_) => anyhow::bail!("worker timed out after {:?}", config.timeout),
    };

    Ok(RunOutcome {
        case: case.clone(),
        reply,
        workspace,
        baseline,
        _workspace_guard: workspace_guard,
        _baseline_guard: baseline_guard,
    })
}

/// Recursively copy `src` into `dst` (which must exist).
fn copy_fixture(src: &Path, dst: &Path) -> anyhow::Result<()> {
    let mut opts = fs_extra::dir::CopyOptions::new();
    opts.content_only = true;
    opts.overwrite = true;
    fs_extra::dir::copy(src, dst, &opts)
        .map_err(|e| anyhow::anyhow!("copy fixture {} -> {}: {e}", src.display(), dst.display()))?;
    Ok(())
}

/// `git init` + commit everything as baseline so the worker's
/// `git diff --no-color` capture step has something to compare against.
fn init_baseline_git(workspace: &Path) -> anyhow::Result<()> {
    let run = |args: &[&str]| -> anyhow::Result<()> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(workspace)
            .output()?;
        if !output.status.success() {
            anyhow::bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    };
    run(&["init", "-q", "-b", "main"])?;
    run(&["config", "user.email", "eval@newt-eval"])?;
    run(&["config", "user.name", "newt-eval"])?;
    run(&["add", "-A"])?;
    run(&["commit", "-q", "-m", "baseline", "--allow-empty"])?;
    Ok(())
}

/// Spawn `newt worker` with the configured Ollama endpoint.
fn spawn_worker(config: &RunnerConfig) -> anyhow::Result<Child> {
    let mut cmd = Command::new(&config.worker_bin);
    cmd.arg("worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if let Some(url) = &config.mock_endpoint {
        cmd.env("OLLAMA_HOST", url);
    }

    // Quieter logs in eval runs — the worker is chatty at info level.
    cmd.env("RUST_LOG", "warn");

    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("spawn {}: {e}", config.worker_bin.display()))
}

/// Send the ACP request sequence and return the parsed [`TaskReply`].
async fn drive_acp(
    child: &mut Child,
    workspace: &Path,
    prompt: &str,
    model_override: Option<&str>,
) -> anyhow::Result<TaskReply> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("worker stdin already taken"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("worker stdout already taken"))?;
    let mut stdout = BufReader::new(stdout).lines();

    // 1. initialize
    send_line(
        &mut stdin,
        &json_rpc("initialize", 1, serde_json::json!({})),
    )
    .await?;
    let _init = read_response(&mut stdout, 1).await?;

    // 2. new_session
    let new_session = json_rpc(
        "new_session",
        2,
        serde_json::json!({ "workspace_path": workspace.to_string_lossy() }),
    );
    send_line(&mut stdin, &new_session).await?;
    let session_resp = read_response(&mut stdout, 2).await?;
    let session_id = session_resp["result"]["session_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("new_session response missing session_id"))?
        .to_string();

    // 3. set_session_model (optional)
    if let Some(model) = model_override {
        let req = json_rpc(
            "set_session_model",
            3,
            serde_json::json!({ "session_id": session_id, "model": model }),
        );
        send_line(&mut stdin, &req).await?;
        let _ = read_response(&mut stdout, 3).await?;
    }

    // 4. prompt
    let prompt_req = json_rpc(
        "prompt",
        4,
        serde_json::json!({ "session_id": session_id, "prompt": prompt }),
    );
    send_line(&mut stdin, &prompt_req).await?;
    let prompt_resp = read_response(&mut stdout, 4).await?;

    let reply: TaskReply = serde_json::from_value(prompt_resp["result"].clone())
        .map_err(|e| anyhow::anyhow!("decode TaskReply: {e}"))?;

    // Close stdin so the worker's `lines()` loop exits cleanly.
    drop(stdin);

    Ok(reply)
}

/// Build a JSON-RPC request value.
fn json_rpc(method: &str, id: i64, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Serialize `value` as a single newline-terminated line and write it.
async fn send_line<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Value,
) -> anyhow::Result<()> {
    let mut s = serde_json::to_string(value)?;
    s.push('\n');
    writer.write_all(s.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one JSON-RPC response from `lines` and check its `id`.
async fn read_response<R: AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
    expected_id: i64,
) -> anyhow::Result<Value> {
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow::anyhow!("worker closed stdout before responding"))?;
    let v: Value = serde_json::from_str(&line)
        .map_err(|e| anyhow::anyhow!("parse worker response '{line}': {e}"))?;
    if let Some(err) = v.get("error") {
        if !err.is_null() {
            anyhow::bail!("worker returned JSON-RPC error: {err}");
        }
    }
    if v.get("id").and_then(Value::as_i64) != Some(expected_id) {
        anyhow::bail!(
            "worker response id mismatch: expected {expected_id}, got {:?}",
            v.get("id")
        );
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_builders_compose() {
        let cfg = RunnerConfig::new("/tmp/newt")
            .with_mock_endpoint("http://127.0.0.1:8080")
            .with_model("llama3.1:8b")
            .with_timeout(Duration::from_secs(5));
        assert_eq!(cfg.worker_bin, PathBuf::from("/tmp/newt"));
        assert_eq!(cfg.mock_endpoint.as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(cfg.model_override.as_deref(), Some("llama3.1:8b"));
        assert_eq!(cfg.timeout, Duration::from_secs(5));
    }

    #[test]
    fn json_rpc_builds_request() {
        let req = json_rpc("ping", 7, serde_json::json!({"k": "v"}));
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 7);
        assert_eq!(req["method"], "ping");
        assert_eq!(req["params"]["k"], "v");
    }

    #[tokio::test]
    async fn run_case_errors_when_worker_bin_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let case_dir = tmp.path().join("case");
        std::fs::create_dir_all(case_dir.join("workspace")).unwrap();
        std::fs::write(
            case_dir.join("case.toml"),
            r#"
name = "x"
description = ""
language = "rust"
prompt = ""
evaluators = []

[mock_response]
content = ""
"#,
        )
        .unwrap();
        let case = TestCase::load_dir(&case_dir).unwrap();
        let cfg = RunnerConfig::new("/nonexistent/newt");
        let err = run_case(&case, &cfg).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn copy_fixture_clones_tree() {
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("a.txt"), "hi").unwrap();
        std::fs::create_dir_all(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub/b.txt"), "bye").unwrap();

        let dst = tempfile::tempdir().unwrap();
        copy_fixture(src.path(), dst.path()).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.path().join("a.txt")).unwrap(),
            "hi"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("sub/b.txt")).unwrap(),
            "bye"
        );
    }

    #[test]
    fn init_baseline_git_creates_repo() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), "hi").unwrap();
        init_baseline_git(tmp.path()).unwrap();
        // .git directory exists
        assert!(tmp.path().join(".git").exists());
        // and a clean working tree
        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(status.stdout.is_empty(), "expected clean working tree");
    }
}
