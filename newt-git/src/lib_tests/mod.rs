use super::*;
use std::path::Path;
use std::process::Command;

/// A `git` invocation with the developer's ambient environment removed.
///
/// Every real-git call in `lib_tests` MUST be built here. A test that inherits
/// configuration it did not ask for does not merely fail, it *misattributes*:
/// it reports a defect in whatever branch happened to be under test (#2221 —
/// `tag.gpgsign = true` turned a bare `git tag` into an annotated signed tag,
/// which needs an editor and fails headless, and three innocent recovery
/// branches were nearly written up as broken because of it).
///
/// `current_dir` alone does NOT protect you: the `GIT_DIR` family points git at
/// another repo regardless of cwd, which is exactly what happens when this
/// suite runs under a git hook.
fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        // Ambient config: global and system are the ones a human edits.
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // Identity, so scrubbing global config cannot leave `commit` with none.
        // These override config, so call sites need no `-c user.name=...`.
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        // Nothing may block on a human.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env_remove("EDITOR")
        .env_remove("GIT_EDITOR")
        .env_remove("VISUAL")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS");
    // The GIT_DIR family: inherited whenever the suite runs under a hook.
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_CEILING_DIRECTORIES",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

fn git(dir: &Path, args: &[&str]) {
    let ok = git_cmd(dir)
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
    git(p, &["commit", "-q", "-m", "first commit"]);
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
    let out = git_cmd(dir)
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

fn head_message(dir: &Path) -> String {
    let out = git_cmd(dir)
        .args(["log", "-1", "--pretty=%B"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[cfg(test)]
mod ambient_environment {
    use super::*;

    /// Regression for #2221: the real-git fixtures inherited the developer's
    /// git environment. With `tag.gpgsign = true` a bare `git tag <name>`
    /// becomes an annotated *signed* tag, which needs an editor and fails
    /// headless. CI never saw it (clean runner config), so the suite reported
    /// a defect in whatever branch happened to be under test — three innocent
    /// recovery branches were nearly written up as broken on exactly this.
    ///
    /// Asserted as a differential: same repo, same command, the scrub is the
    /// only variable. The unscrubbed probe is what keeps this from going
    /// vacuously green if the hazard ever stops reproducing.
    #[test]
    fn git_cmd_defeats_a_poisoned_global_config() {
        let dir = repo_with_commit();
        let p = dir.path();

        let poison = p.join("poisoned.gitconfig");
        std::fs::write(&poison, "[tag]\n\tgpgsign = true\n").unwrap();

        // The hazard is live: an unscrubbed git honours the poisoned config.
        let seen = Command::new("git")
            .current_dir(p)
            .env("GIT_CONFIG_GLOBAL", &poison)
            .args(["config", "--get", "tag.gpgsign"])
            .output()
            .expect("git runs");
        assert_eq!(
            String::from_utf8_lossy(&seen.stdout).trim(),
            "true",
            "fixture no longer reproduces #2221; this test would be vacuous"
        );

        // Unscrubbed, that config changes what `git tag` builds: it either
        // refuses for want of an editor, or writes an annotated tag object.
        let unscrubbed = Command::new("git")
            .current_dir(p)
            .env("GIT_CONFIG_GLOBAL", &poison)
            .env_remove("EDITOR")
            .env_remove("GIT_EDITOR")
            .env_remove("VISUAL")
            .args(["tag", "poisoned"])
            .status()
            .expect("git runs");
        if unscrubbed.success() {
            assert_eq!(
                object_type(p, "refs/tags/poisoned"),
                "tag",
                "poisoned config should have produced an annotated tag"
            );
        }

        // The fixture helper must be immune to all of it.
        git(p, &["tag", "clean"]);
        assert_eq!(
            object_type(p, "refs/tags/clean"),
            "commit",
            "ambient config leaked into the fixture helper"
        );
    }

    /// The `GIT_DIR` family points git at another repository regardless of
    /// `current_dir`, which is what happens when this suite runs under a git
    /// hook. `git -C` does not protect you; only removing the vars does.
    #[test]
    fn git_cmd_removes_the_git_dir_family() {
        let dir = repo_with_commit();
        let envs: std::collections::HashMap<_, _> = git_cmd(dir.path())
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();

        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
        ] {
            assert_eq!(
                envs.get(var),
                Some(&None),
                "{var} must be removed, not merely left inherited"
            );
        }
        assert_eq!(
            envs.get("GIT_CONFIG_GLOBAL").and_then(|v| v.as_deref()),
            Some("/dev/null"),
            "global config must be pinned away from the developer's ~/.gitconfig"
        );
    }

    fn object_type(dir: &Path, refname: &str) -> String {
        let out = git_cmd(dir)
            .args(["cat-file", "-t", refname])
            .output()
            .expect("git runs");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
}

// Families beside this file. Both attributes are required: rustc needs only
// the `#[path]`, but the ratchets' shared scanner resolves a child ONLY when
// a `#[cfg(test)]` immediately precedes the `mod` (#2149).
#[cfg(test)]
#[path = "attribution.rs"]
mod attribution;
#[cfg(test)]
#[path = "branch_list.rs"]
mod branch_list;
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
#[path = "git_scope.rs"]
mod git_scope;
#[cfg(test)]
#[path = "rebase.rs"]
mod rebase;
#[cfg(test)]
#[path = "scoped_legacy.rs"]
mod scoped_legacy;
#[cfg(test)]
#[path = "stash_and_unknown.rs"]
mod stash_and_unknown;
#[cfg(test)]
#[path = "tool_dispatch.rs"]
mod tool_dispatch;
#[cfg(test)]
#[path = "tool_surface.rs"]
mod tool_surface;
