use super::*;

pub(crate) fn fixture(nested: bool) -> (tempfile::TempDir, AdoptedWorktree, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("main");
    let worktree = if nested {
        original.join(".worktrees/task")
    } else {
        temp.path().join("task")
    };
    let common = original.join(".git");
    let admin = common.join("worktrees/task");
    let unrelated = temp.path().join("other");
    for dir in [&worktree, &admin, &unrelated, &common.join("objects")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(common.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let policy = AdoptedWorktree {
        original: original.canonicalize().unwrap(),
        worktree: worktree.canonicalize().unwrap(),
        common: common.canonicalize().unwrap(),
        admin: admin.canonicalize().unwrap(),
        protected_branch: Some("main".into()),
    };
    (temp, policy, unrelated.canonicalize().unwrap())
}

/// #2733: removing the original root alone must not leave an ancestor grant
/// authorizing Python writes there; nested task worktrees still work.
#[test]
fn adoption_removes_ancestor_authority_but_keeps_task_and_disjoint_roots() {
    for nested in [false, true] {
        let (temp, policy, unrelated) = fixture(nested);
        let mut session = Caveats::top();
        session.fs_write = Scope::only([
            temp.path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            unrelated.to_string_lossy().into_owned(),
        ]);
        let narrowed = policy.attenuate(&session);
        assert!(!crate::caveats::permits_path(
            &narrowed.fs_write,
            policy.original.join("source.rs").to_str().unwrap()
        ));
        assert!(crate::caveats::permits_path(
            &narrowed.fs_write,
            policy.worktree.join("source.rs").to_str().unwrap()
        ));
        assert!(crate::caveats::permits_path(
            &narrowed.fs_write,
            unrelated.to_str().unwrap()
        ));
        assert_eq!(narrowed.fs_read, session.fs_read);
    }
}

/// #2733: full access must also become a positive task-worktree fence.
#[test]
fn adoption_narrows_all_and_preserves_explicit_git_admin_only() {
    let (_temp, policy, _) = fixture(false);
    let narrowed = policy.attenuate(&Caveats::top());
    assert!(!crate::caveats::permits_path(
        &narrowed.fs_write,
        policy.original.to_str().unwrap()
    ));
    let mut git = narrowed;
    git.fs_write = Scope::only(
        [
            policy.worktree.clone(),
            policy.admin.clone(),
            policy.common.join("objects"),
        ]
        .iter()
        .map(|p| p.to_string_lossy().into_owned()),
    );
    assert_eq!(policy.attenuate(&git).fs_write, git.fs_write);
    assert!(!crate::caveats::permits_path(
        &git.fs_write,
        policy.common.join("HEAD").to_str().unwrap()
    ));
}

pub(crate) fn link(policy: &AdoptedWorktree) {
    std::fs::write(
        policy.worktree.join(".git"),
        format!("gitdir: {}\n", policy.admin.display()),
    )
    .unwrap();
    std::fs::write(policy.admin.join("commondir"), "../..\n").unwrap();
    std::fs::write(
        policy.admin.join("gitdir"),
        format!("{}\n", policy.worktree.join(".git").display()),
    )
    .unwrap();
}

/// #2733: only a new linked checkout of the same repository can be adopted;
/// existing worktrees and failed commands leave the session alone.
#[cfg(unix)]
#[test]
fn verified_creation_requires_new_matching_git_metadata() {
    let (_temp, policy, _) = fixture(false);
    let candidate = Creation::before(&policy.original, &policy.worktree, &Caveats::top()).unwrap();
    assert!(candidate.verify().is_none());
    let candidate = Creation::before(&policy.original, &policy.worktree, &Caveats::top()).unwrap();
    link(&policy);
    let verified = candidate.verify().unwrap();
    assert_eq!(verified.worktree, policy.worktree);
    assert!(Creation::before(&policy.original, &policy.worktree, &Caveats::top()).is_none());
    assert!(verified.valid(&Caveats::top()));
    std::fs::write(policy.admin.join("commondir"), "/unrelated\n").unwrap();
    assert!(!verified.valid(&Caveats::top()));
}

/// #2733: forged reverse links do not establish creation evidence.
#[cfg(unix)]
#[test]
fn creation_rejects_wrong_reverse_link() {
    let (_temp, policy, _) = fixture(false);
    let candidate = Creation::before(&policy.original, &policy.worktree, &Caveats::top()).unwrap();
    link(&policy);
    std::fs::write(
        policy.admin.join("gitdir"),
        policy.original.join(".git").to_string_lossy().as_bytes(),
    )
    .unwrap();
    assert!(candidate.verify().is_none());
}

/// #2733: held session state survives snapshots, is not shared with other
/// sessions, and only an explicit lift/new task removes the restriction.
#[test]
fn session_retains_adoption_until_explicit_lift() {
    let (_temp, policy, _) = fixture(true);
    let first = WorktreeSession::default();
    let second = WorktreeSession::default();
    first.adopt(policy.clone());
    for _ in 0..3 {
        assert_eq!(first.snapshot().unwrap().worktree, policy.worktree);
    }
    assert!(second.snapshot().is_none());
    first.lift();
    assert!(first.snapshot().is_none());
}

/// #2733: lexical traversal and symlink aliases cannot retain write authority.
#[cfg(unix)]
#[test]
fn original_checkout_aliases_and_parent_traversal_are_blocked() {
    let (temp, policy, _) = fixture(true);
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&policy.original, &alias).unwrap();
    assert!(policy.blocked(&alias.join("source.rs")));
    assert!(policy.blocked(&policy.worktree.join("../../source.rs")));
    let mut c = Caveats::top();
    c.fs_write = Scope::only([alias.to_string_lossy().into_owned()]);
    assert!(!crate::caveats::permits_path(
        &policy.attenuate(&c).fs_write,
        &alias.join("source.rs").to_string_lossy()
    ));
}

/// #2733: attenuation never grants a task path that the caller cannot write.
#[test]
fn adoption_preserves_deny_all_and_unrelated_only_authority() {
    let (_temp, policy, unrelated) = fixture(false);
    let mut c = Caveats::top();
    for scope in [
        Scope::none(),
        Scope::only([unrelated.to_string_lossy().into_owned()]),
    ] {
        c.fs_write = scope.clone();
        assert_eq!(policy.attenuate(&c).fs_write, scope);
    }
}

/// #2733: a new Git checkout from a different repository is not adoption.
#[cfg(unix)]
#[test]
fn creation_rejects_a_foreign_repository() {
    let (temp, policy, _) = fixture(false);
    let candidate = Creation::before(&policy.original, &policy.worktree, &Caveats::top()).unwrap();
    let foreign = temp.path().join("foreign.git");
    std::fs::create_dir(&foreign).unwrap();
    link(&policy);
    std::fs::write(
        policy.admin.join("commondir"),
        foreign.to_string_lossy().as_bytes(),
    )
    .unwrap();
    assert!(candidate.verify().is_none());
}

/// #2733: platforms without held, symlink-safe metadata reads cannot mint or
/// validate adoption evidence. A plausible Git layout cannot bypass that floor.
#[cfg(not(unix))]
#[test]
fn unsupported_metadata_reads_refuse_adoption() {
    let (_temp, policy, _) = fixture(false);
    assert!(Creation::before(&policy.original, &policy.worktree, &Caveats::top()).is_none());
    link(&policy);
    assert!(!policy.valid(&Caveats::top()));
}

/// #2748: a linked original checkout can share a common directory outside its
/// own root; an explicit common-dir grant must not expose its refs/config.
#[test]
fn adoption_fences_common_metadata_outside_the_original_checkout() {
    let (temp, mut policy, _) = fixture(false);
    let common = temp.path().join("shared.git");
    std::fs::rename(&policy.common, &common).unwrap();
    policy.common = common.canonicalize().unwrap();
    policy.admin = policy.common.join("worktrees/task");
    let authority = Caveats {
        fs_write: Scope::only([
            policy.common.to_string_lossy().into_owned(),
            policy.worktree.to_string_lossy().into_owned(),
        ]),
        ..Caveats::top()
    };
    let narrowed = policy.attenuate(&authority);
    for relative in [
        "HEAD",
        "index",
        "config",
        "packed-refs",
        "refs/heads/main",
        "logs/refs/heads/main",
    ] {
        let path = policy.common.join(relative);
        assert!(policy.blocked(&path), "{relative}");
        assert!(
            !crate::caveats::permits_path(&narrowed.fs_write, &path.to_string_lossy()),
            "{relative}"
        );
    }
    for path in [
        &policy.worktree,
        &policy.admin,
        &policy.common.join("objects"),
    ] {
        assert!(crate::caveats::permits_path(
            &narrowed.fs_write,
            &path.to_string_lossy()
        ));
    }
}

/// #2759: nested creation is distinguished from supported sibling creation on
/// every platform; dispatch rejects it before running Git.
#[test]
fn adoption_2759_canonical_nested_destination() {
    let (_temp, policy, _) = fixture(false);
    std::fs::create_dir(policy.original.join("sub")).unwrap();
    for (destination, expected) in [
        (policy.original.join(".worktrees/task"), true),
        (policy.original.join("sub/../task"), true),
        (policy.worktree.clone(), false),
    ] {
        let candidate =
            Creation::before(&policy.original.join("sub"), &destination, &Caveats::top()).unwrap();
        assert_eq!(candidate.nested_in_original(), expected, "{destination:?}");
    }
}
