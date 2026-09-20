//! Real filesystem controls ground complete baseline-relative capture. They do
//! not claim the turn-loop phase or scheduler completion adapter is wired.
use super::*;
use crate::caveats::Scope;
use content_addressable::ContentAddressable;

fn objective() -> content_addressable::ContentId {
    ReviewObjective {
        instruction: "review this turn's changes".into(),
        turn_context: "actual-test-turn".into(),
    }
    .content_id()
    .unwrap()
}
fn scope(root: &std::path::Path) -> Scope<String> {
    Scope::only([root.to_string_lossy().into_owned()])
}

#[tokio::test]
async fn review_tree_uses_actual_dirty_baseline_and_omits_unchanged_files() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("dirty.rs"),
        "operator's pre-existing changes",
    )
    .unwrap();
    std::fs::write(root.path().join("untouched.rs"), "unchanged").unwrap();
    let grant = scope(root.path());
    let baseline = capture_workspace(root.path(), &grant, 16384).await.unwrap();
    std::fs::write(root.path().join("dirty.rs"), "actual turn correction").unwrap();
    let current = capture_workspace(root.path(), &grant, 16384).await.unwrap();
    let subject = baseline.changes_to(&current, objective(), 16384).unwrap();
    let material: serde_json::Value = serde_json::from_str(&subject.material().unwrap()).unwrap();
    assert_eq!(
        material["baseline"]["dirty.rs"]["text"],
        "operator's pre-existing changes"
    );
    assert_eq!(
        material["current"]["dirty.rs"]["text"],
        "actual turn correction"
    );
    assert!(material["baseline"].get("untouched.rs").is_none());
    assert!(material["current"].get("untouched.rs").is_none());
}

#[tokio::test]
async fn review_tree_tracks_empty_create_delete_and_external_changes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("deleted"), "").unwrap();
    let grant = scope(root.path());
    let baseline = capture_workspace(root.path(), &grant, 16384).await.unwrap();
    std::fs::remove_file(root.path().join("deleted")).unwrap();
    std::fs::write(root.path().join("created"), "").unwrap();
    let current = capture_workspace(root.path(), &grant, 16384).await.unwrap();
    let subject = baseline.changes_to(&current, objective(), 16384).unwrap();
    let material: serde_json::Value = serde_json::from_str(&subject.material().unwrap()).unwrap();
    assert_eq!(material["baseline"]["deleted"]["text"], "");
    assert!(material["current"].get("deleted").is_none());
    assert_eq!(material["current"]["created"]["text"], "");
    assert!(material["baseline"].get("created").is_none());
    std::fs::write(root.path().join("created"), "shell/extension/external edit").unwrap();
    let changed = capture_workspace(root.path(), &grant, 16384).await.unwrap();
    assert_ne!(
        subject.content_id().unwrap(),
        baseline
            .changes_to(&changed, objective(), 16384)
            .unwrap()
            .content_id()
            .unwrap()
    );
}

#[tokio::test]
async fn review_tree_ordinary_reads_leave_subject_identity_unchanged() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("file"), "actual").unwrap();
    let grant = scope(root.path());
    let baseline = capture_workspace(root.path(), &grant, 16384).await.unwrap();
    let before = baseline.changes_to(&baseline, objective(), 16384).unwrap();
    std::fs::read(root.path().join("file")).unwrap();
    let after = baseline
        .changes_to(
            &capture_workspace(root.path(), &grant, 16384).await.unwrap(),
            objective(),
            16384,
        )
        .unwrap();
    assert_eq!(before.content_id().unwrap(), after.content_id().unwrap());
}

#[tokio::test]
async fn review_tree_mode_change_is_actual_version_change() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("script");
    std::fs::write(&path, "same bytes").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let grant = scope(root.path());
    let baseline = capture_workspace(root.path(), &grant, 16384).await.unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let current = capture_workspace(root.path(), &grant, 16384).await.unwrap();
    let material: serde_json::Value = serde_json::from_str(
        &baseline
            .changes_to(&current, objective(), 16384)
            .unwrap()
            .material()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(material["baseline"]["script"]["executable"], false);
    assert_eq!(material["current"]["script"]["executable"], true);
}

#[tokio::test]
async fn review_tree_refuses_missing_denied_nonutf8_links_and_budget() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    assert!(
        capture_workspace(&root.path().join("missing"), &scope(root.path()), 16384)
            .await
            .is_err()
    );
    assert_eq!(
        capture_workspace(root.path(), &Scope::only(Vec::<String>::new()), 16384)
            .await
            .unwrap_err(),
        CaptureFailure::Denied
    );
    std::fs::write(root.path().join("file"), "long enough").unwrap();
    assert_eq!(
        capture_workspace(root.path(), &scope(root.path()), 1)
            .await
            .unwrap_err(),
        CaptureFailure::OverLimit
    );
    let bad = root.path().join(std::ffi::OsString::from_vec(vec![0xff]));
    std::fs::write(&bad, "unknown name").unwrap();
    assert!(matches!(
        capture_workspace(root.path(), &scope(root.path()), 16384).await,
        Err(CaptureFailure::Incomplete(_))
    ));
    std::fs::remove_file(bad).unwrap();
    std::os::unix::fs::symlink(".", root.path().join("cycle")).unwrap();
    assert!(matches!(
        capture_workspace(root.path(), &scope(root.path()), 16384).await,
        Err(CaptureFailure::Incomplete(_))
    ));
}
