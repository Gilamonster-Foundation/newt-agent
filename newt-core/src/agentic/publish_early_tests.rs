use super::*;

/// #2831: the first eligible extraction nudges once per branch, not per turn.
#[test]
fn publish_early_once_per_branch_requires_observed_facts() {
    let mut state = State::default();
    assert!(state.observe("task", None, false, true).is_none());
    assert!(state
        .observe("task", Some(ExecOutcome::Failed), false, true)
        .is_none());
    assert!(state
        .observe("task", Some(ExecOutcome::Passed), false, false)
        .is_none());
    assert_eq!(state.observe("task", None, false, true), Some(HINT));
    assert!(state
        .observe("task", Some(ExecOutcome::Passed), false, true)
        .is_none());
    assert_eq!(
        state.observe("other", Some(ExecOutcome::Passed), false, true),
        Some(HINT)
    );
}

#[test]
fn publication_and_latest_check_suppress_the_hint() {
    let mut state = State::default();
    assert!(state
        .observe("task", Some(ExecOutcome::Passed), true, true)
        .is_none());
    assert!(state
        .observe("task", Some(ExecOutcome::Passed), false, true)
        .is_none());
    assert!(state
        .observe("failed", Some(ExecOutcome::Passed), false, false)
        .is_none());
    assert!(state
        .observe("failed", Some(ExecOutcome::Failed), false, true)
        .is_none());
    assert!(state.observe("failed", None, false, true).is_none());
    assert!(state
        .observe("failed", Some(ExecOutcome::Denied), false, true)
        .is_none());
}

/// The measured 6096-line copy gets advice, not refusal; ordinary extractions do not.
#[test]
fn bounded_move_threshold_is_advisory() {
    assert_eq!(bounded_move_hint(1500), "");
    assert!(bounded_move_hint(1501).contains("split"));
    assert!(bounded_move_hint(6096).contains("split"));
    assert!(!bounded_move_hint(6096).contains("denied"));
}

#[test]
fn cargo_evidence_excludes_echoes_compounds_and_other_commands() {
    let args = serde_json::json!({"cwd":"task"});
    for command in [
        "echo cargo check",
        "cargo check || true",
        "cargo check; echo ok",
        "cargo test",
        "cargo check --manifest-path elsewhere/Cargo.toml",
        "cargo check && false",
    ] {
        assert!(
            super::super::self_verify::cargo_check_directory(command, &args, "/repo").is_none(),
            "{command}"
        );
    }
    assert_eq!(
        super::super::self_verify::cargo_check_directory(
            "cargo check -p core --lib",
            &args,
            "/repo"
        ),
        Some(std::path::PathBuf::from("/repo/task"))
    );
}

#[test]
fn inherited_head_and_checkout_reflogs_are_not_task_commits() {
    let fields = format!(
        "{} {} Fixture <fixture@example.invalid> 1 +0000",
        "0".repeat(40),
        "a".repeat(40)
    );
    for message in [
        "reset: moving to HEAD",
        "checkout: moving from main to task",
        "branch: Created from HEAD",
        "echo commit: fabricated",
    ] {
        assert!(!commit_in_reflog(&format!("{fields}\t{message}\n")));
    }
    assert!(commit_in_reflog(&format!("{fields}\tcommit: extraction\n")));
    assert!(!commit_in_reflog("malformed\tcommit: extraction"));
    assert!(refactor_publication("Refactor in a WORKTREE and open a PR"));
    assert!(refactor_publication("refactor worktree pull request"));
    assert!(!refactor_publication("refactor worktree print results"));
    assert!(!refactor_publication("refactor in place and open PR"));
}

