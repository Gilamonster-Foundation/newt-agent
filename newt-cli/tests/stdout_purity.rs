//! Regression: `newt worker` and `newt mcp` must NEVER write
//! non-protocol bytes to stdout. Every non-empty line of stdout must
//! parse as JSON-RPC.
//!
//! This protects the structural fix in
//! `newt_cli::stdio_guard::redirect_stdout_to_stderr` and the
//! `with_writer(std::io::stderr)` configuration on the tracing
//! subscriber. If either regresses (someone removes the redirect, a
//! dep adds a `println!` that the redirect was masking, etc.) this
//! test fails.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Initialize request — valid for both ACP (`newt worker`) and MCP
/// (`newt mcp`). Both protocols use newline-delimited JSON-RPC 2.0
/// and accept `initialize` with an empty params object.
fn init_request() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {},
    })
}

#[tokio::test]
async fn worker_stdout_is_pure_json_rpc() {
    let bin = locate_newt_bin();
    spawn_and_assert_pure(&bin, &["worker"]).await;
}

#[tokio::test]
async fn mcp_stdout_is_pure_json_rpc() {
    let bin = locate_newt_bin();
    spawn_and_assert_pure(&bin, &["mcp"]).await;
}

/// Common driver: spawn the binary with the given args, send a single
/// initialize, close stdin, collect stdout, assert every non-empty
/// line parses as JSON.
async fn spawn_and_assert_pure(bin: &PathBuf, args: &[&str]) {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        // OLLAMA_HOST is set to an unreachable address. With the
        // verbatim contract, discover() doesn't probe — so the
        // worker starts cleanly. `initialize` doesn't touch Ollama,
        // so we get a clean response. (If the test sent a `prompt`
        // we'd see an Ollama error on stderr; that's fine — we only
        // assert stdout purity.)
        .env("OLLAMA_HOST", "http://127.0.0.1:1")
        // Crank up tracing on purpose — we want to PROVE that tracing
        // output stays on stderr even when the dep tree is chatty.
        .env("RUST_LOG", "debug")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().expect("spawn newt");

    {
        let mut stdin = child.stdin.take().expect("take stdin");
        let line = format!(
            "{}\n",
            serde_json::to_string(&init_request()).expect("serialize init")
        );
        stdin.write_all(line.as_bytes()).await.expect("write init");
        // Dropping stdin closes it — the server's readline loop exits
        // and the process terminates.
    }

    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .expect("worker timed out")
        .expect("collect worker output");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Every non-empty line of stdout must be valid JSON.
    let mut saw_response = false;
    for (idx, line) in stdout.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(line);
        assert!(
            parsed.is_ok(),
            "stdout line {} is not valid JSON ({} args={:?})\n  line: {:?}\n\nFull stdout:\n{}\n\nFull stderr:\n{}",
            idx + 1,
            bin.display(),
            args,
            line,
            stdout,
            String::from_utf8_lossy(&output.stderr),
        );
        let v = parsed.unwrap();
        assert_eq!(
            v.get("jsonrpc").and_then(|j| j.as_str()),
            Some("2.0"),
            "every stdout line must be a JSON-RPC 2.0 frame: {line}"
        );
        saw_response = true;
    }

    assert!(
        saw_response,
        "expected at least one JSON-RPC response on stdout (got none)\n\nFull stdout:\n{}\n\nFull stderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Same locator strategy as `newt-eval/tests/mock_e2e.rs`:
/// 1. `$CARGO_TARGET_DIR` (set by `cargo llvm-cov`)
/// 2. `<manifest>/../target/{debug,release}/newt`
/// 3. `<manifest>/../target/llvm-cov-target/{debug,release}/newt`
fn locate_newt_bin() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest.parent().expect("manifest dir has parent");

    let mut target_dirs: Vec<PathBuf> = Vec::new();
    if let Some(tdir) = std::env::var_os("CARGO_TARGET_DIR") {
        target_dirs.push(PathBuf::from(tdir));
    }
    target_dirs.push(workspace_root.join("target"));
    target_dirs.push(workspace_root.join("target").join("llvm-cov-target"));

    for tdir in &target_dirs {
        for profile in ["debug", "release"] {
            let candidate = tdir.join(profile).join("newt");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    // Best-effort build — `cargo test` for newt-cli usually builds
    // the `newt` binary as a sibling artifact, but llvm-cov runs in
    // an isolated target dir.
    let _ = std::process::Command::new(env!("CARGO"))
        .args(["build", "--bin", "newt"])
        .output();

    workspace_root.join("target").join("debug").join("newt")
}
