//! Real source membership controls. These exercise capture, not the pending
//! turn-loop phase or production extension dispatch adapter.
use super::*;
use crate::caveats::Scope;
use content_addressable::ContentAddressable;
use std::path::Path;

fn git(root: &Path, args: &[&str]) {
    let output = crate::git_hardening::hardened_git(root, args)
        .unwrap()
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn repository() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    std::fs::write(root.path().join("source.rs"), "committed source").unwrap();
    std::fs::write(
        root.path().join(".gitignore"),
        "target/\nignored-source.rs\n",
    )
    .unwrap();
    git(root.path(), &["add", "--", "source.rs", ".gitignore"]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=Review Fixture",
            "-c",
            "user.email=review@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "fixture baseline",
        ],
    );
    root
}
fn objective() -> content_addressable::ContentId {
    ReviewObjective {
        instruction: "review implementation source".into(),
        turn_context: "source-fixture".into(),
    }
    .content_id()
    .unwrap()
}
fn present(before: &WorkspaceSnapshot, after: &WorkspaceSnapshot) -> serde_json::Value {
    serde_json::from_str(
        &before
            .changes_to(after, objective(), 16384)
            .unwrap()
            .material()
            .unwrap(),
    )
    .unwrap()
}
fn generated(root: &Path) {
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join("target/generated.bin"), [0xff, 0, 0xfe]).unwrap();
}

#[tokio::test]
async fn review_implementation_shell_change_excludes_generated_binary_output() {
    // The old physical-tree/text capture rejects an ordinary pre-existing build
    // output before it can review this small, actual shell edit.
    let root = repository();
    generated(root.path());
    std::fs::write(root.path().join("source.rs"), "operator dirty baseline").unwrap();
    let index = std::fs::read(root.path().join(".git/index")).unwrap();
    let before = capture_workspace(root.path(), &Scope::All, 65536)
        .await
        .unwrap();
    let status = std::process::Command::new("sh").arg("-c")
        .arg("printf 'actual shell correction' > source.rs; printf 'more generated output' > target/new-output")
        .current_dir(root.path()).status().unwrap();
    assert!(status.success());
    let after = capture_workspace(root.path(), &Scope::All, 65536)
        .await
        .unwrap();
    let body = present(&before, &after);
    assert_eq!(
        body["baseline"]["source.rs"]["text"],
        "operator dirty baseline"
    );
    assert_eq!(
        body["current"]["source.rs"]["text"],
        "actual shell correction"
    );
    assert!(body["current"].get("target/new-output").is_none());
    assert!(body["current"].get("target/generated.bin").is_none());
    assert!(body["implementation_scope"]["excluded_directory_names"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("target")));
    assert!(body["implementation_scope"]["current_excluded_roots"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("target")));
    let subject = before.changes_to(&after, objective(), 16384).unwrap();
    let reply = serde_json::json!({"subject": subject.content_id().unwrap(), "objective": subject.objective(), "disposition":"no_findings", "findings":[]}).to_string();
    let mut journal = crate::event_journal::Journal::new();
    let evidence = record_reply(&mut journal, &subject, &reply).unwrap();
    assert_eq!(&evidence.node.payload().coverage, subject.coverage());
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        index
    );
}

#[tokio::test]
async fn review_implementation_external_ignored_source_is_not_omitted() {
    // Git ignore rules do not declare all ignored source outside review scope.
    // Direct external writes here are capture evidence, not an assertion that
    // the production extension adapter is already wired.
    let root = repository();
    std::fs::write(
        root.path().join("ignored-source.rs"),
        "ignored dirty baseline",
    )
    .unwrap();
    let before = capture_workspace(root.path(), &Scope::All, 65536)
        .await
        .unwrap();
    std::fs::write(root.path().join("ignored-source.rs"), "external correction").unwrap();
    let after = capture_workspace(root.path(), &Scope::All, 65536)
        .await
        .unwrap();
    let body = present(&before, &after);
    assert_eq!(
        body["baseline"]["ignored-source.rs"]["text"],
        "ignored dirty baseline"
    );
    assert_eq!(
        body["current"]["ignored-source.rs"]["text"],
        "external correction"
    );
}

#[tokio::test]
async fn review_implementation_unchanged_large_binary_does_not_consume_text_budget() {
    // Baseline resources and changed-subject presentation have distinct bounds.
    // Unchanged binary source is observed but never presented as review text.
    let root = repository();
    std::fs::write(root.path().join("fixture.bin"), vec![0xff; 300 * 1024]).unwrap();
    let before = capture_workspace(root.path(), &Scope::All, 1024 * 1024)
        .await
        .unwrap();
    std::fs::write(root.path().join("source.rs"), "small change").unwrap();
    let after = capture_workspace(root.path(), &Scope::All, 1024 * 1024)
        .await
        .unwrap();
    let body = present(&before, &after);
    assert_eq!(body["current"]["source.rs"]["text"], "small change");
    assert!(body["baseline"].get("fixture.bin").is_none());
    assert!(body["current"].get("fixture.bin").is_none());
}

#[tokio::test]
async fn review_implementation_tracked_source_under_build_directory_is_retained() {
    // A basename cannot relabel tracked project source as generated output.
    let root = repository();
    generated(root.path());
    std::fs::write(
        root.path().join("target/source.rs"),
        "tracked source baseline",
    )
    .unwrap();
    git(root.path(), &["add", "-f", "--", "target/source.rs"]);
    let index = std::fs::read(root.path().join(".git/index")).unwrap();
    let before = capture_workspace(root.path(), &Scope::All, 65536)
        .await
        .unwrap();
    std::fs::write(
        root.path().join("target/source.rs"),
        "tracked source correction",
    )
    .unwrap();
    let after = capture_workspace(root.path(), &Scope::All, 65536)
        .await
        .unwrap();
    let body = present(&before, &after);
    assert_eq!(
        body["baseline"]["target/source.rs"]["text"],
        "tracked source baseline"
    );
    assert_eq!(
        body["current"]["target/source.rs"]["text"],
        "tracked source correction"
    );
    assert!(body["implementation_scope"]["current_tracked_overrides"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("target/source.rs")));
    assert!(body["current"].get("target/generated.bin").is_none());
    assert!(body["implementation_scope"]["excluded_directory_names"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("target")));
    assert!(body["implementation_scope"]["current_excluded_roots"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("target")));
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        index
    );
}