/// #2831: actual writes (including copy_from expansion) keep their bytes and succeed.
#[tokio::test]
async fn publish_early_large_write_advice_never_refuses() {
    let temp = tempfile::tempdir().unwrap();
    let content = "line\n".repeat(1501);
    std::fs::write(temp.path().join("source"), &content).unwrap();
    let caveats = crate::Caveats {
        fs_write: crate::Scope::only([temp.path().to_string_lossy().into_owned()]),
        ..crate::Caveats::top()
    };
    for (name, args) in [
        (
            "plain",
            serde_json::json!({"path":"plain", "content":content}),
        ),
        (
            "copied",
            serde_json::json!({"path":"copied", "content":"", "copy_from":{"path":"source","start_line":1,"end_line":1501}}),
        ),
    ] {
        let out = crate::execute_tool(
            "write_file",
            &args,
            temp.path().to_str().unwrap(),
            false,
            3,
            &caveats,
            &mut crate::NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(out.contains("Refactor hint:"), "{out}");
        assert_eq!(
            std::fs::read_to_string(temp.path().join(name))
                .unwrap()
                .lines()
                .count(),
            1501
        );
    }
}

/// Ground the wiring in reciprocal task metadata. The provider regression
/// separately grounds this fixture's HEAD/reflog spelling in actual Git.
#[test]
fn publish_early_observer_keeps_branch_facts_across_continuations() {
    use std::path::Path;
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("original");
    let task = temp.path().join("task");
    let admin = original.join(".git/worktrees/task");
    std::fs::create_dir_all(admin.join("logs")).unwrap();
    std::fs::create_dir_all(task.join("package")).unwrap();
    std::fs::write(task.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
    std::fs::write(admin.join("gitdir"), task.join(".git").to_str().unwrap()).unwrap();
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    std::fs::write(admin.join("HEAD"), "ref: refs/heads/task\n").unwrap();
    let commit = format!(
        "{} {} Fixture <fixture@example.invalid> 1 +0000\tcommit: extraction\n",
        "0".repeat(40),
        "a".repeat(40)
    );
    std::fs::write(admin.join("logs/HEAD"), &commit).unwrap();
    let read = crate::Scope::All;
    let session = crate::worktree_adoption::WorktreeSession::default();
    session.record_task_worktree(&task, "task");
    let check = serde_json::json!({"command":"cargo check", "cwd":task.join("package")});
    let objective = "Refactor in a worktree and open a PR";
    let observe = |session: &crate::worktree_adoption::WorktreeSession,
                   objective,
                   args: &serde_json::Value,
                   outcome,
                   pr| {
        after_tool(
            session,
            objective,
            "run_command",
            args,
            original.to_str().unwrap(),
            outcome,
            pr,
            &read,
            Some(task.as_path()),
        )
    };
    // A pass in the original checkout is not a pass in the task worktree.
    assert!(observe(
        &session,
        objective,
        &serde_json::json!({"command":"cargo check","cwd":original}),
        Some(ExecOutcome::Passed),
        None
    )
    .is_none());
    std::fs::write(admin.join("logs/HEAD"), "creation only").unwrap();
    assert!(observe(&session, objective, &check, Some(ExecOutcome::Passed), None).is_none());
    std::fs::write(admin.join("logs/HEAD"), &commit).unwrap();
    assert_eq!(
        observe(
            &session,
            "continue",
            &serde_json::json!({"command":"git status"}),
            Some(ExecOutcome::Passed),
            None
        ),
        Some(HINT)
    );
    assert!(observe(&session, objective, &check, Some(ExecOutcome::Passed), None).is_none());
    let published = crate::worktree_adoption::WorktreeSession::default();
    published.record_task_worktree(&task, "task");
    let receipt = crate::git_staging::Outcome::PrCreated {
        url: "https://github.com/example/project/pull/1".into(),
    };
    assert!(observe(
        &published,
        "Publish this branch",
        &serde_json::json!({"command":"gh pr create"}),
        Some(ExecOutcome::Passed),
        Some(&receipt)
    )
    .is_none());
    assert!(observe(
        &published,
        objective,
        &check,
        Some(ExecOutcome::Passed),
        None
    )
    .is_none());
    assert!(read_fact(Path::new("does-not-exist"), &crate::Scope::none()).is_none());
}
