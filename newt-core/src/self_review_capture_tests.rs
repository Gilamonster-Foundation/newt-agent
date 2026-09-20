//! Actual confined artifact reads; no model or scheduler execution claimed.
use super::*;
use crate::{caveats::Scope, event_journal::Journal};
use content_addressable::{ContentAddressable, ContentId};

fn objective() -> ContentId {
    ReviewObjective {
        instruction: "review the supplied artifacts".into(),
        turn_context: "fixture-turn".into(),
    }
    .content_id()
    .unwrap()
}
fn read_scope(root: &std::path::Path) -> Scope<String> {
    Scope::only([root.to_string_lossy().into_owned()])
}
fn capture(
    root: &std::path::Path,
    scope: &Scope<String>,
    paths: &[&str],
    limit: usize,
) -> Result<PresentedSubject, CaptureFailure> {
    capture_artifacts(
        root,
        scope,
        objective(),
        &paths
            .iter()
            .map(|path| (*path).to_owned())
            .collect::<Vec<_>>(),
        limit,
    )
}

#[test]
fn review_capture_actual_files_preserves_bytes_and_empty_membership() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("source.rs"), "actual bytes\n").unwrap();
    std::fs::write(dir.path().join("empty"), "").unwrap();
    let scope = read_scope(dir.path());
    let subject = capture(dir.path(), &scope, &["source.rs", "empty"], 4096).unwrap();
    let material: serde_json::Value = serde_json::from_str(&subject.material().unwrap()).unwrap();
    assert_eq!(material["current"]["source.rs"]["text"], "actual bytes\n");
    assert_eq!(material["current"]["empty"]["text"], "");
    assert_eq!(
        std::fs::read(dir.path().join("source.rs")).unwrap(),
        b"actual bytes\n"
    );
    assert_eq!(std::fs::read(dir.path().join("empty")).unwrap(), b"");
}

#[test]
fn review_capture_denied_scope_does_not_become_empty_subject() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("secret"), "private bytes").unwrap();
    assert_eq!(
        capture(
            dir.path(),
            &Scope::only(Vec::<String>::new()),
            &["secret"],
            4096
        )
        .unwrap_err(),
        CaptureFailure::Denied
    );
}

#[test]
fn review_capture_missing_artifact_and_failed_open_are_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let scope = read_scope(dir.path());
    std::fs::create_dir(dir.path().join("directory")).unwrap();
    for path in ["missing", "directory"] {
        assert!(matches!(
            capture(dir.path(), &scope, &[path], 4096),
            Err(CaptureFailure::Incomplete(_))
        ));
    }
}

#[test]
fn review_capture_binary_and_file_limit_are_not_partial_success() {
    let dir = tempfile::tempdir().unwrap();
    let scope = read_scope(dir.path());
    std::fs::write(dir.path().join("nul"), b"valid utf8\0binary").unwrap();
    std::fs::write(dir.path().join("nonutf8"), [0xff]).unwrap();
    std::fs::write(dir.path().join("large"), vec![b'x'; 256 * 1024 + 1]).unwrap();
    assert_eq!(
        capture(dir.path(), &scope, &["nul"], 4096).unwrap_err(),
        CaptureFailure::Binary
    );
    for path in ["nonutf8", "large"] {
        assert!(matches!(
            capture(dir.path(), &scope, &[path], 1024 * 1024),
            Err(CaptureFailure::Incomplete(_))
        ));
    }
}

#[test]
fn review_capture_fresh_actual_change_refuses_previous_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.rs");
    std::fs::write(&path, "before").unwrap();
    let scope = read_scope(dir.path());
    let original = capture(dir.path(), &scope, &["source.rs"], 4096).unwrap();
    let mut journal = Journal::new();
    let raw = serde_json::json!({"subject": original.content_id().unwrap(), "objective": objective(), "disposition": "no_findings", "findings": []}).to_string();
    let line = record_reply(&mut journal, &original, &raw).unwrap();
    let expected = *journal.head().unwrap();
    assert!(consume_evidence(&line, expected, objective(), || capture(
        dir.path(),
        &scope,
        &["source.rs"],
        4096
    ))
    .is_ok());
    std::fs::write(&path, "changed outside the tool hook").unwrap();
    assert_eq!(
        consume_evidence(&line, expected, objective(), || capture(
            dir.path(),
            &scope,
            &["source.rs"],
            4096
        ))
        .unwrap_err(),
        ReviewFailure::StaleSubject
    );
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(
        consume_evidence(&line, expected, objective(), || capture(
            dir.path(),
            &scope,
            &["source.rs"],
            4096
        )),
        Err(ReviewFailure::Capture(CaptureFailure::Incomplete(_)))
    ));
}

#[cfg(unix)]
#[test]
fn review_capture_symlink_escape_cannot_disclose_outside_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "outside bytes").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link")).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("outside")).unwrap();
    for path in ["link", "outside/secret"] {
        assert!(capture(dir.path(), &read_scope(dir.path()), &[path], 4096).is_err());
    }
}
