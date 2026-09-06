use super::*;

#[test]
fn stash_push_resets_worktree_and_pop_restores() {
    // Regression (#992): pure-Rust `git stash` — push saves tracked changes
    // (worktree back to HEAD) + lists them; pop restores + drops the entry.
    let dir = repo_with_commit();
    let p = dir.path();
    std::fs::write(p.join("a.txt"), "changed\n").unwrap(); // dirty a tracked file
    let eng = GitEngine::open(p).unwrap();
    let author = Author {
        name: "T".into(),
        email: "t@e.x".into(),
    };
    let out = eng.stash_push(&GitCaveats::top(), &author).unwrap();
    assert!(out.contains("Saved working directory"), "got: {out}");
    assert_eq!(
        std::fs::read_to_string(p.join("a.txt")).unwrap(),
        "hello\n",
        "worktree reset to HEAD after push"
    );
    let list = eng.stash_list(&GitCaveats::top()).unwrap();
    assert_eq!(list.len(), 1, "one stash entry: {list:?}");
    assert!(list[0].starts_with("stash@{0}:"), "{}", list[0]);

    let out = eng.stash_pop(&GitCaveats::top(), 0).unwrap();
    assert!(out.contains("popped"), "got: {out}");
    assert_eq!(
        std::fs::read_to_string(p.join("a.txt")).unwrap(),
        "changed\n",
        "pop restored the stashed change"
    );
    assert!(
        eng.stash_list(&GitCaveats::top()).unwrap().is_empty(),
        "entry dropped after a clean pop"
    );
}
#[test]
fn stash_is_a_known_op_and_write_gated() {
    // Regression (#992): `git: stash` was "unknown git op 'stash'".
    let dir = repo_with_commit();
    let p = dir.path();
    let t = tool(p);
    let out = t
        .dispatch("stash-list", &serde_json::json!({}), &GitCaveats::top())
        .unwrap();
    assert!(
        !out.contains("unknown git op"),
        "stash-list recognized: {out}"
    );
    // Push is a write → denied under read-only caps (fail-closed like commit).
    std::fs::write(p.join("a.txt"), "dirty\n").unwrap();
    let err = t
        .dispatch("stash", &serde_json::json!({}), &GitCaveats::read_only())
        .unwrap_err();
    assert!(
        err.contains("not permitted"),
        "read-only denies stash push: {err}"
    );
}
