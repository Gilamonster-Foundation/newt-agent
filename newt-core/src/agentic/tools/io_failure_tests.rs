//! #2449 implementation review: unknown IO is not evidence that nothing ran.
use super::*;

#[test]
fn grit_2449_unknown_io_preserves_absent_execution_classification() {
    for kind in [io::ErrorKind::Other, io::ErrorKind::Interrupted] {
        let slot = OnceLock::new();
        let error = io::Error::new(kind, "operation status unavailable");
        assert_eq!(
            IoFailure::io(&error, "error: operation status unavailable".into()).record(Some(&slot)),
            "error: operation status unavailable"
        );
        assert_eq!(
            slot.get(),
            None,
            "unknown error cannot claim the operation never ran"
        );
    }
}

#[test]
fn grit_2449_known_io_keeps_specific_producer_evidence() {
    for (kind, expected) in [
        (io::ErrorKind::Unsupported, ExecOutcome::Unavailable),
        (io::ErrorKind::PermissionDenied, ExecOutcome::Denied),
        (io::ErrorKind::NotFound, ExecOutcome::Failed),
        (io::ErrorKind::TimedOut, ExecOutcome::TimedOut),
    ] {
        let slot = OnceLock::new();
        let error = io::Error::new(kind, "fixture");
        IoFailure::io(&error, "opaque presentation".into()).record(Some(&slot));
        assert_eq!(slot.get(), Some(&expected));
    }
}

/// A directory iterator can fail after returning an entry. Its partial names
/// do not establish a successful listing; the producer retains the typed error.
#[test]
fn grit_2449_partial_directory_listing_preserves_entry_failure() {
    let result = directory_names([
        Ok("first.txt".to_string()),
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "entry denied",
        )),
    ]);
    let error = result.expect_err("a partial listing must not become Passed");
    let slot = OnceLock::new();
    error.record(Some(&slot));
    assert_eq!(slot.get(), Some(&ExecOutcome::Denied));
    assert_eq!(
        directory_names([Ok("first.txt".into())]).unwrap(),
        ["first.txt"]
    );
}
