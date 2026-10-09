//! Isolated native Git fixture (same contract as native_git_broker_pipeline).
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn git(root: &Path, args: &[&str]) -> String {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new("/usr/bin/git")
        .current_dir(root)
        .env_clear()
        .env("HOME", home.path())
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("GIT_TEMPLATE_DIR", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

pub fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let fixture = tempfile::tempdir().unwrap();
    // macOS /var is a symlink; grant the same canonical paths Git resolves.
    let root = fixture.path().canonicalize().unwrap();
    let repo = root.join("repo");
    let linked = root.join("linked");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("tracked"), "original\n").unwrap();
    git(&repo, &["add", "tracked"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--no-gpg-sign",
            "-qm",
            "fixture",
        ],
    );
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "task",
            linked.to_str().unwrap(),
        ],
    );
    (fixture, repo, linked)
}
