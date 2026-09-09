//! Reproduction for #1016 — "OCAP sandbox blocks basic functions".
//!
//! **This file characterizes TODAY'S behavior. It is not a regression test for
//! a fix** — #1016 asks for ambient-authority carve-outs, and every candidate
//! carve-out flips an invariant this repo deliberately asserts, so the shape of
//! the fix is a maintainer decision. Until that decision lands, this is the
//! countable fact: the interactive default fence blocks `git commit`.
//!
//! The chain, all on `main`:
//!
//! 1. `ToolPermissions::to_caveats` ships `fs_read: Scope::All` for every preset
//!    (config/permissions.rs) — but the interactive session then NARROWS it:
//!    `newt-tui`'s `policy_for` calls `apply_cli_fs_grants` for every preset
//!    except `full_access`, and `lock_fs_to_workspace` turns `All` into
//!    `Only([workspace])` (caveats.rs). Deliberate: an unconfigured session must
//!    not read outside the CWD.
//! 2. Those session caveats go straight to the confined dispatch
//!    (`agentic::tools::shell::dispatch_bridled_shell` → `Registry::dispatch`),
//!    so `Scope::Only` on the read axis arms the backend's read fence: a
//!    Landlock read ruleset on Linux, `(deny file-read*)` on macOS Seatbelt.
//! 3. `HOME` IS passed through to the confined child by default (`venv_env_map`
//!    seeds the `shell_env_passthrough` allow-list, default HOME+USER), so a
//!    granted tool reaches for `$HOME/…` and gets EACCES — `$HOME` is outside
//!    the fence and outside the backend's `base_read_paths`.
//!
//! `git` is in the `WorkspaceDev` exec allowlist: newt grants the authority to
//! RUN it and then denies the authority to read its configuration. The macOS
//! transcript in #1016 (`file system sandbox blocked open()` on
//! `/Applications/Xcode.app/…/libxcrun.dylib`) is the same mechanism on the
//! other backend — `/Applications` is absent from the macOS `base_read_paths`.
//!
//! Linux-only and `#[serial]`, matching `confined_exec_landlock.rs`: the
//! invariant is the kernel's own enforcement on a real child.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

use newt_core::caveats::{lock_fs_to_workspace, Caveats};
use newt_core::{PermissionPreset, ToolPermissions};
use serial_test::serial;
use tempfile::TempDir;

/// The caveats an interactive session actually hands the shell tool: the
/// `WorkspaceDev` preset lowered to `Caveats`, then locked to the workspace.
///
/// This is `newt-tui::policy_for` minus its two env reads — `apply_cli_fs_grants`
/// is `lock_fs_to_workspace` with the `--read` / `--write` grants parsed out of
/// `NEWT_READ_PATHS` / `NEWT_WRITE_PATHS`, and a default session sets neither.
/// Passing the grants as arguments keeps the test deterministic under a parallel
/// runner instead of mutating process-global env.
fn interactive_caveats(ws: &Path, read_grants: &[String]) -> Caveats {
    let ws = ws.to_string_lossy().into_owned();
    let mut caveats = ToolPermissions {
        preset: PermissionPreset::WorkspaceDev,
        ..Default::default()
    }
    .to_caveats(&ws);
    lock_fs_to_workspace(&mut caveats, &ws, read_grants, &[]);
    caveats
}

/// A workspace that is a real git repo, plus a `HOME` outside it holding the
/// operator's `.gitconfig` — the ordinary layout of any interactive session.
fn repo_and_home() -> (TempDir, TempDir) {
    let ws = TempDir::new().unwrap();
    let out = Command::new("git")
        .args(["init", "-q", "."])
        .current_dir(ws.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "git init: {out:?}");

    let home = TempDir::new().unwrap();
    std::fs::write(
        home.path().join(".gitconfig"),
        "[user]\n\tname = Repro\n\temail = repro@example.invalid\n",
    )
    .unwrap();
    (ws, home)
}

