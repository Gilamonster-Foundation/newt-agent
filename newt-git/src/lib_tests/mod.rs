use super::*;
use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("git runs")
        .success();
    assert!(ok, "git {args:?} failed");
}

/// A temp repo with one commit on `a.txt`.
fn repo_with_commit() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    std::fs::write(p.join("a.txt"), "hello\n").unwrap();
    git(p, &["add", "a.txt"]);
    git(
        p,
        &[
            "-c",
            "user.name=Tester",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "first commit",
        ],
    );
    dir
}

use newt_core::agentic::GitTool as _;

fn tool(dir: &Path) -> LocalGitTool {
    LocalGitTool {
        root: dir.to_path_buf(),
        author: Author {
            name: "newt-agent[bot]".into(),
            email: "bot@example.com".into(),
        },
        // The canonical attribution the session would refresh from the live
        // model + resolved identity. `from_runtime` is tool-less, so this
        // is deterministic in tests (no wall clock, no subprocess).
        attribution: Some(newt_core::attribution::CommitAttribution::from_runtime(
            "qwen3:30b",
            None,
            "noreply@newt-agent.com",
        )),
        commit_succeeded: std::sync::atomic::AtomicUsize::new(0),
        contributors_consumed: std::sync::atomic::AtomicUsize::new(0),
    }
}

fn commit_count(dir: &Path) -> usize {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

fn head_message(dir: &Path) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["log", "-1", "--pretty=%B"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

// Families beside this file. Both attributes are required: rustc needs only
// the `#[path]`, but the ratchets' shared scanner resolves a child ONLY when
// a `#[cfg(test)]` immediately precedes the `mod` (#2149).
#[cfg(test)]
#[path = "attribution.rs"]
mod attribution;
#[cfg(test)]
#[path = "checkout_branch.rs"]
mod checkout_branch;
#[cfg(test)]
#[path = "engine_read.rs"]
mod engine_read;
#[cfg(test)]
#[path = "engine_write.rs"]
mod engine_write;
#[cfg(test)]
#[path = "rebase.rs"]
mod rebase;
#[cfg(test)]
#[path = "stash_and_unknown.rs"]
mod stash_and_unknown;
#[cfg(test)]
#[path = "tool_dispatch.rs"]
mod tool_dispatch;
#[cfg(test)]
#[path = "tool_surface.rs"]
mod tool_surface;
