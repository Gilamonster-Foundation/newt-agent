//! Actual local repositories exercise read-only capture side effects and
//! cumulative membership. No network service or model request is involved.
use super::*;
use crate::Scope;
use content_addressable::ContentAddressable;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

pub(super) fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = crate::git_hardening::hardened_git(root, args)
        .unwrap()
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
pub(super) fn commit(root: &Path) {
    git(
        root,
        &[
            "-c",
            "user.name=Review Fixture",
            "-c",
            "user.email=review@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "fixture",
        ],
    );
}
fn repository(root: &Path) {
    std::fs::create_dir(root).unwrap();
    git(root, &["init", "-q"]);
    std::fs::write(root.join("tracked.txt"), b"review fixture raw object\n").unwrap();
    git(root, &["add", "--", "tracked.txt"]);
    commit(root);
}
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(root: &Path, at: &Path, map: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in std::fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                map.insert(path.strip_prefix(root).unwrap().to_path_buf(), None);
                walk(root, &path, map);
            } else {
                map.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    Some(std::fs::read(&path).unwrap()),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    walk(root, root, &mut result);
    result
}
async fn capture(root: &Path, limit: usize) -> Result<PresentedSubject, CaptureFailure> {
    let objective = ReviewObjective {
        instruction: "review actual source".into(),
        turn_context: "Git safety fixture".into(),
    }
    .content_id()
    .unwrap();
    capture_existing_diff(
        root,
        &Scope::All,
        objective,
        limit,
        limit,
        Duration::from_secs(10),
    )
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn review_git_safety_missing_promised_blob_cannot_fetch_or_write_administration() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::tempdir().unwrap();
    let origin = scratch.path().join("origin");
    let dest = scratch.path().join("dest");
    repository(&origin);
    repository(&dest);
    // A local existing blob must remain usable under the capture-local guard.
    capture(&origin, 65536).await.unwrap();
    let oid = String::from_utf8(git(&dest, &["rev-parse", "HEAD:tracked.txt"])).unwrap();
    let oid = oid.trim();
    let marker = scratch.path().join("upload-marker");
    let script = scratch.path().join("upload-pack");
    let program = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .unwrap()
        .canonicalize()
        .unwrap();
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf executed > {}\nexec {} upload-pack \"$@\"\n",
            quote(&marker),
            quote(&program)
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (key, value) in [
        ("core.repositoryformatversion", "1"),
        ("extensions.partialClone", "origin"),
        ("remote.origin.url", origin.to_str().unwrap()),
        ("remote.origin.promisor", "true"),
        ("remote.origin.partialclonefilter", "blob:none"),
        ("remote.origin.uploadpack", script.to_str().unwrap()),
    ] {
        git(&dest, &["config", key, value]);
    }
    std::fs::remove_file(dest.join(".git/objects").join(&oid[..2]).join(&oid[2..])).unwrap();
    let before = snapshot(&dest.join(".git"));
    let working = std::fs::read(dest.join("tracked.txt")).unwrap();
    let result = capture(&dest, 65536).await;
    assert!(
        !marker.exists(),
        "read-only capture executed repository upload-pack during lazy hydration"
    );
    assert_eq!(
        snapshot(&dest.join(".git")),
        before,
        "capture changed Git administration"
    );
    assert_eq!(std::fs::read(dest.join("tracked.txt")).unwrap(), working);
    assert!(
        matches!(result, Err(CaptureFailure::Incomplete(_))),
        "missing local object must remain incomplete: {result:?}"
    );
}

#[tokio::test]
async fn review_git_safety_combined_membership_is_capped_before_any_version_read() {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    let limit = crate::agentic::self_verify::MAX_TREE_ENTRIES;
    for index in 0..limit {
        std::fs::write(root.path().join(format!("p{index:05}")), []).unwrap();
    }
    git(root.path(), &["add", "--all"]);
    commit(root.path());
    // Every individual metadata list is valid; their union is one over cap.
    std::fs::write(root.path().join("untracked-extra"), []).unwrap();
    let listed = git(root.path(), &["ls-files", "-z"]);
    assert_eq!(
        listed
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .count(),
        limit
    );
    // This actual earliest working entry is deliberately unreadable as a file.
    // Correct cumulative membership admission rejects BEFORE touching versions;
    // the old uncapped union reaches it and returns a different capture failure.
    std::fs::remove_file(root.path().join("p00000")).unwrap();
    std::fs::create_dir(root.path().join("p00000")).unwrap();
    assert_eq!(
        capture(root.path(), 4 * 1024 * 1024).await.unwrap_err(),
        CaptureFailure::OverLimit
    );
}