/// `git commit` exactly as `run_command` runs it: the same `Registry::dispatch`
/// entry point `dispatch_bridled_shell` calls, the same `{cmd, cwd, env}` args
/// `confined_dispatch_args` builds, the same session caveats.
async fn confined_commit(ws: &Path, home: &Path, read_grants: &[String]) -> serde_json::Value {
    let args = serde_json::json!({
        "cmd": "git commit --allow-empty -m repro",
        "cwd": ws.to_string_lossy(),
        "env": {
            // `venv_env_map`'s default passthrough. The child SEES $HOME; the
            // fence then denies reading anything in it.
            "HOME": home.to_string_lossy(),
            // Bare-name resolution for the granted `git`. A real session
            // inherits this from the operator's PATH.
            "PATH": "/usr/bin:/bin",
        },
    });
    // The safe-subset engine, not `agent_bridle::registry()`'s brush default:
    // brush re-execs the HOST binary as its confined worker, which a test
    // harness binary cannot serve. `run_command` selects between the two per
    // dispatch (`shell_engine`); both honor the identical `Tool` contract and
    // the identical caveats, so the fence under test is the same one.
    agent_bridle::Registry::builder()
        .tool(std::sync::Arc::new(agent_bridle::ShellTool::new()))
        .build()
        .dispatch("shell", args, &interactive_caveats(ws, read_grants))
        .await
        .expect("the shell dispatch itself must not error")
}

/// The fence must be the kernel's, not an advisory no-op — otherwise both tests
/// below would read the same whether or not the read fence existed.
fn assert_kernel_fenced(out: &serde_json::Value) {
    assert_eq!(
        out["sandbox_kind"], "landlock",
        "this reproduction is only meaningful under a real kernel fence: {out}"
    );
    assert_eq!(
        out["enforcement"]["fs_read"], "kernel",
        "the READ axis specifically must be kernel-enforced: {out}"
    );
}

/// #1016: under the interactive DEFAULT fence, `git commit` fails — the agent
/// may execute `git` but may not read the `~/.gitconfig` that gives it an
/// author identity. Nothing hostile is being attempted; this is the ordinary
/// commit at the end of an ordinary edit.
///
/// The control below runs the SAME command with one `--read $HOME` grant added
/// and requires it to SUCCEED, which pins this failure to the read axis rather
/// than to some unrelated breakage in the confined spawn.
#[tokio::test]
#[serial]
async fn default_fence_blocks_git_commit_because_home_is_unreadable() {
    if !agent_bridle::landlock_is_supported() {
        eprintln!("no Landlock on this host — #1016 reproduction skipped");
        return;
    }
    let (ws, home) = repo_and_home();
    let out = confined_commit(ws.path(), home.path(), &[]).await;
    assert_kernel_fenced(&out);
    assert_ne!(
        out["exit_code"],
        serde_json::json!(0),
        "#1016 no longer reproduces — `git commit` now succeeds under the \
         default fence. If that is intended, delete this file and close #1016: {out}"
    );
    let stderr = out["stderr"].as_str().unwrap_or_default();
    assert!(
        stderr.contains("Author identity unknown") || stderr.contains("Permission denied"),
        "expected the config read to be what failed, got: {out}"
    );
}

/// The control that makes the test above non-vacuous, and the operator
/// workaround that exists today: `newt --read $HOME` (→ `NEWT_READ_PATHS` →
/// `apply_cli_fs_grants`) widens the read fence and the identical command runs.
#[tokio::test]
#[serial]
async fn one_read_grant_over_home_makes_the_same_commit_succeed() {
    if !agent_bridle::landlock_is_supported() {
        eprintln!("no Landlock on this host — #1016 control skipped");
        return;
    }
    let (ws, home) = repo_and_home();
    let grant = vec![home.path().to_string_lossy().into_owned()];
    let out = confined_commit(ws.path(), home.path(), &grant).await;
    assert_kernel_fenced(&out);
    assert_eq!(
        out["exit_code"],
        serde_json::json!(0),
        "one `--read $HOME` grant must let the same commit run: {out}"
    );
}
