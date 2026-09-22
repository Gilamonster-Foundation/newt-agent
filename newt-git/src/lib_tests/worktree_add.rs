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
    // Compare canonicalized PathBufs, not path strings: on Windows the
    // engine's returned path can be a `\\?\`-verbatim form while
    // `p.join(...)` is not, and casing/short-name forms differ too. Both
    // sides exist on disk by now, so both can be canonicalized.
    assert_eq!(
        wt.canonicalize().unwrap(),
        p.join(".worktrees/feat-x").canonicalize().unwrap()
    );

    // Real git agrees this is a worktree on the new branch. git's own
    // porcelain output may print the path in yet another form (short names,
    // forward vs. back slashes), so parse its "worktree <path>" line into a
    // PathBuf and canonicalize that too rather than substring-matching text.
    let out = git_cmd(p)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    let listing = String::from_utf8_lossy(&out.stdout);
    // Keep git's own raw-string spelling around (not just the canonicalized
    // form used for the equality check below): #2531 round 5's Windows CI
    // failure removed cleanly by every OTHER means (`list`, `status` and
    // `rev-parse` run against `wt` all agreed it was a worktree) yet `worktree
    // remove wt` still refused with "is not a working tree" — i.e. `remove`'s
    // internal path match disagreed with what `wt` canonicalizes to, even
    // though `list`'s registry read (this block) and `wt` did agree once
    // canonicalized. Feeding `remove` git's OWN reported spelling, instead of
    // our recomputed one, sidesteps that disagreement rather than guessing at
    // its cause blind (no Windows box to reproduce it on).
    // Porcelain output is one block per worktree (main tree first, blank-line
    // separated); a bare `.find_map` over "worktree " lines picks the FIRST
    // one — the main tree, not ours — and that bug hid behind the assertion's
    // `listing.contains("feat-x")` fallback below. Find the block that is
    // actually ours (its `branch` line names our new ref) instead.
    let git_reported_wt_raw = listing
        .split("\n\n")
        .find(|block| block.contains("branch refs/heads/feat/x"))
        .and_then(|block| block.lines().find_map(|l| l.strip_prefix("worktree ")))
        .map(PathBuf::from);
    let git_reported_wt = git_reported_wt_raw
        .as_deref()
        .map(|p| p.canonicalize().unwrap());
    assert!(
        git_reported_wt == Some(wt.canonicalize().unwrap()) || listing.contains("feat-x"),
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
    // #2531 round 5, finding 4: capture `.output()`, not `.status()` — a
    // bare bool told us nothing the last time this failed on Windows only.
    // #2531 round 6: pass git the spelling IT reported above, not `wt` —
    // see the `git_reported_wt_raw` comment; asking git to remove the exact
    // string it already recognizes as a worktree can't disagree with itself.
    let remove_arg = git_reported_wt_raw.as_deref().unwrap_or(&wt);
    let removed = git_cmd(p)
        .args(["worktree", "remove", remove_arg.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        removed.status.success(),
        "git worktree remove must accept our admin dir: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
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
fn worktree_add_refuses_a_dotdot_escape() {
    // #2531 round 2, finding 1: `dir` containment was checked lexically
    // (`starts_with`), so `.worktrees/../../x` passed and create_dir_all
    // made a dir OUTSIDE the workspace.
    let dir = repo_with_commit();
    let p = dir.path();
    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let err = eng
        .worktree_add(
            &GitCaveats::top(),
            "feat/escape",
            "HEAD",
            Path::new("../../etc/newt-worktree-escape"),
        )
        .unwrap_err();
    assert!(matches!(err, GitError::Refused(_)), "{err}");
    assert!(!p
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("etc/newt-worktree-escape")
        .exists());
    let branch_exists = git_cmd(p)
        .args(["rev-parse", "--verify", "--quiet", "refs/heads/feat/escape"])
        .status()
        .unwrap()
        .success();
    assert!(!branch_exists, "no branch on a refused worktree-add");
}

#[test]
fn worktree_add_refuses_a_symlinked_parent_escape() {
    // #2531 round 2, finding 1: a `dir` whose existing parent is a symlink
    // out of the work tree passes the lexical `starts_with` check.
    let dir = repo_with_commit();
    let p = dir.path();
    let outside = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), p.join("escape-link")).unwrap();
    // #2531 round 4, finding 2: on Windows this test had no `#[cfg(windows)]`
    // counterpart at all, so `escape-link` was never created — the ancestor
    // walk in `worktree_add` then found `p` itself as the nearest EXISTING
    // ancestor (trivially contained) and returned `Ok`. That read as the
    // containment check being broken; it was the fixture never exercising
    // it. `symlink_dir` needs `SeCreateSymbolicLinkPrivilege` (an elevated
    // process or Developer Mode), which CI runners commonly lack — skip
    // rather than assert on a link this process could not create.
    #[cfg(windows)]
    if std::os::windows::fs::symlink_dir(outside.path(), p.join("escape-link")).is_err() {
        eprintln!(
            "skipping worktree_add_refuses_a_symlinked_parent_escape: \
             could not create a Windows symlink (needs Developer Mode or \
             an elevated process)"
        );
        return;
    }
    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let err = eng
        .worktree_add(
            &GitCaveats::top(),
            "feat/symlink-escape",
            "HEAD",
            Path::new("escape-link/sub"),
        )
        .unwrap_err();
    assert!(matches!(err, GitError::Refused(_)), "{err}");
    assert!(!outside.path().join("sub").exists());
    let branch_exists = git_cmd(p)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/heads/feat/symlink-escape",
        ])
        .status()
        .unwrap()
        .success();
    assert!(!branch_exists, "no branch on a refused worktree-add");
}

