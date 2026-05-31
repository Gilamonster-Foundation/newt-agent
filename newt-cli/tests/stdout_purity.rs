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

/// Locate the `newt` binary in whatever target directory cargo is
/// actually using.
///
/// Search order (first match wins):
/// 1. `$CARGO_TARGET_DIR` — explicit-wins, mirrors `cargo llvm-cov`
///    and any operator-set override.
/// 2. `cargo_metadata::MetadataCommand` — picks up `~/.cargo/config.toml`'s
///    `[build] target-dir` (the shared cache the workspace standardizes
///    on; see `~/workspaces/WORKSPACE_RULES.md`). This is the case the
///    plain `<manifest>/../target` fallback misses.
/// 3. `<manifest>/../target` — final fallback for environments where
///    `cargo metadata` itself fails (e.g. a partially-built sysroot).
/// 4. `<manifest>/../target/llvm-cov-target` — legacy llvm-cov path
///    kept for parity with older CI invocations that didn't set
///    `CARGO_TARGET_DIR`.
///
/// Each directory is probed for `release/newt` first, then `debug/newt`
/// (the test driver builds with `cargo test --release` in CI; locally
/// it's usually debug).
///
/// If nothing is found, panic with the full list of directories
/// searched — same UX as the runner-side #40/#43 fix.
fn locate_newt_bin() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest.parent().expect("manifest dir has parent");

    let mut target_dirs: Vec<PathBuf> = Vec::new();

    // 1. Explicit env var — always wins.
    if let Some(tdir) = std::env::var_os("CARGO_TARGET_DIR") {
        target_dirs.push(PathBuf::from(tdir));
    }

    // 2. `cargo metadata` — picks up `~/.cargo/config.toml`'s
    //    `[build] target-dir`. Best-effort: if the call fails (e.g.
    //    offline registry, partially-built sysroot) we just skip it
    //    and fall back to the conventional paths below.
    if let Ok(meta) = cargo_metadata::MetadataCommand::new()
        .manifest_path(workspace_root.join("Cargo.toml"))
        .no_deps()
        .exec()
    {
        target_dirs.push(PathBuf::from(meta.target_directory.as_std_path()));
    }

    // 3 + 4. Conventional fallbacks.
    target_dirs.push(workspace_root.join("target"));
    target_dirs.push(workspace_root.join("target").join("llvm-cov-target"));

    for tdir in &target_dirs {
        for profile in ["release", "debug"] {
            let candidate = tdir.join(profile).join("newt");
            if candidate.exists() {
                return candidate;
            }
        }
    }

    // Nothing found. Surface every path we tried — the runner-side
    // #40/#43 fix taught us that "binary not found" is useless without
    // the search list.
    let searched: Vec<String> = target_dirs
        .iter()
        .flat_map(|d| {
            ["release", "debug"]
                .iter()
                .map(move |p| d.join(p).join("newt").display().to_string())
        })
        .collect();
    panic!(
        "newt binary not found. Searched:\n  - {}\n\
         Hint: run `cargo build -p newt-agent` (or `--release`) in the workspace root.",
        searched.join("\n  - "),
    );
}
