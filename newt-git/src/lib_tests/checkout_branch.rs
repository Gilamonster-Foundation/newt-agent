use super::*;

// -- Step 27.2: checkout (create+switch) + branch-delete ----------------
#[test]
fn checkout_creates_and_switches_to_a_new_branch() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let msg = eng.checkout(&GitCaveats::top(), "feat/y", true).unwrap();
    assert!(msg.contains("created and switched"), "{msg}");
    // The system git agrees HEAD now points at the new branch.
    let out = git_cmd(dir.path())
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "feat/y");
}
#[test]
fn checkout_switches_to_existing_branch_at_same_commit() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    eng.branch(&GitCaveats::top(), "feat/z").unwrap(); // ref at HEAD, HEAD stays main
    let msg = eng.checkout(&GitCaveats::top(), "feat/z", false).unwrap();
    assert_eq!(msg, "switched to branch 'feat/z'");
}
/// #2485: switching to a branch at a different commit, with a clean tree,
/// now moves HEAD and resets tree == that branch's tree — same guarded tail
/// `rebase` uses (#2518's ignored-file guard, `checkout_between_trees`).
#[test]
fn checkout_switches_to_a_different_commit_on_a_clean_tree() {
    let dir = repo_with_commit();
    let p = dir.path();
    git(p, &["checkout", "-q", "-b", "ahead"]);
    std::fs::write(p.join("a.txt"), "v2\n").unwrap();
    git(p, &["add", "a.txt"]);
    git(p, &["commit", "-q", "-m", "c2"]);
    let ahead_head = String::from_utf8_lossy(
        &git_cmd(p)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    git(p, &["checkout", "-q", "main"]);
    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let msg = eng.checkout(&GitCaveats::top(), "ahead", false).unwrap();
    assert_eq!(msg, "switched to branch 'ahead'");
    let head_ref = git_cmd(p)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&head_ref.stdout).trim(), "ahead");
    let head_oid = git_cmd(p).args(["rev-parse", "HEAD"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&head_oid.stdout).trim(), ahead_head);
    let status = git_cmd(p).args(["status", "--porcelain"]).output().unwrap();
    assert!(
        status.stdout.is_empty(),
        "tree must be clean after the switch: {:?}",
        String::from_utf8_lossy(&status.stdout)
    );
    assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "v2\n");
}
/// #2485: a dirty tree refuses the switch with no side effects — same
/// clean-tree precondition as `rebase`.
#[test]
fn checkout_refuses_a_different_commit_on_a_dirty_tree() {
    let dir = repo_with_commit();
    let p = dir.path();
    git(p, &["checkout", "-q", "-b", "ahead"]);
    std::fs::write(p.join("a.txt"), "v2\n").unwrap();
    git(p, &["add", "a.txt"]);
    git(p, &["commit", "-q", "-m", "c2"]);
    git(p, &["checkout", "-q", "main"]);
    std::fs::write(p.join("a.txt"), "dirty\n").unwrap();
    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let err = eng
        .checkout(&GitCaveats::top(), "ahead", false)
        .unwrap_err();
    assert!(matches!(err, GitError::Refused(_)), "{err}");
    let out = git_cmd(p)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "main");
    assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "dirty\n");
}
/// #2517/#2518 (RECON item 2): the same ignored-file guard `rebase` needs
/// applies to `checkout` — an ignored file invisible to `status`'s clean
/// check must not be silently overwritten by the switch.
#[test]
fn checkout_refuses_when_the_target_tree_would_overwrite_an_ignored_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    std::fs::write(p.join(".gitignore"), "x.env\n").unwrap();
    std::fs::write(p.join("a.txt"), "v1\n").unwrap();
    git(p, &["add", ".gitignore", "a.txt"]);
    git(p, &["commit", "-q", "-m", "c1"]);
    let main_head = String::from_utf8_lossy(
        &git_cmd(p)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    git(p, &["checkout", "-q", "-b", "adds-env"]);
    std::fs::write(p.join("x.env"), "TRACKED\n").unwrap();
    git(p, &["add", "-f", "x.env"]);
    git(p, &["commit", "-q", "-m", "c2 adds x.env"]);
    git(p, &["checkout", "-q", "main"]);
    // A local, ignored x.env now sits on disk — invisible to `status` and
    // NOT what the target branch tracks.
    std::fs::write(p.join("x.env"), "LOCAL\n").unwrap();

    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let err = eng
        .checkout(&GitCaveats::top(), "adds-env", false)
        .unwrap_err();
    assert!(matches!(err, GitError::Refused(_)), "{err}");
    assert!(err.to_string().contains("x.env"), "{err}");
    let out = git_cmd(p).args(["rev-parse", "HEAD"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), main_head);
    assert_eq!(
        std::fs::read_to_string(p.join("x.env")).unwrap(),
        "LOCAL\n",
        "the local ignored file must be untouched"
    );
}
#[test]
fn checkout_missing_branch_without_create_is_refused() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let err = eng.checkout(&GitCaveats::top(), "nope", false).unwrap_err();
    assert!(matches!(err, GitError::Refused(_)), "{err}");
}
#[test]
fn branch_delete_removes_a_non_current_branch() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    eng.branch(&GitCaveats::top(), "scratch").unwrap();
    let msg = eng.branch_delete(&GitCaveats::top(), "scratch").unwrap();
    assert_eq!(msg, "deleted branch 'scratch'");
    let exists = git_cmd(dir.path())
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/scratch"])
        .status()
        .unwrap()
        .success();
    assert!(!exists, "ref must be gone after branch-delete");
}
#[test]
fn branch_delete_refuses_current_branch_and_missing() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let cur = eng.branch_delete(&GitCaveats::top(), "main").unwrap_err();
    assert!(matches!(cur, GitError::Refused(_)), "{cur}");
    let missing = eng.branch_delete(&GitCaveats::top(), "ghost").unwrap_err();
    assert!(matches!(missing, GitError::Refused(_)), "{missing}");
}
#[test]
fn checkout_and_branch_delete_fail_closed_without_refs() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
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
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
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
    let eng = GitEngine::open(dir.path(), &Scope::All).unwrap();
    let s = eng.status(&GitCaveats::top()).unwrap();
    let json = serde_json::to_string(&s).unwrap();
    let back: StatusReport = serde_json::from_str(&json).unwrap();
    assert_eq!(s, back);
}
/// An unborn HEAD (an orphan branch with nothing committed yet) has no old
/// tree. The switch must still populate the worktree from the target branch:
/// before the fix the tail skipped the reset when `old_tree` was `None`, so
/// HEAD moved while the worktree stayed empty (tree != HEAD).
#[test]
fn checkout_from_an_unborn_head_populates_the_tree() {
    let dir = repo_with_commit();
    let p = dir.path();
    git(p, &["branch", "-q", "other"]);
    git(p, &["checkout", "-q", "--orphan", "fresh"]);
    git(p, &["rm", "-rfq", "."]);
    assert!(!p.join("a.txt").exists(), "precondition: empty worktree");
    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let msg = eng.checkout(&GitCaveats::top(), "other", false).unwrap();
    assert_eq!(msg, "switched to branch 'other'");
    assert!(
        p.join("a.txt").exists(),
        "the target tree must be checked out"
    );
    let status = git_cmd(p).args(["status", "--porcelain"]).output().unwrap();
    assert!(
        status.stdout.is_empty(),
        "tree must equal HEAD after the switch: {:?}",
        String::from_utf8_lossy(&status.stdout)
    );
}
/// A same-size edit made in the same timestamp tick as the index write is
/// "racily clean": size and mtime still match the index entry, so a stat-only
/// check calls the tree clean and the switch overwrites the edit. Git guards
/// this by hashing entries whose mtime is not older than the index file's.
/// Forced deterministically here by pinning both mtimes to one instant.
#[test]
fn checkout_refuses_a_racily_clean_edit() {
    let dir = repo_with_commit();
    let p = dir.path();
    git(p, &["checkout", "-q", "-b", "ahead"]);
    std::fs::write(p.join("a.txt"), "v2\n").unwrap();
    git(p, &["add", "a.txt"]);
    git(p, &["commit", "-q", "-m", "c2"]);
    git(p, &["checkout", "-q", "main"]);
    let eng = GitEngine::open(p, &Scope::All).unwrap();
    std::fs::write(p.join("a.txt"), "dirty\n").unwrap(); // same size as "hello\n"
                                                         // The index caches the edited file's stat against the OLD blob, and the
                                                         // index file is stamped in the same instant: exactly a same-tick race.
    let mut index = eng.repo.load_index().unwrap();
    let entry = index.get_mut(b"a.txt", 0).unwrap();
    *entry = grit_lib::index::entry_from_stat(&p.join("a.txt"), b"a.txt", entry.oid, entry.mode)
        .unwrap();
    eng.repo.write_index(&mut index).unwrap();
    let edited = std::fs::metadata(p.join("a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let index_file = std::fs::OpenOptions::new()
        .write(true)
        .open(p.join(".git/index"))
        .unwrap();
    index_file.set_modified(edited).unwrap();
    let err = eng
        .checkout(&GitCaveats::top(), "ahead", false)
        .unwrap_err();
    assert!(matches!(err, GitError::Refused(_)), "{err}");
    assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "dirty\n");
}
