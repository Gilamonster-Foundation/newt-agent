use super::*;

/// Real linked worktrees ground per-call target selection and attribution:
/// a cwd argument must never silently stage or commit the source checkout.
#[test]
fn cwd_stages_and_commits_only_the_selected_worktree() {
    let repo = repo_with_commit();
    let cwd = ".worktrees/task";
    git(repo.path(), &["worktree", "add", "-b", "task", cwd]);
    let target = repo.path().join(cwd);
    let source_head = std::fs::read(repo.path().join(".git/refs/heads/main")).unwrap();
    let source_index = std::fs::read(repo.path().join(".git/index")).unwrap();
    std::fs::write(target.join("task.txt"), "task-only\n").unwrap();
    let t = tool(repo.path());
    for (op, args) in [
        (
            "add",
            serde_json::json!({"cwd": cwd, "paths": ["task.txt"]}),
        ),
        (
            "commit",
            serde_json::json!({"cwd": cwd, "message": "implement task"}),
        ),
    ] {
        t.dispatch(op, &args, &GitCaveats::top(), &Caveats::top())
            .unwrap();
    }
    let message = git_cmd(&target)
        .args(["log", "-1", "--pretty=%B"])
        .output()
        .unwrap();
    assert!(message.status.success());
    let message = String::from_utf8_lossy(&message.stdout);
    assert!(message.contains("implement task"), "{message}");
    assert!(message.contains("Co-authored-by: qwen3:30b"), "{message}");
    assert_eq!(
        std::fs::read(repo.path().join(".git/refs/heads/main")).unwrap(),
        source_head
    );
    assert_eq!(
        std::fs::read(repo.path().join(".git/index")).unwrap(),
        source_index
    );
    assert!(!repo.path().join("task.txt").exists());
    assert_eq!(t.drain_commit_success(), 1);
}

#[test]
fn cwd_rejects_escapes_and_non_repository_directories_before_git_writes() {
    let repo = repo_with_commit();
    let outside = repo_with_commit();
    std::fs::create_dir(repo.path().join("empty")).unwrap();
    let t = tool(repo.path());
    for cwd in ["../outside", outside.path().to_str().unwrap(), "empty", ""] {
        let out = t.dispatch(
            "branch",
            &serde_json::json!({"cwd": cwd, "name": "wrong-target"}),
            &GitCaveats::top(),
            &Caveats::top(),
        );
        assert!(out.is_err(), "invalid cwd {cwd:?} was accepted: {out:?}");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), repo.path().join("escape")).unwrap();
        assert!(t
            .dispatch(
                "branch-list",
                &serde_json::json!({"cwd": "escape"}),
                &GitCaveats::top(),
                &Caveats::top()
            )
            .is_err());
    }
    assert!(!repo.path().join(".git/refs/heads/wrong-target").exists());
    assert!(!outside.path().join(".git/refs/heads/wrong-target").exists());
}

#[test]
fn cwd_preserves_scoped_read_and_write_boundaries() {
    let repo = repo_with_commit();
    git(
        repo.path(),
        &["worktree", "add", "-b", "task", ".worktrees/task"],
    );
    let t = tool(repo.path());
    let args = serde_json::json!({"cwd": ".worktrees/task", "scope": "local"});
    let mut session = Caveats::top();
    session.fs_read = Scope::only([repo.path().to_string_lossy().into_owned()]);
    assert!(t
        .dispatch("branch-list", &args, &GitCaveats::read_only(), &session)
        .is_ok());
    assert!(t
        .dispatch(
            "commit",
            &serde_json::json!({"cwd": ".worktrees/task", "message": "no bypass"}),
            &GitCaveats::top(),
            &session
        )
        .unwrap_err()
        .contains("scoped fs_read"));
    session.fs_read = Scope::All;
    session.fs_write = Scope::none();
    assert!(t
        .dispatch(
            "branch",
            &serde_json::json!({"cwd": ".worktrees/task", "name": "denied"}),
            &GitCaveats::top(),
            &session
        )
        .is_err());
    assert!(!repo.path().join(".git/refs/heads/denied").exists());
}

