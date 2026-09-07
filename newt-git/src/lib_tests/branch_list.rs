use super::*;

/// Real refs ground the catalog's read-only branch-list promise: packed and
/// loose refs are one namespace, while symbolic remote aliases are not branches.
#[test]
fn branch_list_counts_ref_names_not_oids_and_respects_scope() {
    let dir = repo_with_commit();
    let p = dir.path();
    git(p, &["branch", "feature/nested"]);
    git(p, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(
        p,
        &["update-ref", "refs/remotes/origin/feature/nested", "HEAD"],
    );
    git(
        p,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    git(p, &["tag", "not-a-branch"]);
    git(p, &["pack-refs", "--all", "--prune"]);
    // A loose copy shadows its packed entry; it must not be counted twice.
    let oid = std::fs::read_to_string(p.join(".git/packed-refs"))
        .unwrap()
        .lines()
        .find(|line| line.ends_with(" refs/heads/main"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    std::fs::write(p.join(".git/refs/heads/main"), format!("{oid}\n")).unwrap();
    let t = tool(p);
    let out = t
        .dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &newt_core::caveats::Caveats::top(),
        )
        .unwrap();
    assert!(out.contains("local branches: 2"), "{out}");
    assert!(out.contains("cached remote-tracking branches: 2"), "{out}");
    assert!(out.contains("refs/heads/feature/nested"), "{out}");
    assert!(out.contains("refs/remotes/origin/feature/nested"), "{out}");
    assert!(
        !out.contains("origin/HEAD") && !out.contains("not-a-branch"),
        "{out}"
    );
    let oracle = Command::new("git")
        .current_dir(p)
        .args([
            "for-each-ref",
            "--format=%(refname)%09%(symref)",
            "refs/heads/",
            "refs/remotes/",
        ])
        .output()
        .unwrap();
    assert!(oracle.status.success());
    let oracle = String::from_utf8(oracle.stdout).unwrap();
    let expected = oracle
        .lines()
        .filter_map(|line| {
            let (name, target) = line.split_once('\t').unwrap();
            (!name.starts_with("refs/remotes/") || target.is_empty()).then_some(name)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        out.lines()
            .filter(|line| line.starts_with("refs/"))
            .collect::<Vec<_>>(),
        expected
    );
    for (scope, present, absent) in [
        ("local", "refs/heads/main", "refs/remotes/"),
        ("remote", "refs/remotes/origin/main", "refs/heads/"),
    ] {
        let out = t
            .dispatch(
                "branch-list",
                &serde_json::json!({"scope": scope}),
                &GitCaveats::read_only(),
                &newt_core::caveats::Caveats::top(),
            )
            .unwrap();
        assert!(
            out.contains(present) && !out.contains(absent),
            "{scope}: {out}"
        );
    }
    assert!(t
        .dispatch(
            "branch-list",
            &serde_json::json!({"scope": "everything"}),
            &GitCaveats::read_only(),
            &newt_core::caveats::Caveats::top()
        )
        .is_err());
    assert!(t
        .dispatch(
            "branch-list",
            &serde_json::json!({"scope": 1}),
            &GitCaveats::read_only(),
            &newt_core::caveats::Caveats::top()
        )
        .is_err());
    assert!(t
        .dispatch(
            "branch-list",
            &serde_json::json!({"name": "filtered"}),
            &GitCaveats::read_only(),
            &newt_core::caveats::Caveats::top()
        )
        .is_err());
    assert!(t
        .dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::none(),
            &newt_core::caveats::Caveats::top()
        )
        .is_err());
}

/// The real HEAD states ground the read-only count contract: neither an
/// unborn name nor a detached HEAD introduces an additional branch ref.
#[test]
fn branch_list_handles_unborn_and_detached_head() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let out = tool(dir.path())
        .dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &newt_core::caveats::Caveats::top(),
        )
        .unwrap();
    assert!(out.contains("local branches: 0"), "{out}");
    let dir = repo_with_commit();
    git(dir.path(), &["checkout", "--detach", "-q"]);
    let out = tool(dir.path())
        .dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &newt_core::caveats::Caveats::top(),
        )
        .unwrap();
    assert!(out.contains("local branches: 1"), "{out}");
}
