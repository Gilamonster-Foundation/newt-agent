use std::path::Path;

use content_addressable::ContentId;

use super::{CaptureFailure, PresentedSubject, ReviewSubject, ReviewVersions};
use crate::caveats::Scope;

/// Observe explicit supplied artifacts through the existing bounded file
/// capability. This read-only adapter does not infer a write lane or request
/// additional authority. A disappeared artifact makes recapture incomplete.
///
/// # Errors
/// Returns denied/incomplete/binary/over-limit rather than partial material.
pub fn capture_artifacts(
    workspace: &Path,
    read_scope: &Scope<String>,
    objective: ContentId,
    paths: &[String],
    max_bytes: usize,
) -> Result<PresentedSubject, CaptureFailure> {
    if paths.is_empty() || paths.iter().any(|path| path.trim().is_empty()) {
        return Err(CaptureFailure::Incomplete(
            "review artifacts require nonempty paths".into(),
        ));
    }
    let mut versions = ReviewVersions::default();
    let mut remaining = max_bytes;
    for path in paths {
        if versions.current.contains_key(path) {
            continue;
        }
        remaining = remaining
            .checked_sub(path.len())
            .ok_or(CaptureFailure::OverLimit)?;
        let bytes =
            crate::agentic::capture_review_file(read_scope, &workspace.join(path), remaining)?;
        remaining = remaining
            .checked_sub(bytes.bytes.len())
            .ok_or(CaptureFailure::OverLimit)?;
        versions.current.insert(path.clone(), bytes);
    }
    PresentedSubject::new(
        objective,
        Some(ReviewSubject::Artifacts {
            paths: paths.to_vec(),
        }),
        versions,
        max_bytes,
    )
}