/// F32/#2537, PR #2577 round 2 item 3/4: the PRODUCTION shape — session root
/// IS a linked worktree living OUTSIDE the main checkout's directory tree
/// (e.g. `~/workspaces/.worktrees/<lane>` vs `~/workspaces/newt-agent`), with
/// a session `Caveats` built exactly the way `apply_cli_fs_grants` builds it
/// (`fs_write` = the worktree only; `fs_read` = the worktree PLUS
/// `own_gitdir_grants`'s read roots — the metadata is read-only). No `cwd`
/// is needed because `root` already IS the worktree. Would have failed
/// before the item-3 fix: the old whole-directory
/// `permits_path(write_scope, git_dir/common_dir)` check always failed here,
/// since `fs_write` deliberately never grants those directories.
#[test]
fn own_branch_linked_worktree_commit_succeeds_under_a_production_shaped_session() {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    std::fs::create_dir(&main).unwrap();
    git(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("seed"), "x").unwrap();
    git(&main, &["add", "seed"]);
    git(&main, &["commit", "-q", "-m", "init"]);
    let worktree = root.path().join("wt");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            worktree.to_str().unwrap(),
            "-b",
            "task",
        ],
    );

    let grants = newt_core::git_hardening::own_gitdir_grants(&worktree);
    assert!(
        !grants.read.is_empty(),
        "the worktree's own gitdir must be a read grant"
    );
    // `fs_read` stays `All`: the legacy engine's ordinary (non-`branch-list`)
    // ops refuse a bounded read scope outright (`check_git_read_scope`) —
    // that is a SEPARATE, orthogonal gate from the `fs_write` fix under test
    // here. `own_gitdir_grants().read` is exercised directly by
    // `newt-core::caveats::own_gitdir_grants_are_read_only_in_session_caveats`.
    let mut session = Caveats::top();
    session.fs_write = Scope::only([worktree.to_string_lossy().into_owned()]);

    std::fs::write(worktree.join("f.txt"), "hi\n").unwrap();
    let t = tool(&worktree);
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["f.txt"]}),
        &GitCaveats::top(),
        &session,
    )
    .unwrap();
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "task work"}),
        &GitCaveats::top(),
        &session,
    )
    .unwrap();
    let message = git_cmd(&worktree)
        .args(["log", "-1", "--pretty=%B"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&message.stdout).contains("task work"));
}

/// F32/#2537, PR #2577 round 3 item 1: round 2's "cwd always resolves inside
/// `self.root`, so it must be the session's own repo" reasoning was false —
/// a NESTED linked worktree's `git_dir`/`common_dir` can point ANYWHERE on
/// disk via its `commondir` file, entirely independent of where the worktree
/// directory itself sits. Here the session workspace `root` is a plain
/// (non-repo) directory containing a linked worktree of a DIFFERENT repo that
/// lives OUTSIDE the workspace entirely — `cwd` resolves inside `root` (so
/// `checked_dispatch_root` admits it), but the repo it opens is neither
/// write-granted nor the session's own repo. Must be refused. Would have
/// failed before this fix: round 2's code granted this unconditionally.
#[test]
fn nested_worktree_of_a_foreign_repo_is_refused_even_though_cwd_is_inside_root() {
    let root = tempfile::tempdir().unwrap();
    let foreign = tempfile::tempdir().unwrap(); // OUTSIDE root entirely
    git(foreign.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(foreign.path().join("seed"), "x").unwrap();
    git(foreign.path(), &["add", "seed"]);
    git(foreign.path(), &["commit", "-q", "-m", "init"]);
    let nested = root.path().join("nested");
    git(
        foreign.path(),
        &[
            "worktree",
            "add",
            "-q",
            nested.to_str().unwrap(),
            "-b",
            "task",
        ],
    );

    // `root` itself is not a repo at all — the session workspace need not be
    // one — so it can never be "the same repo" as `nested`.
    let t = tool(root.path());
    let mut session = Caveats::top();
    session.fs_write = Scope::only([root.path().to_string_lossy().into_owned()]);
    std::fs::write(nested.join("f.txt"), "x\n").unwrap();
    let err = t
        .dispatch(
            "add",
            &serde_json::json!({"cwd": "nested", "paths": ["f.txt"]}),
            &GitCaveats::top(),
            &session,
        )
        .unwrap_err();
    assert!(err.contains("capability denied"), "got: {err}");
}
