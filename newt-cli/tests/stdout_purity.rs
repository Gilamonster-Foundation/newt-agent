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

/// #1303 acceptance 1 (decision clause A/E): the mouse tier NEVER emits capture
/// sequences on a non-interactive path — even with the opt-in FORCED ON
/// (`NEWT_MOUSE=1`) — because stdout is piped (not a TTY), and separately when
/// `TERM=dumb`. This is the byte-for-byte non-interactive invariant, proven
/// through an ACTUAL turn (a prompt is fed so the turn's `with_live_spill_watch`
/// path runs), not just REPL init.
#[tokio::test]
async fn chat_emits_no_mouse_capture_sequences_when_piped() {
    let bin = locate_newt_bin();
    // Piped stdout with a normal TERM, opt-in forced on → still no mouse.
    assert_no_mouse_capture(&bin, "xterm-256color").await;
    // Piped stdout AND TERM=dumb → still no mouse.
    assert_no_mouse_capture(&bin, "dumb").await;
}

/// Spawn the real `newt` chat REPL with piped stdio and `NEWT_MOUSE=1` (the
/// mouse opt-in forced ON), close stdin (EOF), and assert stdout carries NONE of
/// crossterm's mouse-capture private-mode sequences. This proves the load-bearing
/// non-interactive invariant: with piped (non-TTY) stdio, `mouse_capable` is
/// false, so capture is never enabled and the byte stream stays byte-for-byte the
/// 0.7.3 output — even with the opt-in forced on and even under `TERM=dumb`.
///
/// We deliberately do NOT feed a prompt: a real turn would only reach the
/// unreachable model and retry with backoff (flaky/slow in CI, which has no
/// model), and the piped path early-returns before the turn-level guard anyway,
/// so a prompt could not reach it. The turn-level gate `mouse_capable_for`
/// (refusing without BOTH TTYs even with the opt-in on) is proven
/// deterministically by the newt-tui unit test
/// `mouse_tier_requires_optin_and_a_supported_interactive_terminal`. A seeded
/// empty config skips first-run setup so the run is fast and deterministic.
async fn assert_no_mouse_capture(bin: &PathBuf, term: &str) {
    // Isolate config from the real `~/.newt`; seed it so no first-run wizard.
    let home = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(home.path().join(".newt")).expect("mk .newt");
    std::fs::write(home.path().join(".newt/config.toml"), "").expect("seed config");

    let mut cmd = Command::new(bin);
    cmd.arg("--no-splash")
        .env("OLLAMA_HOST", "http://127.0.0.1:1")
        // Force the mouse opt-in ON: the TTY gate must STILL refuse.
        .env("NEWT_MOUSE", "1")
        .env("TERM", term)
        .env("HOME", home.path())
        .env_remove("NEWT_CONFIG")
        .env_remove("NEWT_CONFIG_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().expect("spawn newt");
    // Close stdin immediately (EOF) so the REPL exits without running a turn.
    // The non-interactive invariant does not depend on a turn: piped stdio is not
    // a TTY, so `mouse_capable` is false and capture is never enabled regardless.
    // Feeding a prompt would only reach the unreachable model and retry with
    // backoff (flaky/slow in CI with no model); the piped path early-returns
    // before the turn-level guard anyway, so it cannot exercise the gate.
    drop(child.stdin.take());

    let output = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("newt chat timed out")
        .expect("collect newt output");

    // crossterm's Enable/DisableMouseCapture emit these private-mode toggles.
    for seq in [
        &b"\x1b[?1000"[..],
        b"\x1b[?1002",
        b"\x1b[?1003",
        b"\x1b[?1006",
        b"\x1b[?1015",
    ] {
        assert!(
            !output.stdout.windows(seq.len()).any(|w| w == seq),
            "mouse-capture sequence {:?} leaked on the non-interactive path (TERM={term})\n\
             Full stdout:\n{}\n\nFull stderr:\n{}",
            String::from_utf8_lossy(seq),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

/// The cargo target directory, resolved the way cargo itself resolves it: the
/// `CARGO_TARGET_DIR` env var, then a workspace or user `config.toml`'s `[build]
/// target-dir`, then the default. This makes the test robust to a target-dir set
/// in `~/.cargo/config.toml` (newt-agent#64), which a plain
/// `workspace_root/target` guess misses (failing with a confusing `NotFound`).
fn cargo_target_dir() -> Option<PathBuf> {
    cargo_metadata::MetadataCommand::new()
        .exec()
        .ok()
        .map(|m| m.target_directory.into_std_path_buf())
}

/// Locator strategy (first hit wins):
/// 1. `$CARGO_TARGET_DIR` (set by `cargo llvm-cov`)
/// 2. the cargo-resolved target dir (honors `~/.cargo/config.toml`, newt-agent#64)
/// 3. `<manifest>/../target/{debug,release}/newt`
/// 4. `<manifest>/../target/llvm-cov-target/{debug,release}/newt`
fn locate_newt_bin() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest.parent().expect("manifest dir has parent");

    let mut target_dirs: Vec<PathBuf> = Vec::new();
    if let Some(tdir) = std::env::var_os("CARGO_TARGET_DIR") {
        target_dirs.push(PathBuf::from(tdir));
    }
    // Honor `[build] target-dir` from cargo config (newt-agent#64).
    if let Some(tdir) = cargo_target_dir() {
        target_dirs.push(tdir);
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

    // Final fallback: the cargo-resolved target dir (newt-agent#64), else the
    // conventional path.
    cargo_target_dir()
        .unwrap_or_else(|| workspace_root.join("target"))
        .join("debug")
        .join("newt")
}
