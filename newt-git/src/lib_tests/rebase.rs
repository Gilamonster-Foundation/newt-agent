use super::*;

// --- rebase (structured plan) ------------------------------------------
/// A repo with three linear commits c1→c2→c3 (a/b/c.txt). Returns the dir
/// and the full oids [c1, c2, c3].
fn repo_with_three() -> (tempfile::TempDir, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    let mk = |name: &str, content: &str, msg: &str| {
        std::fs::write(p.join(name), content).unwrap();
        git(p, &["add", name]);
        git(
            p,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e.c",
                "commit",
                "-q",
                "-m",
                msg,
            ],
        );
    };
    mk("a.txt", "v1\n", "c1");
    mk("b.txt", "b\n", "c2");
    mk("c.txt", "c\n", "c3");
    let out = Command::new("git")
        .current_dir(p)
        .args(["log", "--format=%H", "--reverse"])
        .output()
        .unwrap();
    let oids = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(String::from)
        .collect();
    (dir, oids)
}
#[test]
fn rebase_rewords_a_middle_commit() {
    let (dir, oids) = repo_with_three();
    let t = tool(dir.path());
    let out = t
        .dispatch(
            "rebase",
            &serde_json::json!({
                "onto": oids[0],
                "plan": [
                    {"commit": oids[1], "action": "reword", "message": "b reworded"},
                    {"commit": oids[2], "action": "pick"},
                ]
            }),
            &GitCaveats::top(),
        )
        .unwrap();
    assert!(out.starts_with("rebased onto"), "got: {out}");
    assert_eq!(commit_count(dir.path()), 3, "same number of commits");
    // History: c1, b reworded, c3.
    let log = Command::new("git")
        .current_dir(dir.path())
        .args(["log", "--format=%s", "--reverse"])
        .output()
        .unwrap();
    let subjects = String::from_utf8_lossy(&log.stdout);
    assert!(subjects.contains("b reworded"), "got: {subjects}");
    assert!(
        !subjects.contains("\nc2\n"),
        "old c2 subject gone: {subjects}"
    );
    // b.txt and c.txt still present (changes preserved).
}
/// #1709 req 9: an ordinary `pick` (NOT reword/squash) — which replays the
/// original commit's message verbatim — receives canonical Newt attribution
/// too. Every newly created rebase commit is finalized through the same
/// finalizer as `commit`/`amend`. The user subject/body is preserved; the
/// Newt model trailer + Harness provenance are appended. Real-resource.
#[test]
fn rebase_pick_commit_receives_canonical_attribution() {
    let (dir, oids) = repo_with_three();
    let p = dir.path();
    let t = tool(p);
    // The picked commit (c3) was authored by "T <t@e.c>" with a bare
    // subject "c3" and NO attribution. Replay it with a plain `pick`.
    t.dispatch(
        "rebase",
        &serde_json::json!({
            "onto": oids[0],
            "plan": [
                {"commit": oids[1], "action": "pick"},
                {"commit": oids[2], "action": "pick"},
            ]
        }),
        &GitCaveats::top(),
    )
    .unwrap();
    // The HEAD commit (c3 replayed) now carries canonical attribution.
    let body = head_message(p);
    assert!(
        body.contains(" | Model: qwen3:30b | "),
        "pick commit received the live model provenance: {body}"
    );
    assert!(
        body.contains("Co-authored-by: qwen3:30b (newt-agent v"),
        "pick commit received the model Co-authored-by trailer: {body}"
    );
    // The user's original subject/body is preserved.
    let first_line = body.lines().next().unwrap_or("");
    assert_eq!(first_line, "c3", "pick preserved the user subject: {body}");
}
#[test]
fn rebase_squashes_two_commits_into_one() {
    let (dir, oids) = repo_with_three();
    let t = tool(dir.path());
    t.dispatch(
        "rebase",
        &serde_json::json!({
            "onto": oids[0],
            "plan": [
                {"commit": oids[1], "action": "pick"},
                {"commit": oids[2], "action": "squash", "message": "folded note"},
            ]
        }),
        &GitCaveats::top(),
    )
    .unwrap();
    // c1 + one squashed commit = 2.
    assert_eq!(commit_count(dir.path()), 2);
    // The squashed commit carries both messages.
    let body = head_message(dir.path());
    assert!(
        body.contains("c2") && body.contains("folded note"),
        "got: {body}"
    );
    // Both files landed in the squashed tree.
    let files = Command::new("git")
        .current_dir(dir.path())
        .args(["ls-tree", "--name-only", "-r", "HEAD"])
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&files.stdout);
    assert!(
        names.contains("b.txt") && names.contains("c.txt"),
        "got: {names}"
    );
}
#[test]
fn rebase_drops_a_commit() {
    let (dir, oids) = repo_with_three();
    let t = tool(dir.path());
    t.dispatch(
        "rebase",
        &serde_json::json!({
            "onto": oids[0],
            "plan": [
                {"commit": oids[1], "action": "pick"},
                {"commit": oids[2], "action": "drop"},
            ]
        }),
        &GitCaveats::top(),
    )
    .unwrap();
    assert_eq!(commit_count(dir.path()), 2);
    let names = Command::new("git")
        .current_dir(dir.path())
        .args(["ls-tree", "--name-only", "-r", "HEAD"])
        .output()
        .unwrap();
    let names = String::from_utf8_lossy(&names.stdout);
    assert!(
        !names.contains("c.txt"),
        "dropped commit's file gone: {names}"
    );
}
/// #1709 family: a rebase that produced ZERO commits (an all-drop plan) is
/// a successful history operation but NOT an attribution epoch. It must
/// NOT signal `commit_succeeded` and must NOT consume the contributor
/// snapshot — pending contributors survive it and a later commit in the
/// same lifecycle still credits them. Real git (tempdir + real commits)
/// because "the contributor survived onto the next commit" is a property
/// of the real commit object, not a mock.
#[test]
fn rebase_all_drop_preserves_pending_contributors() {
    let (dir, oids) = repo_with_three();
    let mut t = tool(dir.path());
    // Inject one accumulated contributor (model-a) into the envelope.
    if let Some(a) = t.attribution.as_mut() {
        a.contributors.push(
            newt_core::attribution::Attribution::new(
                "model-a",
                "newt-agent",
                newt_core::build_info::PACKAGE_VERSION,
                "noreply@newt-agent.com",
            )
            // Mirrors production: the session ledger stamps this build on
            // every contribution it records.
            .with_build(newt_core::build_info::SOURCE_ID),
        );
    }
    // All-drop plan: onto c1, drop c2 and c3 → produced == 0, dropped == 2.
    let out = t
        .dispatch(
            "rebase",
            &serde_json::json!({
                "onto": oids[0],
                "plan": [
                    {"commit": oids[1], "action": "drop"},
                    {"commit": oids[2], "action": "drop"},
                ]
            }),
            &GitCaveats::top(),
        )
        .unwrap();
    assert!(
        out.contains("0 commit(s), 2 dropped"),
        "all-drop rebase produced 0 commits: {out}"
    );
    // No Newt commit landed → no commit_succeeded signal.
    assert_eq!(
        t.drain_commit_success(),
        0,
        "a 0-produced rebase must NOT report commit_succeeded"
    );
    // The contributor cursor was NOT advanced: a subsequent commit in the
    // same lifecycle still credits model-a (the snapshot remains pending).
    std::fs::write(dir.path().join("d.txt"), "z\n").unwrap();
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["d.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "after all-drop rebase"}),
        &GitCaveats::top(),
    )
    .unwrap();
    let msg = head_message(dir.path());
    let version = newt_core::build_info::PACKAGE_VERSION;
    // The build revision is part of the contributor identity, so it
    // renders in the qualifier alongside the version.
    let build = newt_core::build_info::SOURCE_ID;
    assert!(
        msg.contains(&format!(
            "Co-authored-by: model-a (newt-agent v{version} {build}) <noreply@newt-agent.com>"
        )),
        "the pending contributor survived the 0-produced rebase and is credited on the next commit: {msg}"
    );
    // That next commit IS an epoch (it produced a commit) → it signals.
    assert_eq!(
        t.drain_commit_success(),
        1,
        "the commit after the all-drop rebase signals normally"
    );
}
#[test]
fn rebase_aborts_on_conflict_leaving_the_branch_unchanged() {
    // c1: a=v1; c2: a=v2; c3: a=v3. Cherry-picking c3 onto c1 conflicts
    // (both c1 and c3 changed a.txt from c2's base).
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    let mk = |content: &str, msg: &str| {
        std::fs::write(p.join("a.txt"), content).unwrap();
        git(p, &["add", "a.txt"]);
        git(
            p,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e.c",
                "commit",
                "-q",
                "-m",
                msg,
            ],
        );
    };
    mk("v1\n", "c1");
    mk("v2\n", "c2");
    mk("v3\n", "c3");
    let head_before = Command::new("git")
        .current_dir(p)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let oids: Vec<String> = String::from_utf8_lossy(
        &Command::new("git")
            .current_dir(p)
            .args(["log", "--format=%H", "--reverse"])
            .output()
            .unwrap()
            .stdout,
    )
    .lines()
    .map(String::from)
    .collect();
    let t = tool(p);
    let err = t
        .dispatch(
            "rebase",
            &serde_json::json!({
                "onto": oids[0],
                "plan": [{"commit": oids[2], "action": "pick"}]
            }),
            &GitCaveats::top(),
        )
        .unwrap_err();
    assert!(
        err.contains("conflict") && err.contains("aborted"),
        "got: {err}"
    );
    // The branch ref did NOT move.
    let head_after = Command::new("git")
        .current_dir(p)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        head_before.stdout, head_after.stdout,
        "branch must be unchanged"
    );
}
#[test]
fn rebase_denied_on_read_only() {
    let (dir, oids) = repo_with_three();
    let t = tool(dir.path());
    let err = t
        .dispatch(
            "rebase",
            &serde_json::json!({"onto": oids[0], "plan": [{"commit": oids[1], "action": "pick"}]}),
            &GitCaveats::read_only(),
        )
        .unwrap_err();
    assert!(
        err.contains("denied") && err.contains("commit"),
        "got: {err}"
    );
}
#[test]
fn local_git_tool_unknown_op_and_missing_args_error() {
    let dir = repo_with_commit();
    let t = tool(dir.path());
    let err = t
        .dispatch("frobnicate", &serde_json::json!({}), &GitCaveats::top())
        .unwrap_err();
    assert!(err.contains("unknown git op"), "got: {err}");
    // commit without a message is a clear arg error, not a panic.
    let err = t
        .dispatch("commit", &serde_json::json!({}), &GitCaveats::top())
        .unwrap_err();
    assert!(err.contains("message"), "got: {err}");
}
