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
