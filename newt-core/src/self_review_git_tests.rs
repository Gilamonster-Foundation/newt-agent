//! Real temporary Git repositories ground the read-only capture contract:
//! staged, unstaged and untracked subjects are observed without index writes,
//! commits, external diff/textconv, or widening the original read authority.
use super::*;
use crate::caveats::Scope;
use content_addressable::ContentAddressable;
use std::{path::Path, time::Duration};

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let out = crate::git_hardening::hardened_git(root, args)
        .unwrap()
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}
fn repository() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    std::fs::write(root.path().join("tracked.txt"), "committed version\n").unwrap();
    std::fs::write(root.path().join("deleted.txt"), "").unwrap();
    git(root.path(), &["add", "--", "tracked.txt", "deleted.txt"]);
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
        instruction: "review existing work without editing".into(),
        turn_context: "fixture-existing-diff".into(),
    }
    .content_id()
    .unwrap()
}
async fn capture(
    root: &Path,
    scope: &Scope<String>,
    limit: usize,
) -> Result<PresentedSubject, CaptureFailure> {
    capture_existing_diff(
        root,
        scope,
        objective(),
        limit,
        limit,
        Duration::from_secs(10),
    )
    .await
}
fn material(subject: &PresentedSubject) -> serde_json::Value {
    serde_json::from_str(&subject.material().unwrap()).unwrap()
}

#[tokio::test]
async fn review_git_preserves_index_worktree_and_all_three_subject_versions() {
    let root = repository();
    std::fs::write(root.path().join("tracked.txt"), "staged version\n").unwrap();
    git(root.path(), &["add", "--", "tracked.txt"]);
    std::fs::write(root.path().join("tracked.txt"), "unstaged version\n").unwrap();
    std::fs::write(root.path().join("untracked.txt"), "new file\n").unwrap();
    std::fs::write(root.path().join("empty.txt"), "").unwrap();
    std::fs::remove_file(root.path().join("deleted.txt")).unwrap();
    let before_index = std::fs::read(root.path().join(".git/index")).unwrap();
    let before_head = git(root.path(), &["rev-parse", "HEAD"]);
    let before_files: Vec<_> = ["tracked.txt", "untracked.txt", "empty.txt"]
        .into_iter()
        .map(|path| (path, std::fs::read(root.path().join(path)).unwrap()))
        .collect();
    let subject = capture(root.path(), &Scope::All, 65536).await.unwrap();
    let body = material(&subject);
    assert_eq!(
        body["baseline"]["tracked.txt"]["text"],
        "committed version\n"
    );
    assert_eq!(body["index"]["tracked.txt"]["text"], "staged version\n");
    assert_eq!(body["current"]["tracked.txt"]["text"], "unstaged version\n");
    assert_eq!(body["current"]["untracked.txt"]["text"], "new file\n");
    assert_eq!(body["current"]["empty.txt"]["text"], "");
    assert_eq!(body["baseline"]["deleted.txt"]["text"], "");
    assert!(body["current"].get("deleted.txt").is_none());
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        before_index
    );
    assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), before_head);
    for (path, bytes) in before_files {
        assert_eq!(std::fs::read(root.path().join(path)).unwrap(), bytes);
    }
    assert!(!root.path().join("deleted.txt").exists());
}

#[tokio::test]
async fn review_git_empty_diff_is_valid_without_commit_or_index_mutation() {
    let root = repository();
    let index = std::fs::read(root.path().join(".git/index")).unwrap();
    let subject = capture(root.path(), &Scope::All, 65536).await.unwrap();
    let body = material(&subject);
    for version in ["baseline", "index", "current"] {
        assert!(body[version].as_object().unwrap().is_empty());
    }
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        index
    );
}

#[tokio::test]
async fn review_git_index_change_survives_worktree_restoring_head_bytes() {
    let root = repository();
    std::fs::write(root.path().join("tracked.txt"), "staged only").unwrap();
    git(root.path(), &["add", "--", "tracked.txt"]);
    std::fs::write(root.path().join("tracked.txt"), "committed version\n").unwrap();
    let body = material(&capture(root.path(), &Scope::All, 65536).await.unwrap());
    assert_eq!(
        body["baseline"]["tracked.txt"]["text"],
        body["current"]["tracked.txt"]["text"]
    );
    assert_eq!(body["index"]["tracked.txt"]["text"], "staged only");
}

