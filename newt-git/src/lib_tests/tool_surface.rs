use super::*;

#[test]
fn local_git_tool_log_lists_commits() {
    let dir = repo_with_commit();
    let t = tool(dir.path());
    let out = t
        .dispatch(
            "log",
            &serde_json::json!({"limit": 5}),
            &GitCaveats::top(),
            &newt_core::caveats::Caveats::top(),
        )
        .unwrap();
    assert!(out.contains("first commit"), "got: {out}");
}
#[test]
fn local_git_tool_add_then_commit_succeeds_when_permitted() {
    let dir = repo_with_commit_on_task_branch();
    std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
    let t = tool(dir.path());
    let staged = t
        .dispatch(
            "add",
            &serde_json::json!({"paths": ["b.txt"]}),
            &GitCaveats::top(),
            &newt_core::caveats::Caveats::top(),
        )
        .unwrap();
    assert!(staged.contains("b.txt"), "got: {staged}");
    let committed = t
        .dispatch(
            "commit",
            &serde_json::json!({"message": "add b"}),
            &GitCaveats::top(),
            &newt_core::caveats::Caveats::top(),
        )
        .unwrap();
    assert!(committed.starts_with("committed "), "got: {committed}");
    assert!(committed.contains("add b"), "got: {committed}");
}
#[test]
fn local_git_tool_amend_rewords_head_without_adding_a_commit() {
    let dir = repo_with_commit_on_task_branch();
    std::fs::write(dir.path().join("d.txt"), "d\n").unwrap();
    let t = tool(dir.path());
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["d.txt"]}),
        &GitCaveats::top(),
        &newt_core::caveats::Caveats::top(),
    )
    .unwrap();
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "add d"}),
        &GitCaveats::top(),
        &newt_core::caveats::Caveats::top(),
    )
    .unwrap();
    let count_before = commit_count(dir.path());

    // Reword the last commit.
    let out = t
        .dispatch(
            "amend",
            &serde_json::json!({"message": "add d (reworded)"}),
            &GitCaveats::top(),
            &newt_core::caveats::Caveats::top(),
        )
        .unwrap();
    assert!(out.starts_with("amended "), "got: {out}");
    // Same number of commits (HEAD replaced, not stacked).
    assert_eq!(commit_count(dir.path()), count_before);
    // The new subject is in HEAD.
    let body = head_message(dir.path());
    assert!(body.contains("add d (reworded)"), "got: {body}");
    assert!(
        body.contains("Co-authored-by: qwen3:30b"),
        "amend re-signs the new message: {body}"
    );
}
#[test]
fn local_git_tool_amend_keeps_message_when_omitted() {
    let dir = repo_with_commit_on_task_branch();
    let t = tool(dir.path());
    // Amend with no message → keep "first commit".
    t.dispatch(
        "amend",
        &serde_json::json!({}),
        &GitCaveats::top(),
        &newt_core::caveats::Caveats::top(),
    )
    .unwrap();
    assert!(head_message(dir.path()).contains("first commit"));
}
#[test]
fn local_git_tool_amend_denied_on_read_only() {
    let dir = repo_with_commit();
    let t = tool(dir.path());
    let err = t
        .dispatch(
            "amend",
            &serde_json::json!({"message": "x"}),
            &GitCaveats::read_only(),
            &newt_core::caveats::Caveats::top(),
        )
        .unwrap_err();
    assert!(
        err.contains("denied") && err.contains("commit"),
        "got: {err}"
    );
}
#[test]
fn local_git_tool_commit_denied_on_read_only_caveats() {
    let dir = repo_with_commit();
    let t = tool(dir.path());
    // read_only permits status/log/diff but never a commit.
    let err = t
        .dispatch(
            "commit",
            &serde_json::json!({"message": "nope"}),
            &GitCaveats::read_only(),
            &newt_core::caveats::Caveats::top(),
        )
        .unwrap_err();
    assert!(
        err.contains("denied") && err.contains("commit"),
        "got: {err}"
    );
    // …but a read op is allowed under the same caveats.
    assert!(t
        .dispatch(
            "status",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &newt_core::caveats::Caveats::top()
        )
        .is_ok());
}

