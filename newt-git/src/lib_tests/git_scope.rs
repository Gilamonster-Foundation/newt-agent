use super::*;

/// A child test process grounds ambient-environment isolation without racing
/// other tests by changing this process's GIT_NAMESPACE.
#[test]
fn branch_list_rejects_ambient_namespace() {
    const CHILD: &str = "NEWT_BRANCH_LIST_NAMESPACE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::git_scope::branch_list_rejects_ambient_namespace",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("GIT_NAMESPACE", "isolated")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let repo = repo_with_commit();
    let out = tool(repo.path()).dispatch(
        "branch-list",
        &serde_json::json!({}),
        &GitCaveats::read_only(),
        &read_scope(&[repo.path()]),
    );
    assert!(
        matches!(out, Err(ref error) if error.contains("GIT_NAMESPACE")),
        "{out:?}"
    );
}

/// Real configuration text grounds fail-closed format detection rather than
/// relying on grit's simplified unquoted refStorage detector.
#[test]
fn branch_list_rejects_reftable_config_spellings() {
    let repo = repo_with_commit();
    for value in ["reftable", "\"reftable\"", "reftable # comment"] {
        std::fs::write(
            repo.path().join(".git/config"),
            format!("[extensions]\nrefStorage = {value}\n"),
        )
        .unwrap();
        let out = tool(repo.path()).dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &read_scope(&[repo.path()]),
        );
        assert!(
            matches!(out, Err(ref error) if error.contains("reftable")),
            "{value}: {out:?}"
        );
    }
}

/// Real packed refs ground the name-to-path boundary before symbolic alias
/// lookup. Malformed remote names must not expose an external file's content;
/// malformed local names must not be included in otherwise successful counts.
#[test]
fn branch_list_rejects_pathlike_packed_names_before_symbolic_lookup() {
    let repo = repo_with_commit();
    std::fs::create_dir_all(repo.path().join(".git/refs/remotes")).unwrap();
    let external = tempfile::NamedTempFile::new_in(repo.path().parent().unwrap()).unwrap();
    let marker = "external ref fixture must not be disclosed";
    std::fs::write(external.path(), marker).unwrap();
    let oid = std::fs::read_to_string(repo.path().join(".git/refs/heads/main")).unwrap();
    for name in ["remotes", "heads"].into_iter().flat_map(|namespace| {
        [
            format!(
                "refs/{namespace}/../../../../{}",
                external.path().file_name().unwrap().to_str().unwrap()
            ),
            format!("refs/{namespace}"),
        ]
    }) {
        std::fs::write(
            repo.path().join(".git/packed-refs"),
            format!("{} {name}\n", oid.trim()),
        )
        .unwrap();
        let out = tool(repo.path()).dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &read_scope(&[repo.path()]),
        );
        assert!(
            matches!(out, Err(ref error) if error.contains("invalid branch ref name") && !error.contains(marker)),
            "{out:?}"
        );
    }
}

/// Native Git can leave an empty lock beside a valid branch. The ref parser
/// ignores that non-ref; path validation must not turn it into a false blocker.
#[test]
fn branch_list_ignores_non_ref_files_without_following_their_contents() {
    let repo = repo_with_commit();
    std::fs::write(repo.path().join(".git/refs/heads/topic.lock"), "").unwrap();
    let out = tool(repo.path())
        .dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &read_scope(&[repo.path()]),
        )
        .unwrap();
    assert!(out.contains("local branches: 1"), "{out}");
}

/// A real FIFO grounds the non-regular-file guard. The watchdog makes the
/// regression fail instead of hanging the test process in an unbounded open.
#[cfg(unix)]
#[test]
fn branch_list_rejects_special_ref_files_without_opening_them() {
    let repo = repo_with_commit();
    assert!(Command::new("mkfifo")
        .arg(repo.path().join(".git/refs/heads/pipe"))
        .status()
        .unwrap()
        .success());
    let root = repo.path().to_owned();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = tool(&root).dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &read_scope(&[&root]),
        );
        let _ = tx.send(out);
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("must reject a FIFO without opening it");
    assert!(
        matches!(out, Err(ref error) if error.contains("non-regular")),
        "{out:?}"
    );
}

fn read_scope(paths: &[&Path]) -> newt_core::caveats::Caveats {
    use newt_core::caveats::{Caveats, Scope};
    Caveats {
        fs_read: Scope::only(paths.iter().map(|p| p.to_string_lossy().into_owned())),
        fs_write: Scope::none(),
        exec: Scope::none(),
        net: Scope::none(),
        ..Caveats::top()
    }
}

/// Real refs ground the strict catalog's nullable scope: null and omission
/// both enumerate all branches using only the bounded repository read grant.
#[test]
fn confined_act_branch_list_null_scope_matches_omitted_bounded_all() {
    let repo = repo_with_commit();
    git(
        repo.path(),
        &["update-ref", "refs/remotes/origin/main", "HEAD"],
    );
    let session = read_scope(&[repo.path()]);
    let t = tool(repo.path());
    let dispatch = |args: serde_json::Value| {
        t.dispatch("branch-list", &args, &GitCaveats::read_only(), &session)
    };
    let omitted = dispatch(serde_json::json!({"op": "branch-list"})).unwrap();
    assert!(omitted.contains("local branches: 1"), "{omitted}");
    assert!(
        omitted.contains("cached remote-tracking branches: 1"),
        "{omitted}"
    );
    assert_eq!(
        omitted
            .lines()
            .filter(|line| line.starts_with("refs/"))
            .collect::<Vec<_>>(),
        ["refs/heads/main", "refs/remotes/origin/main"]
    );
    assert_eq!(
        dispatch(serde_json::json!({"op": "branch-list", "scope": "all"})).unwrap(),
        omitted
    );
    for invalid in [
        serde_json::json!("everything"),
        serde_json::json!(1),
        serde_json::json!(true),
        serde_json::json!([]),
        serde_json::json!({}),
    ] {
        let out = dispatch(serde_json::json!({"op": "branch-list", "scope": invalid}));
        assert!(
            matches!(out, Err(ref error) if error.contains("scope must be local, remote, or all")),
            "{out:?}"
        );
    }
    assert_eq!(
        dispatch(serde_json::json!({"op": "branch-list", "scope": null})).unwrap(),
        omitted
    );
}