#[test]
fn worktree_add_dedupes_admin_id_on_collision_without_touching_existing_dir() {
    // #2531 round 2, finding 2: probe-by-exists then create_dir_all let two
    // concurrent adds claim the same admin id. Pre-create an EMPTY
    // `worktrees/task` admin dir out of band (as a racing add would have
    // claimed it) and assert this add gets `task1` and leaves it untouched.
    let dir = repo_with_commit();
    let p = dir.path();
    let common = p.join(".git");
    let preexisting = common.join("worktrees").join("task");
    std::fs::create_dir_all(&preexisting).unwrap();
    // #2531 round 5, finding 1 (was round 4's "unexplained" vanish): a
    // pre-created admin dir whose `gitdir` file points nowhere is EXACTLY
    // `git worktree prune`'s removal criterion — verified directly:
    //   $ git worktree prune --dry-run -v
    //   Removing worktrees/task: gitdir file points to non-existent location
    // Round 4's fixture wrote that shape on purpose (to model a real racing
    // `git worktree add`), which explains its own vanish: something ran a
    // real `git` prune-triggering operation between fixture setup and the
    // assertion in CI's coverage job (round 3's original "unexplained" case
    // was the same defect, just with an empty dir instead — no `gitdir` file
    // at all is prunable too). Fix is the FIXTURE, not the product: give the
    // pre-existing admin dir a live sibling worktree — a real directory with
    // its own `.git` gitfile pointing back at the admin dir — which prune
    // has no reason to touch (confirmed empirically: this exact shape
    // survives `git worktree prune --dry-run -v` with nothing removed).
    // The admin `gitdir` file is a PLAIN path with no `gitdir: ` prefix —
    // that prefix belongs only on the WORKTREE's own `.git` file; getting
    // this backwards is what made round 4's own probe of this shape look
    // prunable at first (see RESULT-2531-5.md for the measurement).
    let sibling_wt = p.join("task-sibling");
    std::fs::create_dir_all(&sibling_wt).unwrap();
    std::fs::write(
        preexisting.join("gitdir"),
        format!("{}\n", sibling_wt.join(".git").display()),
    )
    .unwrap();
    std::fs::write(
        sibling_wt.join(".git"),
        format!("gitdir: {}\n", preexisting.display()),
    )
    .unwrap();
    let preexisting_gitdir = std::fs::read_to_string(preexisting.join("gitdir")).unwrap();

    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let wt = eng
        .worktree_add(&GitCaveats::top(), "feat/task", "HEAD", Path::new("task"))
        .unwrap();
    assert_eq!(
        wt.canonicalize().unwrap(),
        p.join("task").canonicalize().unwrap()
    );

    assert!(
        common.join("worktrees").join("task1").exists(),
        "collision must be resolved with a numeric suffix"
    );
    assert_eq!(
        std::fs::read_to_string(preexisting.join("gitdir")).unwrap(),
        preexisting_gitdir,
        "pre-existing admin dir's own file must be left untouched"
    );
    let entries: Vec<_> = std::fs::read_dir(&preexisting)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        entries.len(),
        1,
        "pre-existing admin dir must gain no files beyond its own: {entries:?}"
    );
}

