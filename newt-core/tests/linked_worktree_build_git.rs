//! Build confinement must retain validated linked-worktree Git reads without
//! widening access to sibling working files or shared Git writes.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;

use newt_core::caveats::{permits_path, Scope};
use newt_core::confined_exec::{build_tool_request, ConstrainedExecutor};
use newt_core::git_hardening::own_gitdir_grants;

#[path = "support/linked_git_fixture.rs"]
mod fixture_support;
use fixture_support::{fixture, git};

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

/// #2835: inherited Git pointers/config and PATH cannot redirect fixture setup
/// or run hooks against an outside repository. Poison only a child process.
#[test]
fn fixture_ignores_hostile_git_environment() {
    let (_outside, outside_repo, _) = fixture();
    let sentinel = outside_repo.join("tracked");
    let before: Vec<_> = ["tracked", ".git/HEAD", ".git/config", ".git/index"]
        .iter()
        .map(|p| std::fs::read(outside_repo.join(p)).unwrap())
        .collect();
    let hostile = tempfile::tempdir().unwrap();
    let injected = hostile.path().join("injected");
    let script = format!(
        "#!/bin/sh\nprintf poisoned > '{}'\nexit 1\n",
        injected.display()
    );
    use std::os::unix::fs::PermissionsExt;
    for name in ["git", "post-checkout"] {
        let path = hostile.path().join(name);
        std::fs::write(&path, &script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "hostile_environment_fixture_child",
            "--ignored",
            "--nocapture",
        ])
        .env("PATH", hostile.path())
        .env("HOME", hostile.path())
        .env("GIT_DIR", outside_repo.join(".git"))
        .env("GIT_WORK_TREE", &outside_repo)
        .env("GIT_INDEX_FILE", outside_repo.join(".git/index"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", hostile.path())
        .env("GIT_CONFIG_PARAMETERS", "invalid injected config")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "hermetic fixture failed: {output:?}"
    );
    let after: Vec<_> = ["tracked", ".git/HEAD", ".git/config", ".git/index"]
        .iter()
        .map(|p| std::fs::read(outside_repo.join(p)).unwrap())
        .collect();
    assert_eq!(before, after);
    assert_eq!(std::fs::read(sentinel).unwrap(), b"original\n");
    assert!(!injected.exists(), "inherited PATH/config executed a hook");
}

#[test]
#[ignore = "launched by fixture_ignores_hostile_git_environment with hostile child-only env"]
fn hostile_environment_fixture_child() {
    let (_fixture, repo, linked) = fixture();
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]),
        git(&linked, &["rev-parse", "HEAD"])
    );
}

/// #2835: matching rev-parse path strings do not authorize replacement objects
/// installed after the trusted session bind but before preparing a build.
#[test]
fn replacement_admin_is_not_rebound_at_build_time() {
    let (_fixture, repo, linked) = fixture();
    own_gitdir_grants(&linked);
    let admin = repo.join(".git/worktrees/linked");
    let saved = repo.join("saved-admin");
    std::fs::rename(&admin, &saved).unwrap();
    std::fs::create_dir(&admin).unwrap();
    for file in ["HEAD", "commondir", "gitdir"] {
        std::fs::copy(saved.join(file), admin.join(file)).unwrap();
    }
    assert_eq!(
        git(&linked, &["rev-parse", "--absolute-git-dir"]).trim(),
        admin.to_str().unwrap()
    );
    let request = build_tool_request(&linked, &linked, "/usr/bin/git", ["status"], &Scope::All);
    assert!(!permits_path(
        &request.caveats().fs_read,
        &admin.to_string_lossy()
    ));
    assert!(!permits_path(
        &request.caveats().fs_read,
        &repo.join(".git").to_string_lossy()
    ));
}
