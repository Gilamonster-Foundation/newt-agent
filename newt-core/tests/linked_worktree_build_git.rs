//! Build confinement must retain validated linked-worktree Git reads without
//! widening access to sibling working files or shared Git writes.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::process::Command;

use newt_core::caveats::{permits_path, Scope};
use newt_core::confined_exec::{build_tool_request, ConstrainedExecutor};
use newt_core::git_hardening::own_gitdir_grants;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
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

fn confined(root: &Path, program: &str, args: &[&str]) -> newt_core::confined_exec::ConfinedOutput {
    let request = build_tool_request(root, root, program, args.iter().copied(), &Scope::All)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    let output =
        ConstrainedExecutor::run(&request).expect("native filesystem confinement must spawn");
    #[cfg(target_os = "linux")]
    assert_eq!(output.sandbox_kind, agent_bridle::SandboxKind::Landlock);
    #[cfg(target_os = "macos")]
    assert_eq!(output.sandbox_kind, agent_bridle::SandboxKind::Seatbelt);
    output
}

/// G2 regression: Git succeeds before and after a build-fence read; the fence
/// must not disguise valid metadata as a corrupt repository (Linux/Seatbelt).
#[test]
fn build_fence_keeps_linked_worktree_git_metadata_readable() {
    let (_fixture, repo, linked) = fixture();
    // Same trusted bind as session startup; dispatch must only revalidate it.
    own_gitdir_grants(&linked);
    let expected = git(&linked, &["rev-parse", "HEAD"]);
    let admin = repo.join(".git/worktrees/linked");
    let head = std::fs::read(admin.join("HEAD")).unwrap();
    let common = std::fs::read(admin.join("commondir")).unwrap();
    for root in [&repo, &linked] {
        let output = confined(root, "git", &["rev-parse", "HEAD"]);
        assert_eq!(std::fs::read(admin.join("HEAD")).unwrap(), head);
        assert_eq!(std::fs::read(admin.join("commondir")).unwrap(), common);
        assert_eq!(git(&linked, &["rev-parse", "HEAD"]), expected);
        assert!(
            output.success,
            "build fence hid valid metadata: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }
    let output = confined(&linked, "git", &["show", "HEAD:tracked"]);
    assert!(output.success, "{output:?}");
    assert_eq!(output.stdout, b"original\n");
}

/// The metadata read fix must not expose the parent checkout, another linked
/// worktree's files, or parent files, nor grant any new Git-directory writes.
#[test]
fn linked_build_keeps_siblings_and_git_writes_outside_the_fence() {
    let (_fixture, repo, linked) = fixture();
    let sibling = repo.parent().unwrap().join("sibling");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "other",
            sibling.to_str().unwrap(),
        ],
    );
    let parent_file = repo.parent().unwrap().join("private");
    std::fs::write(&parent_file, "private parent\n").unwrap();
    own_gitdir_grants(&linked);
    let request = build_tool_request(&linked, &linked, "git", ["status"], &Scope::All);
    std::fs::write(linked.join("tracked"), "pending work\n").unwrap();
    let stash = confined(&linked, "git", &["stash", "push"]);
    assert!(!stash.success, "stash must not gain shared Git writes");
    assert!(!String::from_utf8_lossy(&stash.stderr).contains("not a git repository"));
    assert_eq!(
        std::fs::read(linked.join("tracked")).unwrap(),
        b"pending work\n"
    );
    assert!(!repo.join(".git/refs/stash").exists());
    for path in [repo.join("tracked"), sibling.join("tracked"), parent_file] {
        assert!(!permits_path(
            &request.caveats().fs_read,
            &path.to_string_lossy()
        ));
        let output = confined(&linked, "cat", &[path.to_str().unwrap()]);
        assert!(!output.success, "outside read succeeded: {path:?}");
        assert!(output.stdout.is_empty());
    }
    for path in [
        repo.join(".git/refs/heads/main"),
        repo.join(".git/worktrees/linked/HEAD"),
    ] {
        assert!(!permits_path(
            &request.caveats().fs_write,
            &path.to_string_lossy()
        ));
        let before = std::fs::read(&path).unwrap();
        let output = confined(
            &linked,
            "sh",
            &[
                "-c",
                "printf changed > \"$1\"",
                "write-probe",
                path.to_str().unwrap(),
            ],
        );
        assert!(!output.success, "outside write succeeded: {path:?}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

/// A model-rewritten .git pointer must not mint read authority for a different
/// repository. An unbound worktree must not acquire that authority either.
#[test]
fn build_reads_require_the_original_validated_git_identity() {
    let (_fixture, repo, linked) = fixture();
    let request = build_tool_request(&linked, &linked, "git", ["status"], &Scope::All);
    assert!(!permits_path(
        &request.caveats().fs_read,
        &repo.join(".git").to_string_lossy()
    ));
    own_gitdir_grants(&linked);
    let (_other_fixture, other_repo, other_linked) = fixture();
    std::fs::copy(other_linked.join(".git"), linked.join(".git")).unwrap();
    let request = build_tool_request(&linked, &linked, "git", ["status"], &Scope::All);
    for admin in [repo.join(".git"), other_repo.join(".git")] {
        assert!(!permits_path(
            &request.caveats().fs_read,
            &admin.to_string_lossy()
        ));
    }
    assert!(!confined(&linked, "git", &["rev-parse", "HEAD"]).success);
}