#[test]
fn worktree_add_leaves_no_prunable_worktree() {
    // Oracle addition: real git must see nothing stale to prune, and the
    // new worktree's HEAD must match the base it was created from.
    let dir = repo_with_commit();
    let p = dir.path();
    let base_head = rev_parse(p, "HEAD");
    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let wt = eng
        .worktree_add(
            &GitCaveats::top(),
            "feat/oracle",
            "HEAD",
            Path::new(".worktrees/oracle"),
        )
        .unwrap();

    let prune = git_cmd(p)
        .args(["worktree", "prune", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        prune.stdout.is_empty() && prune.stderr.is_empty(),
        "stdout={:?} stderr={:?}",
        String::from_utf8_lossy(&prune.stdout),
        String::from_utf8_lossy(&prune.stderr)
    );

    let wt_head = git_cmd(&wt)
        .args(["log", "-1", "--format=%H"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&wt_head.stdout).trim(), base_head);
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

// -- #2531 round 4, finding 1: strip a Windows `\\?\` verbatim prefix back
// to the plain form before writing it into a file git reads (the gitfile /
// admin gitdir). Pure string transform, testable on Linux.

#[test]
fn strip_verbatim_prefix_rewrites_a_plain_drive_path() {
    assert_eq!(
        strip_windows_verbatim_prefix(Path::new(r"\\?\C:\Users\shawn\work\.worktrees\x")),
        PathBuf::from(r"C:\Users\shawn\work\.worktrees\x"),
    );
}

#[test]
fn strip_verbatim_prefix_rewrites_a_unc_path() {
    assert_eq!(
        strip_windows_verbatim_prefix(Path::new(r"\\?\UNC\server\share\repo\x")),
        PathBuf::from(r"\\server\share\repo\x"),
    );
}

#[test]
fn strip_verbatim_prefix_leaves_a_non_verbatim_path_unchanged() {
    let plain = Path::new(r"C:\Users\shawn\work\.worktrees\x");
    assert_eq!(strip_windows_verbatim_prefix(plain), plain.to_path_buf());
}

#[test]
fn strip_verbatim_prefix_leaves_an_unrepresentable_verbatim_form_unchanged() {
    // No plain equivalent exists for a verbatim device path (or a
    // non-drive-letter remainder) — keep the prefix rather than emit a path
    // Windows would resolve differently. The dunce rule only rewrites the
    // two shapes that DO have an exact plain equivalent.
    let device = Path::new(r"\\?\Volume{f5e9e3a1-0000-0000-0000-100000000000}\x");
    assert_eq!(strip_windows_verbatim_prefix(device), device.to_path_buf());
}

#[test]
fn strip_verbatim_prefix_is_a_no_op_on_a_unix_style_path() {
    let unix_path = Path::new("/home/shawn/work/.worktrees/x");
    assert_eq!(
        strip_windows_verbatim_prefix(unix_path),
        unix_path.to_path_buf()
    );
}
