use super::*;

// -- item 5a (epic #2524): worktree-add built from grit-lib primitives,
// since grit-lib itself has no `add` mutator yet (see the `worktree_add`
// doc comment). Real git is the oracle for the on-disk layout.

#[test]
fn worktree_add_creates_a_worktree_git_accepts() {
    let dir = repo_with_commit();
    let p = dir.path();
    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let wt = eng
        .worktree_add(
            &GitCaveats::top(),
            "feat/x",
            "HEAD",
            Path::new(".worktrees/feat-x"),
        )
        .unwrap();
    assert_eq!(wt, p.join(".worktrees/feat-x"));

    // Real git agrees this is a worktree on the new branch.
    let out = git_cmd(p)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(
        listing.contains(&wt.canonicalize().unwrap().to_string_lossy().into_owned())
            || listing.contains("feat-x"),
        "{listing}"
    );
    assert!(listing.contains("refs/heads/feat/x"), "{listing}");

    let status = git_cmd(&wt)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        status.stdout.is_empty(),
        "new worktree must be clean: {:?}",
        String::from_utf8_lossy(&status.stdout)
    );
    let head = git_cmd(&wt).args(["rev-parse", "HEAD"]).output().unwrap();
    let base_head = rev_parse(p, "HEAD");
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), base_head);
    assert_eq!(
        std::fs::read_to_string(wt.join("a.txt")).unwrap(),
        "hello\n"
    );

    // git accepts the admin dir we built well enough to remove it cleanly.
    let removed = git_cmd(p)
        .args(["worktree", "remove", wt.to_str().unwrap()])
        .status()
        .unwrap()
        .success();
    assert!(removed, "git worktree remove must accept our admin dir");
}

#[test]
fn worktree_add_leaves_the_source_checkout_untouched() {
    let dir = repo_with_commit();
    let p = dir.path();
    let head_before = rev_parse(p, "HEAD");
    let branch_before = String::from_utf8_lossy(
        &git_cmd(p)
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    let index_before = std::fs::read(p.join(".git/index")).unwrap();

    let eng = GitEngine::open(p, &Scope::All).unwrap();
    eng.worktree_add(
        &GitCaveats::top(),
        "feat/y",
        "HEAD",
        Path::new(".worktrees/y"),
    )
    .unwrap();

    assert_eq!(rev_parse(p, "HEAD"), head_before);
    let branch_after = String::from_utf8_lossy(
        &git_cmd(p)
            .args(["symbolic-ref", "--short", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string();
    assert_eq!(branch_after, branch_before);
    assert_eq!(std::fs::read(p.join(".git/index")).unwrap(), index_before);
}

#[test]
fn worktree_add_refuses_an_existing_branch() {
    let dir = repo_with_commit();
    let p = dir.path();
    let eng = GitEngine::open(p, &Scope::All).unwrap();
    eng.branch(&GitCaveats::top(), "taken").unwrap();
    let err = eng
        .worktree_add(
            &GitCaveats::top(),
            "taken",
            "HEAD",
            Path::new(".worktrees/taken"),
        )
        .unwrap_err();
    assert!(matches!(err, GitError::Refused(_)), "{err}");
    assert!(!p.join(".worktrees/taken").exists());
}

#[test]
fn worktree_add_refuses_an_existing_dir() {
    let dir = repo_with_commit();
    let p = dir.path();
    std::fs::create_dir_all(p.join(".worktrees/dupe")).unwrap();
    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let err = eng
        .worktree_add(
            &GitCaveats::top(),
            "feat/dupe",
            "HEAD",
            Path::new(".worktrees/dupe"),
        )
        .unwrap_err();
    assert!(matches!(err, GitError::Refused(_)), "{err}");
    // Refusal must not have created the branch either.
    let exists = git_cmd(p)
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/feat/dupe"])
        .status()
        .unwrap()
        .success();
    assert!(!exists, "no branch on a refused worktree-add");
}

#[test]
fn worktree_add_refuses_an_unresolvable_base() {
    let dir = repo_with_commit();
    let p = dir.path();
    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let err = eng
        .worktree_add(
            &GitCaveats::top(),
            "feat/z",
            "not-a-thing",
            Path::new(".worktrees/z"),
        )
        .unwrap_err();
    assert!(!p.join(".worktrees/z").exists());
    let branch_exists = git_cmd(p)
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/feat/z"])
        .status()
        .unwrap()
        .success();
    assert!(!branch_exists, "{err}");
}

#[test]
fn worktree_add_refuses_a_dir_outside_the_work_tree() {
    let dir = repo_with_commit();
    let p = dir.path();
    let outside = tempfile::tempdir().unwrap();
    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let err = eng
        .worktree_add(&GitCaveats::top(), "feat/outside", "HEAD", outside.path())
        .unwrap_err();
    assert!(matches!(err, GitError::Refused(_)), "{err}");
}

#[test]
fn worktree_add_fails_closed_without_refs_or_stage() {
    let dir = repo_with_commit();
    let p = dir.path();
    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let ro = GitCaveats::read_only();
    assert!(matches!(
        eng.worktree_add(&ro, "feat/ro", "HEAD", Path::new(".worktrees/ro")),
        Err(GitError::Denied("refs"))
    ));
    assert!(!p.join(".worktrees/ro").exists());
}