/// Real repositories ground the dispatch mock's authority boundary. Git's
/// read bit cannot substitute for the session's repository filesystem grant.
#[test]
fn git_reads_require_the_injected_repository_read_scope() {
    let dir = repo_with_commit();
    let other = tempfile::tempdir().unwrap();
    for session in [read_scope(&[]), read_scope(&[other.path()])] {
        let out = tool(dir.path()).dispatch(
            "status",
            &serde_json::json!({}),
            &GitCaveats::from_session(&session),
            &session,
        );
        assert!(
            matches!(out, Err(ref error) if error.contains("fs_read")),
            "{out:?}"
        );
    }
}

/// Real gitfiles/common directories ground the dispatcher grant contract:
/// a linked checkout does not confer access to its external administration.
#[test]
fn branch_list_linked_worktree_requires_explicit_metadata_read_grants() {
    let repo = repo_with_commit();
    let linked = tempfile::tempdir().unwrap();
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.path().to_str().unwrap(),
        ],
    );
    let admin = grit_lib::repo::resolve_dot_git(&linked.path().join(".git")).unwrap();
    for session in [
        read_scope(&[linked.path()]),
        read_scope(&[linked.path(), &admin]),
    ] {
        let out = tool(linked.path()).dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &session,
        );
        assert!(
            matches!(out, Err(ref error) if error.contains("fs_read")),
            "{out:?}"
        );
    }
    let session = read_scope(&[linked.path(), &repo.path().join(".git")]);
    let out = tool(linked.path())
        .dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &session,
        )
        .unwrap();
    assert!(
        out.contains("local branches: 2") && out.contains("refs/heads/linked"),
        "{out}"
    );
}

/// A real nested working directory grounds bounded discovery: ancestor
/// search may not turn a child-only grant into a grant for its parent repo.
#[test]
fn branch_list_discovery_stays_inside_read_scope() {
    let repo = repo_with_commit();
    let child = repo.path().join("nested");
    std::fs::create_dir(&child).unwrap();
    let out = tool(&child).dispatch(
        "branch-list",
        &serde_json::json!({}),
        &GitCaveats::read_only(),
        &read_scope(&[&child]),
    );
    assert!(
        matches!(out, Err(ref error) if error.contains("fs_read")),
        "{out:?}"
    );
    assert!(tool(&child)
        .dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &read_scope(&[repo.path()])
        )
        .is_ok());
}

/// A second real commondir must not redirect an approved shared store to a
/// third, ungranted repository during the library's symbolic-ref resolution.
#[test]
fn branch_list_rejects_nested_common_directory_indirection() {
    let repo = repo_with_commit();
    let linked = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.path().to_str().unwrap(),
        ],
    );
    std::fs::write(
        repo.path().join(".git/commondir"),
        outside.path().to_string_lossy().as_bytes(),
    )
    .unwrap();
    let session = read_scope(&[linked.path(), &repo.path().join(".git")]);
    let out = tool(linked.path()).dispatch(
        "branch-list",
        &serde_json::json!({}),
        &GitCaveats::read_only(),
        &session,
    );
    assert!(
        matches!(out, Err(ref error) if error.contains("nested Git commondir")),
        "{out:?}"
    );
}

/// Real symlinks ground the missing child-path check: approving .git itself
/// is not permission to follow refs/config/packed-refs to an external file.
#[cfg(unix)]
#[test]
fn branch_list_ref_inputs_cannot_escape_through_child_symlinks() {
    for relative in ["config", "packed-refs", "refs/heads/escape"] {
        let repo = repo_with_commit();
        let external = tempfile::NamedTempFile::new().unwrap();
        let target = repo.path().join(".git").join(relative);
        if target.exists() {
            std::fs::remove_file(&target).unwrap();
        }
        std::os::unix::fs::symlink(external.path(), &target).unwrap();
        let out = tool(repo.path()).dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &read_scope(&[repo.path()]),
        );
        assert!(
            matches!(out, Err(ref error) if error.contains("fs_read")),
            "{relative}: {out:?}"
        );
    }
    let repo = repo_with_commit();
    let external = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(external.path(), repo.path().join(".git/refs/heads/escape"))
        .unwrap();
    let out = tool(repo.path()).dispatch(
        "branch-list",
        &serde_json::json!({}),
        &GitCaveats::read_only(),
        &read_scope(&[repo.path()]),
    );
    assert!(
        matches!(out, Err(ref error) if error.contains("fs_read")),
        "directory: {out:?}"
    );
}

/// Real symbolic-ref contents ground the library-resolution guard. A ref
/// target is not an arbitrary pathname, even when the parser accepts it.
#[test]
fn branch_list_ref_inputs_reject_pathlike_symbolic_targets() {
    let repo = repo_with_commit();
    for target in ["../../outside", "/outside", "HEAD"] {
        std::fs::write(
            repo.path().join(".git/refs/heads/escape"),
            format!("ref: {target}\n"),
        )
        .unwrap();
        let out = tool(repo.path()).dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &read_scope(&[repo.path()]),
        );
        assert!(
            matches!(out, Err(ref error) if error.contains("symbolic ref target")),
            "{target}: {out:?}"
        );
    }
}
