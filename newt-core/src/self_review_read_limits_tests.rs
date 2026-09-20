//! Actual descriptor offsets prove the aggregate allowance reaches the reader.
use super::*;
use std::io::{Seek, SeekFrom};

#[test]
fn review_capture_rejects_oversized_version_before_reading_descriptor() {
    let mut file = tempfile::tempfile().unwrap();
    use std::io::Write;
    file.write_all(b"actual unread bytes").unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    let duplicate = file.try_clone().unwrap();
    assert_eq!(
        crate::agentic::capture_review_opened(duplicate, 1).unwrap_err(),
        CaptureFailure::OverLimit
    );
    assert_eq!(
        file.stream_position().unwrap(),
        0,
        "shared descriptor offset proves no bytes read"
    );
    let duplicate = file.try_clone().unwrap();
    assert_eq!(
        crate::agentic::capture_review_opened(duplicate, 64)
            .unwrap()
            .bytes,
        b"actual unread bytes"
    );
    assert_eq!(
        file.stream_position().unwrap(),
        19,
        "successful twin actually reads the version"
    );
}

#[test]
fn review_capture_artifact_remaining_limit_precedes_binary_classification() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a"), b"1234").unwrap();
    std::fs::write(root.path().join("b"), b"1234\0binary").unwrap();
    let objective = content_addressable::RawContentId::from_content(b"reader fixture");
    use content_addressable::ContentAddressable;
    let objective = ReviewObjective {
        instruction: "review".into(),
        turn_context: objective.to_string(),
    }
    .content_id()
    .unwrap();
    assert_eq!(
        capture_artifacts(
            root.path(),
            &crate::Scope::All,
            objective,
            &["a".into(), "b".into()],
            8
        )
        .unwrap_err(),
        CaptureFailure::OverLimit
    );
}
