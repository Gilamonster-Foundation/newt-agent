use super::*;

#[test]
fn open_and_status_on_clean_repo() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path()).unwrap();
    let s = eng.status(&GitCaveats::top()).unwrap();
    assert!(s.clean, "fresh commit -> clean: {s:?}");
    assert_eq!(s.branch.as_deref(), Some("main"));
    assert!(s.head.is_some());
}
#[test]
fn head_snapshot_is_full_oid_cheap_identity_and_read_gated() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path()).unwrap();
    let snapshot = eng.head_snapshot(&GitCaveats::read_only()).unwrap();
    let commit = eng.log(&GitCaveats::read_only(), 1).unwrap().remove(0);

    assert_eq!(snapshot.branch.as_deref(), Some("main"));
    assert_eq!(snapshot.head.as_deref(), Some(commit.id.as_str()));
    assert!(snapshot.head.as_ref().unwrap().len() > 7);
    assert!(matches!(
        eng.head_snapshot(&GitCaveats::none()),
        Err(GitError::Denied("read"))
    ));
}
#[test]
fn log_returns_the_commit() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path()).unwrap();
    let log = eng.log(&GitCaveats::top(), 10).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].summary, "first commit");
    assert_eq!(log[0].author_name, "Tester");
    assert_eq!(log[0].author_email, "t@example.com");
    assert!(log[0].parents.is_empty(), "root commit has no parents");
}
#[test]
fn status_sees_unstaged_modification() {
    let dir = repo_with_commit();
    std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
    let eng = GitEngine::open(dir.path()).unwrap();
    let s = eng.status(&GitCaveats::top()).unwrap();
    assert!(!s.clean);
    assert!(s.unstaged.iter().any(|f| f.path == "a.txt"));
}
#[test]
fn status_sees_untracked_file() {
    let dir = repo_with_commit();
    std::fs::write(dir.path().join("new.txt"), "x\n").unwrap();
    let eng = GitEngine::open(dir.path()).unwrap();
    let s = eng.status(&GitCaveats::top()).unwrap();
    assert!(s.untracked.iter().any(|p| p == "new.txt"), "{s:?}");
}
#[test]
fn diff_worktree_lists_the_change() {
    let dir = repo_with_commit();
    std::fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
    let eng = GitEngine::open(dir.path()).unwrap();
    let d = eng.diff(&GitCaveats::top(), DiffSpec::Worktree).unwrap();
    assert!(d.files.iter().any(|f| f.path == "a.txt"));
}