#[tokio::test]
async fn review_git_narrow_grant_is_denied_without_metadata_side_effects() {
    let root = repository();
    let before = std::fs::read(root.path().join(".git/index")).unwrap();
    let scope = Scope::only([root.path().to_string_lossy().into_owned()]);
    assert_eq!(
        capture(root.path(), &scope, 65536).await.unwrap_err(),
        CaptureFailure::Denied
    );
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        before
    );
}

#[tokio::test]
async fn review_git_does_not_run_textconv_or_external_diff() {
    let root = repository();
    let marker = root.path().join("executed");
    let gadget = format!("sh -c 'touch {}'", marker.display());
    git(root.path(), &["config", "diff.external", &gadget]);
    git(root.path(), &["config", "diff.review.textconv", &gadget]);
    std::fs::write(root.path().join(".gitattributes"), "*.txt diff=review\n").unwrap();
    std::fs::write(root.path().join("tracked.txt"), "actual updated text").unwrap();
    let subject = capture(root.path(), &Scope::All, 65536).await.unwrap();
    assert_eq!(
        material(&subject)["current"]["tracked.txt"]["text"],
        "actual updated text"
    );
    assert!(!marker.exists());
}

#[tokio::test]
async fn review_git_binary_over_limit_and_missing_head_are_incomplete() {
    let root = repository();
    std::fs::write(root.path().join("tracked.txt"), b"binary\0data").unwrap();
    assert_eq!(
        capture(root.path(), &Scope::All, 65536).await.unwrap_err(),
        CaptureFailure::Binary
    );
    std::fs::write(root.path().join("tracked.txt"), vec![b'x'; 4096]).unwrap();
    assert_eq!(
        capture(root.path(), &Scope::All, 512).await.unwrap_err(),
        CaptureFailure::OverLimit
    );
    let unborn = tempfile::tempdir().unwrap();
    git(unborn.path(), &["init", "-q"]);
    assert!(matches!(
        capture(unborn.path(), &Scope::All, 65536).await,
        Err(CaptureFailure::Incomplete(_))
    ));
}

#[tokio::test]
async fn review_git_assume_unchanged_cannot_hide_actual_subject_changes() {
    let root = repository();
    git(
        root.path(),
        &["update-index", "--assume-unchanged", "--", "tracked.txt"],
    );
    std::fs::write(root.path().join("tracked.txt"), "hidden actual change").unwrap();
    let before_index = std::fs::read(root.path().join(".git/index")).unwrap();
    let subject = capture(root.path(), &Scope::All, 65536).await.unwrap();
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        before_index
    );
    assert_eq!(
        material(&subject)["current"]["tracked.txt"]["text"],
        "hidden actual change",
        "index promises must not hide authorized actual working bytes"
    );
}

#[tokio::test]
async fn review_git_skip_worktree_cannot_hide_actual_subject_changes() {
    let root = repository();
    git(
        root.path(),
        &["update-index", "--skip-worktree", "--", "tracked.txt"],
    );
    std::fs::write(root.path().join("tracked.txt"), "hidden actual change").unwrap();
    let before_index = std::fs::read(root.path().join(".git/index")).unwrap();
    let subject = capture(root.path(), &Scope::All, 65536).await.unwrap();
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        before_index
    );
    assert_eq!(
        material(&subject)["current"]["tracked.txt"]["text"],
        "hidden actual change",
        "index promises must not hide authorized actual working bytes"
    );
}

