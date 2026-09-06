use super::*;

// -- Step 27.2: checkout (create+switch) + branch-delete ----------------
#[test]
fn checkout_creates_and_switches_to_a_new_branch() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path()).unwrap();
    let msg = eng.checkout(&GitCaveats::top(), "feat/y", true).unwrap();
    assert!(msg.contains("created and switched"), "{msg}");
    // The system git agrees HEAD now points at the new branch.
    let out = Command::new("git")
        .current_dir(dir.path())
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "feat/y");
}
#[test]
fn checkout_switches_to_existing_branch_at_same_commit() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path()).unwrap();
    eng.branch(&GitCaveats::top(), "feat/z").unwrap(); // ref at HEAD, HEAD stays main
    let msg = eng.checkout(&GitCaveats::top(), "feat/z", false).unwrap();
    assert_eq!(msg, "switched to branch 'feat/z'");
}
#[test]
fn checkout_refuses_existing_branch_at_a_different_commit() {
    let dir = repo_with_commit();
    let p = dir.path();
    // 'ahead' is one commit past main; switching there would need a worktree
    // update, which newt does not do — it must refuse with no side effects.
    git(p, &["checkout", "-q", "-b", "ahead"]);
    std::fs::write(p.join("a.txt"), "v2\n").unwrap();
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
            "c2",
        ],
    );
    git(p, &["checkout", "-q", "main"]);
    let eng = GitEngine::open(p).unwrap();
    let err = eng
        .checkout(&GitCaveats::top(), "ahead", false)
        .unwrap_err();
    assert!(matches!(err, GitError::Refused(_)), "{err}");
    let out = Command::new("git")
        .current_dir(p)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "main");
}
#[test]
fn checkout_missing_branch_without_create_is_refused() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path()).unwrap();
    let err = eng.checkout(&GitCaveats::top(), "nope", false).unwrap_err();
    assert!(matches!(err, GitError::Refused(_)), "{err}");
}
#[test]
fn branch_delete_removes_a_non_current_branch() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path()).unwrap();
    eng.branch(&GitCaveats::top(), "scratch").unwrap();
    let msg = eng.branch_delete(&GitCaveats::top(), "scratch").unwrap();
    assert_eq!(msg, "deleted branch 'scratch'");
    let exists = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/scratch"])
        .status()
        .unwrap()
        .success();
    assert!(!exists, "ref must be gone after branch-delete");
}
#[test]
fn branch_delete_refuses_current_branch_and_missing() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path()).unwrap();
    let cur = eng.branch_delete(&GitCaveats::top(), "main").unwrap_err();
    assert!(matches!(cur, GitError::Refused(_)), "{cur}");
    let missing = eng.branch_delete(&GitCaveats::top(), "ghost").unwrap_err();
    assert!(matches!(missing, GitError::Refused(_)), "{missing}");
}
#[test]
fn checkout_and_branch_delete_fail_closed_without_refs() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path()).unwrap();
    let ro = GitCaveats::read_only();
    assert!(matches!(
        eng.checkout(&ro, "x", true),
        Err(GitError::Denied("refs"))
    ));
    assert!(matches!(
        eng.branch_delete(&ro, "x"),
        Err(GitError::Denied("refs"))
    ));
}
#[test]
fn read_ops_fail_closed_without_read_capability() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path()).unwrap();
    let no = GitCaveats::none();
    assert!(matches!(eng.status(&no), Err(GitError::Denied("read"))));
    assert!(matches!(eng.log(&no, 1), Err(GitError::Denied("read"))));
    assert!(matches!(
        eng.diff(&no, DiffSpec::Worktree),
        Err(GitError::Denied("read"))
    ));
}
#[test]
fn status_report_serde_roundtrip() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path()).unwrap();
    let s = eng.status(&GitCaveats::top()).unwrap();
    let json = serde_json::to_string(&s).unwrap();
    let back: StatusReport = serde_json::from_str(&json).unwrap();
    assert_eq!(s, back);
}
