use super::*;

/// Real alternate objects ground the scope policy: an approved repository
/// root cannot authorize the legacy engine's transitive object-store reads.
#[test]
fn scoped_legacy_git_refuses_external_alternates_before_opening_the_engine() {
    use newt_core::caveats::{Caveats, Scope};
    let external = repo_with_commit();
    let marker = "external alternate commit must not be disclosed";
    git(
        external.path(),
        &[
            "-c",
            "user.name=Tester",
            "-c",
            "user.email=t@example.com",
            "commit",
            "--amend",
            "-q",
            "-m",
            marker,
        ],
    );
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "-b", "main"]);
    std::fs::copy(
        external.path().join(".git/refs/heads/main"),
        repo.path().join(".git/refs/heads/main"),
    )
    .unwrap();
    std::fs::write(
        repo.path().join(".git/objects/info/alternates"),
        external
            .path()
            .join(".git/objects")
            .to_string_lossy()
            .as_bytes(),
    )
    .unwrap();
    let mut session = Caveats {
        fs_write: Scope::none(),
        exec: Scope::none(),
        net: Scope::none(),
        ..Caveats::top()
    };
    let unrestricted = tool(repo.path())
        .dispatch(
            "log",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &session,
        )
        .unwrap();
    assert!(
        unrestricted.contains(marker),
        "fixture must ground a real alternate read"
    );
    session.fs_read = Scope::only([repo.path().to_string_lossy().into_owned()]);
    let branches = tool(repo.path())
        .dispatch(
            "branch-list",
            &serde_json::json!({}),
            &GitCaveats::read_only(),
            &session,
        )
        .unwrap();
    assert!(branches.contains("local branches: 1"));
    for caps in [GitCaveats::read_only(), GitCaveats::top()] {
        let out = tool(repo.path()).dispatch("log", &serde_json::json!({}), &caps, &session);
        assert!(
            matches!(out, Err(ref error) if error.contains("fs_read") && !error.contains(marker)),
            "{out:?}"
        );
    }
}
