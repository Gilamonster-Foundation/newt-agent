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
///
/// We explicitly clear `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE`
/// inherited from the parent process so that callers invoked from
/// inside a git hook (e.g. a worker spawned during `git push`'s
/// pre-push) don't accidentally diff the *hook's* repo instead of
/// the workspace passed in.
pub fn capture_diff(workspace: &Path) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(["diff", "--no-color"])
        .current_dir(workspace)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_PREFIX")
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
        // Clear inherited git env vars so the test's `git init` is
        // scoped to `path` and doesn't accidentally mutate the outer
        // worktree when the test runs from a git hook (e.g. pre-push).
        let run = |args: &[&str]| {
            StdCommand::new("git")
                .args(args)
                .current_dir(path)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .env_remove("GIT_COMMON_DIR")
                .env_remove("GIT_PREFIX")
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
        // Inherited GIT_DIR etc. are cleared so the add/commit land in
        // `tmp`, not the outer worktree.
        StdCommand::new("git")
            .args(["add", "hello.txt"])
            .current_dir(tmp.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_PREFIX")
            .output()
            .unwrap();
        StdCommand::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(tmp.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_PREFIX")
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

    /// Regression: when called from inside a git hook (which sets
    /// `GIT_DIR`, `GIT_WORK_TREE`, etc. in the child env), `capture_diff`
    /// used to inherit those vars and diff the *hook's* repo instead of
    /// the workspace passed in. We now strip them explicitly. Before the
    /// fix this test would return a non-empty diff (the parent repo's
    /// working-tree noise).
    #[test]
    fn capture_diff_ignores_inherited_git_env() {
        use std::sync::Mutex;
        // Setting env vars races across parallel tests; serialize this one.
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();

        let tmp = tempfile::tempdir().unwrap();
        init_git_repo(tmp.path());

        // Simulate the hook environment: point GIT_DIR at a bogus path.
        // Without env_remove() in capture_diff(), git would either error
        // or diff something other than tmp.path().
        let prev_git_dir = std::env::var_os("GIT_DIR");
        let prev_git_work_tree = std::env::var_os("GIT_WORK_TREE");
        // SAFETY: serialized by ENV_LOCK above; no other thread reads/writes
        // these vars while we hold the guard.
        unsafe {
            std::env::set_var("GIT_DIR", "/nonexistent/git/dir");
            std::env::set_var("GIT_WORK_TREE", "/nonexistent/work/tree");
        }

        let diff = capture_diff(tmp.path()).unwrap();

        // Restore.
        unsafe {
            match prev_git_dir {
                Some(v) => std::env::set_var("GIT_DIR", v),
                None => std::env::remove_var("GIT_DIR"),
            }
            match prev_git_work_tree {
                Some(v) => std::env::set_var("GIT_WORK_TREE", v),
                None => std::env::remove_var("GIT_WORK_TREE"),
            }
        }

        assert!(
            is_empty_diff(&diff),
            "capture_diff must clear inherited GIT_DIR/GIT_WORK_TREE; got: {diff:?}"
        );
    }
}