/// #2449: an aggregate capture limit must stop before reading later blobs,
/// rather than allocating each whole blob and rejecting their final sum.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn review_git_aggregate_limit_stops_before_later_blob_read() {
    use std::os::unix::fs::PermissionsExt;
    const CHILD: &str = "NEWT_REVIEW_AGGREGATE_CAPTURE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "self_review::git_tests::review_git_aggregate_limit_stops_before_later_blob_read",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated capture assertion failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let _environment = crate::process_env::lock();
    let root = repository();
    std::fs::write(root.path().join("a"), vec![b'a'; 64]).unwrap();
    std::fs::write(root.path().join("b"), vec![b'b'; 64]).unwrap();
    git(root.path(), &["add", "--", "a", "b"]);
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
            "bounded fixture",
        ],
    );
    let first_oid = String::from_utf8(git(root.path(), &["rev-parse", "HEAD:a"])).unwrap();
    let second_oid = String::from_utf8(git(root.path(), &["rev-parse", "HEAD:b"])).unwrap();
    std::fs::write(root.path().join("a"), vec![b'A'; 64]).unwrap();
    std::fs::write(root.path().join("b"), vec![b'B'; 64]).unwrap();
    let original_path = std::env::var_os("PATH").unwrap();
    let real_git = std::env::split_paths(&original_path)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .unwrap()
        .canonicalize()
        .unwrap();
    let tools = tempfile::tempdir().unwrap();
    let calls = tools.path().join("calls");
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexec {} \"$@\"\n",
        quote(&calls),
        quote(&real_git)
    );
    let shim = tools.path().join("git");
    std::fs::write(&shim, script).unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    struct RestorePath(std::ffi::OsString);
    impl Drop for RestorePath {
        fn drop(&mut self) {
            crate::process_env::set_var("PATH", &self.0.to_string_lossy());
        }
    }
    let _restore = RestorePath(original_path.clone());
    let mut search = vec![tools.path().to_path_buf()];
    search.extend(std::env::split_paths(&original_path));
    crate::process_env::set_var(
        "PATH",
        &std::env::join_paths(search).unwrap().to_string_lossy(),
    );
    assert_eq!(
        capture(root.path(), &Scope::All, 700).await.unwrap_err(),
        CaptureFailure::OverLimit
    );
    let calls = std::fs::read_to_string(calls).unwrap();
    assert!(
        calls.contains(&format!("cat-file blob {}", first_oid.trim())),
        "the fixture must reach an earlier actual blob read: {calls}"
    );
    assert!(
        !calls.contains(&format!("cat-file blob {}", second_oid.trim())),
        "aggregate limit was checked only after reading the later blob: {calls}"
    );
}

/// #2449: worktree Git diff can execute a repository clean filter even with
/// external-diff/textconv disabled. Capture must read raw versions instead.
#[tokio::test]
async fn review_git_clean_filter_cannot_execute_or_normalize_the_subject() {
    let root = repository();
    let marker = root.path().join("filter-executed");
    let filter = format!(
        "touch '{}'; cat",
        marker.to_string_lossy().replace('\'', "'\\''")
    );
    git(root.path(), &["config", "filter.review.clean", &filter]);
    std::fs::write(
        root.path().join(".gitattributes"),
        "tracked.txt filter=review\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("tracked.txt"),
        "actual unnormalized working bytes\n",
    )
    .unwrap();
    let index = std::fs::read(root.path().join(".git/index")).unwrap();
    let subject = capture(root.path(), &Scope::All, 65536).await.unwrap();
    assert!(
        !marker.exists(),
        "read-only review executed the repository clean filter"
    );
    assert_eq!(
        material(&subject)["current"]["tracked.txt"]["text"],
        "actual unnormalized working bytes\n"
    );
    assert_eq!(
        std::fs::read(root.path().join(".git/index")).unwrap(),
        index
    );
}

#[tokio::test]
async fn review_git_capture_budget_is_separate_from_changed_text_budget() {
    let root = repository();
    std::fs::write(root.path().join("unchanged.bin"), vec![0xff; 300 * 1024]).unwrap();
    git(root.path(), &["add", "--", "unchanged.bin"]);
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
            "large baseline",
        ],
    );
    std::fs::write(root.path().join("tracked.txt"), "small actual change").unwrap();
    let subject = capture_existing_diff(
        root.path(),
        &Scope::All,
        objective(),
        1024 * 1024,
        16384,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    let body = material(&subject);
    assert_eq!(
        body["current"]["tracked.txt"]["text"],
        "small actual change"
    );
    assert!(body["current"].get("unchanged.bin").is_none());
    assert!(body["baseline"].get("unchanged.bin").is_none());
}