/// F32/#2537, PR #2577 round 2: the `git` tool is the REAL commit path (a
/// shell `git commit` is refused and redirected to it, and this in-process
/// tool reads/writes refs directly — no session `fs_write` grant gates it).
/// So the invariant "newt never moves the default branch" has to be enforced
/// here, in `GitEngine`/`refuse_if_default_branch`, not by withholding a
/// filesystem grant. Would have failed before that guard existed: a commit
/// on `main` landed with no refusal at all.
#[test]
fn commit_on_the_default_branch_is_refused_and_leaves_the_ref_unmoved() {
    let dir = repo_with_commit(); // committed on `main` by the fixture
    let before = std::fs::read(dir.path().join(".git/refs/heads/main")).unwrap();
    std::fs::write(dir.path().join("b.txt"), "x\n").unwrap();
    let t = tool(dir.path());
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["b.txt"]}),
        &GitCaveats::top(),
        &newt_core::caveats::Caveats::top(),
    )
    .unwrap();
    let err = t
        .dispatch(
            "commit",
            &serde_json::json!({"message": "should not land"}),
            &GitCaveats::top(),
            &newt_core::caveats::Caveats::top(),
        )
        .unwrap_err();
    assert!(err.contains("default branch"), "got: {err}");
    assert_eq!(
        std::fs::read(dir.path().join(".git/refs/heads/main")).unwrap(),
        before,
        "refs/heads/main must be unchanged by the refused commit"
    );
}

/// The same invariant for `amend`, with the attempted retarget explicit: a
/// model that somehow moved `HEAD` to `refs/heads/main` (the fence no longer
/// grants writing `HEAD` at all — see `caveats::apply_cli_fs_grants` — but
/// the engine-level guard is the backstop if it ever did) still cannot amend
/// there.
#[test]
fn amend_on_the_default_branch_is_refused_and_leaves_the_ref_unmoved() {
    let dir = repo_with_commit();
    let before = std::fs::read(dir.path().join(".git/refs/heads/main")).unwrap();
    let t = tool(dir.path());
    let err = t
        .dispatch(
            "amend",
            &serde_json::json!({"message": "should not land"}),
            &GitCaveats::top(),
            &newt_core::caveats::Caveats::top(),
        )
        .unwrap_err();
    assert!(err.contains("default branch"), "got: {err}");
    assert_eq!(
        std::fs::read(dir.path().join(".git/refs/heads/main")).unwrap(),
        before
    );
}

/// PR #2577 round 4, Blocker 2 red test 1: `branch-delete main` from a
/// session on `task` must be refused — `branch_delete` was unguarded before
/// this round (it refused only the CURRENT branch), so this alone was a
/// default-branch ref move the decision forbids.
#[test]
fn branch_delete_of_the_default_branch_is_refused() {
    let dir = repo_with_commit(); // one commit on `main`
    git(dir.path(), &["checkout", "-q", "-b", "task"]);
    let t = tool(dir.path());
    let err = t
        .dispatch(
            "branch-delete",
            &serde_json::json!({"name": "main"}),
            &GitCaveats::top(),
            &newt_core::caveats::Caveats::top(),
        )
        .unwrap_err();
    assert!(err.contains("default branch"), "got: {err}");
    assert!(
        git_cmd(dir.path())
            .args(["rev-parse", "--verify", "refs/heads/main"])
            .output()
            .unwrap()
            .status
            .success(),
        "refs/heads/main must survive the refused delete"
    );
}

/// PR #2577 round 4, Blocker 2 red test 2: the bypass the fix closes. Delete
/// `main` OUT OF BAND (real git, standing in for whatever else could remove
/// it), point `HEAD` at the now-unborn `refs/heads/main`, and confirm a
/// `git`-tool commit still refuses — the OLD per-ref "is THIS branch unborn"
/// carve-out would have let this land as a brand-new root commit on `main`,
/// discarding `task`'s history with the guard never firing. `task`'s ref
/// surviving is exactly what `repository_has_no_refs` now checks for.
#[test]
fn commit_onto_a_deleted_and_recreated_default_branch_is_still_refused() {
    let dir = repo_with_commit(); // one commit on `main`
    git(dir.path(), &["checkout", "-q", "-b", "task"]);
    git(dir.path(), &["branch", "-D", "main"]); // out of band — task survives
                                                // Point HEAD at the now-unborn `main`, matching what `write_file HEAD`
                                                // could do if the fence still granted it (it does not — see
                                                // `caveats::apply_cli_fs_grants` — this is the engine-level backstop).
    std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

    let t = tool(dir.path());
    let err = t
        .dispatch(
            "commit",
            &serde_json::json!({"message": "should not land"}),
            &GitCaveats::top(),
            &newt_core::caveats::Caveats::top(),
        )
        .unwrap_err();
    assert!(err.contains("default branch"), "got: {err}");
    assert!(
        !git_cmd(dir.path())
            .args(["rev-parse", "--verify", "refs/heads/main"])
            .output()
            .unwrap()
            .status
            .success(),
        "sanity: main really is gone before the refused commit"
    );
}