#[tokio::test]
async fn review_implementation_narrow_scope_does_not_guess_excluded_membership() {
    let root = repository();
    generated(root.path());
    let scope = Scope::only([root.path().to_string_lossy().into_owned()]);
    assert_eq!(
        capture_workspace(root.path(), &scope, 65536)
            .await
            .unwrap_err(),
        CaptureFailure::Denied
    );
}

#[tokio::test]
async fn review_implementation_recapture_retains_original_tracked_override() {
    let root = repository();
    generated(root.path());
    std::fs::write(root.path().join("target/source.rs"), "baseline source").unwrap();
    git(root.path(), &["add", "-f", "--", "target/source.rs"]);
    let before = capture_workspace(root.path(), &Scope::All, 65536)
        .await
        .unwrap();
    git(
        root.path(),
        &["rm", "--cached", "-f", "--", "target/source.rs"],
    );
    std::fs::write(
        root.path().join("target/source.rs"),
        "source after index removal",
    )
    .unwrap();
    let after = before
        .recapture(root.path(), &Scope::All, 65536)
        .await
        .unwrap();
    let body = present(&before, &after);
    assert_eq!(
        body["baseline"]["target/source.rs"]["text"],
        "baseline source"
    );
    assert_eq!(
        body["current"]["target/source.rs"]["text"],
        "source after index removal"
    );
}

#[tokio::test]
async fn review_implementation_non_git_source_with_build_output_is_usable() {
    let root = tempfile::tempdir().unwrap();
    generated(root.path());
    std::fs::write(root.path().join("source.rs"), "non-Git dirty baseline").unwrap();
    let before = capture_workspace(root.path(), &Scope::All, 65536)
        .await
        .unwrap();
    std::fs::write(root.path().join("source.rs"), "non-Git correction").unwrap();
    let after = before
        .recapture(root.path(), &Scope::All, 65536)
        .await
        .unwrap();
    let body = present(&before, &after);
    assert_eq!(
        body["baseline"]["source.rs"]["text"],
        "non-Git dirty baseline"
    );
    assert_eq!(body["current"]["source.rs"]["text"], "non-Git correction");
}

#[tokio::test]
async fn review_implementation_unborn_repository_keeps_index_tracked_source() {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    generated(root.path());
    std::fs::write(
        root.path().join("target/source.rs"),
        "unborn tracked baseline",
    )
    .unwrap();
    git(root.path(), &["add", "-f", "--", "target/source.rs"]);
    let index = std::fs::read(root.path().join(".git/index")).unwrap();
    let before = capture_workspace(root.path(), &Scope::All, 65536)
        .await
        .unwrap();
    std::fs::write(
        root.path().join("target/source.rs"),
        "unborn tracked correction",
    )
    .unwrap();
    let after = before
        .recapture(root.path(), &Scope::All, 65536)
        .await
        .unwrap();
    let body = present(&before, &after);
    assert_eq!(
        body["current"]["target/source.rs"]["text"],
        "unborn tracked correction"
    );
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        index
    );
}

#[tokio::test]
async fn review_implementation_corrupt_repository_is_not_non_git_fallback() {
    let root = repository();
    generated(root.path());
    std::fs::write(root.path().join(".git/HEAD"), "invalid HEAD contents\n").unwrap();
    assert!(matches!(
        capture_workspace(root.path(), &Scope::All, 65536).await,
        Err(CaptureFailure::Incomplete(_))
    ));
}

#[tokio::test]
async fn review_implementation_parent_repository_retains_nested_tracked_build_source() {
    let root = repository();
    let workspace = root.path().join("nested");
    std::fs::create_dir(&workspace).unwrap();
    generated(&workspace);
    std::fs::write(workspace.join("source.rs"), "nested ordinary source").unwrap();
    std::fs::write(
        workspace.join("target/source.rs"),
        "nested tracked baseline",
    )
    .unwrap();
    git(root.path(), &["add", "-f", "--", "nested/target/source.rs"]);
    let before = capture_workspace(&workspace, &Scope::All, 65536)
        .await
        .unwrap();
    std::fs::write(
        workspace.join("target/source.rs"),
        "nested tracked correction",
    )
    .unwrap();
    let after = before
        .recapture(&workspace, &Scope::All, 65536)
        .await
        .unwrap();
    let body = present(&before, &after);
    assert_eq!(
        body["baseline"]["target/source.rs"]["text"],
        "nested tracked baseline"
    );
    assert_eq!(
        body["current"]["target/source.rs"]["text"],
        "nested tracked correction"
    );
}

#[tokio::test]
async fn review_implementation_corrupt_parent_repository_is_not_non_git_fallback() {
    let root = repository();
    let workspace = root.path().join("nested");
    std::fs::create_dir(&workspace).unwrap();
    generated(&workspace);
    std::fs::write(workspace.join("source.rs"), "nested source").unwrap();
    std::fs::write(root.path().join(".git/HEAD"), "invalid HEAD contents\n").unwrap();
    assert!(matches!(
        capture_workspace(&workspace, &Scope::All, 65536).await,
        Err(CaptureFailure::Incomplete(_))
    ));
}
