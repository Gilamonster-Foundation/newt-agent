//! Git diff capture for the ACP worker.
//!
//! After the LLM finishes a turn, the worker shells out to
//! `git diff --no-color` to capture what changed. The empty-diff
//! detector turns "the worker produced no real edits" into a
//! deterministic signal — `feedback_empty_diff_is_a_crash` in workspace
//! memory says foreman should disqualify the worker pre-arbiter.
//!
//! We keep the contract loose here: a workspace that isn't a git repo
//! returns an empty diff with a `tracing::warn!`, not an error. The
//! ACP layer above decides what to do with that.

use std::path::Path;
use std::process::Command;

/// Capture the workspace diff with `git diff --no-color`.
///
/// Returns the diff text on success. Non-zero `git` exit, missing
/// binary, or non-git workspace all return an empty string with a
/// tracing warning — the absence of a diff is itself the signal we
/// want to surface, and the empty-diff detector picks it up.
pub fn capture_diff(workspace: &Path) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(["diff", "--no-color"])
        .current_dir(workspace)
        .output();

    match output {
        Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(out) => {
            tracing::warn!(
                stderr = %String::from_utf8_lossy(&out.stderr),
                "git diff returned non-zero status"
            );
            Ok(String::new())
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not invoke git diff");
            Ok(String::new())
        }
    }
}

/// True when the diff has no real content (whitespace-only or empty).
///
/// Per `feedback_empty_diff_is_a_crash`: a worker that returns nothing
/// is a deterministic failure. Foreman counts it against the model's
/// scorecard.
pub fn is_empty_diff(diff: &str) -> bool {
    diff.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn init_git_repo(path: &Path) {
        let run = |args: &[&str]| {
            StdCommand::new("git")
                .args(args)
                .current_dir(path)
                .output()
                .expect("git command failed")
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@test"]);
        run(&["config", "user.name", "test"]);
    }

    #[test]
    fn empty_diff_is_empty() {
        assert!(is_empty_diff(""));
        assert!(is_empty_diff("   \n\t"));
    }

    #[test]
    fn nonempty_diff_is_not_empty() {
        assert!(!is_empty_diff(
            "--- a/foo\n+++ b/foo\n@@ -1,1 +1,1 @@\n-a\n+b\n"
        ));
    }

    #[test]
    fn capture_diff_on_non_git_workspace_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let diff = capture_diff(tmp.path()).unwrap();
        assert!(is_empty_diff(&diff));
    }

    #[test]
    fn capture_diff_sees_unstaged_changes() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());

        // Commit a file so we have something to diff against.
        let file = tmp.path().join("hello.txt");
        std::fs::write(&file, "before\n").unwrap();
        StdCommand::new("git")
            .args(["add", "hello.txt"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        // Now make an unstaged change.
        std::fs::write(&file, "after\n").unwrap();

        let diff = capture_diff(tmp.path()).unwrap();
        assert!(!is_empty_diff(&diff));
        assert!(diff.contains("-before"));
        assert!(diff.contains("+after"));
    }

    #[test]
    fn capture_diff_clean_repo_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());

        let diff = capture_diff(tmp.path()).unwrap();
        assert!(is_empty_diff(&diff));
    }
}
