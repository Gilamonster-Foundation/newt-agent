use super::*;

#[test]
fn local_git_tool_log_lists_commits() {
    let dir = repo_with_commit();
    let t = tool(dir.path());
    let out = t
        .dispatch("log", &serde_json::json!({"limit": 5}), &GitCaveats::top())
        .unwrap();
    assert!(out.contains("first commit"), "got: {out}");
}
#[test]
fn local_git_tool_add_then_commit_succeeds_when_permitted() {
    let dir = repo_with_commit();
    std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
    let t = tool(dir.path());
    let staged = t
        .dispatch(
            "add",
            &serde_json::json!({"paths": ["b.txt"]}),
            &GitCaveats::top(),
        )
        .unwrap();
    assert!(staged.contains("b.txt"), "got: {staged}");
    let committed = t
        .dispatch(
            "commit",
            &serde_json::json!({"message": "add b"}),
            &GitCaveats::top(),
        )
        .unwrap();
    assert!(committed.starts_with("committed "), "got: {committed}");
    assert!(committed.contains("add b"), "got: {committed}");
}
#[test]
fn local_git_tool_amend_rewords_head_without_adding_a_commit() {
    let dir = repo_with_commit();
    std::fs::write(dir.path().join("d.txt"), "d\n").unwrap();
    let t = tool(dir.path());
    t.dispatch(
        "add",
        &serde_json::json!({"paths": ["d.txt"]}),
        &GitCaveats::top(),
    )
    .unwrap();
    t.dispatch(
        "commit",
        &serde_json::json!({"message": "add d"}),
        &GitCaveats::top(),
    )
    .unwrap();
    let count_before = commit_count(dir.path());

    // Reword the last commit.
    let out = t
        .dispatch(
            "amend",
            &serde_json::json!({"message": "add d (reworded)"}),
            &GitCaveats::top(),
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
    let dir = repo_with_commit();
    let t = tool(dir.path());
    // Amend with no message → keep "first commit".
    t.dispatch("amend", &serde_json::json!({}), &GitCaveats::top())
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
        )
        .unwrap_err();
    assert!(
        err.contains("denied") && err.contains("commit"),
        "got: {err}"
    );
    // …but a read op is allowed under the same caveats.
    assert!(t
        .dispatch("status", &serde_json::json!({}), &GitCaveats::read_only())
        .is_ok());
}
