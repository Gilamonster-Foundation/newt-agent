#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use content_addressable::ContentAddressable;
use content_addressable::ContentId;

use super::{CaptureFailure, PresentedSubject};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use super::{ReviewFile, ReviewSubject, ReviewVersions};
use crate::caveats::Scope;

/// Capture the explicit existing staged/unstaged/untracked diff without writes.
/// HEAD, index and current versions remain separate. The existing metadata
/// authority check intentionally refuses bounded fs_read grants: hardening Git
/// execution is not confinement of its config/object/alternate reads.
///
/// # Errors
/// Denied, incomplete, binary or over-limit capture cannot satisfy review. A
/// missing HEAD is reported incomplete rather than invented as an empty tree.
pub async fn capture_existing_diff(
    workspace: &Path,
    read_scope: &Scope<String>,
    objective: ContentId,
    capture_bytes: usize,
    max_bytes: usize,
    timeout: Duration,
) -> Result<PresentedSubject, CaptureFailure> {
    crate::agentic::check_git_read_scope("metadata", read_scope)
        .map_err(|_| CaptureFailure::Denied)?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let directory = crate::agentic::open_review_directory(read_scope, workspace)?;
        let capture = async {
            let first =
                capture_once(&directory, read_scope, objective, capture_bytes, max_bytes).await?;
            let second =
                capture_once(&directory, read_scope, objective, capture_bytes, max_bytes).await?;
            let id = |subject: &PresentedSubject| {
                subject
                    .content_id()
                    .map_err(|error| CaptureFailure::Incomplete(error.to_string()))
            };
            if id(&first)? != id(&second)? {
                return Err(CaptureFailure::Incomplete(
                    "Git subject changed during review capture".into(),
                ));
            }
            Ok(first)
        };
        tokio::time::timeout(timeout, capture)
            .await
            .map_err(|_| CaptureFailure::Incomplete("Git review capture timed out".into()))?
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (workspace, objective, capture_bytes, max_bytes, timeout);
        Err(CaptureFailure::Incomplete(
            "retained-directory Git capture is unavailable on this platform".into(),
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn capture_once(
    directory: &crate::fs_cap::WorkspaceDir,
    scope: &Scope<String>,
    objective: ContentId,
    capture_bytes: usize,
    max_bytes: usize,
) -> Result<PresentedSubject, CaptureFailure> {
    use super::git_metadata::{output, records, tracked, validate_path};
    let mut remaining = capture_bytes;
    let (head, index) = tracked(directory, scope, &mut remaining).await?;
    let mut paths = BTreeSet::new();
    for path in head.keys().chain(index.keys()) {
        admit_path(&mut paths, path)?;
    }
    let untracked = output(
        directory,
        scope,
        &["ls-files", "--others", "--exclude-standard", "-z"],
        &mut remaining,
    )
    .await?;
    for path in records(&untracked)? {
        validate_path(path)?;
        admit_path(&mut paths, path)?;
    }
    let mut versions = ReviewVersions {
        index: Some(Default::default()),
        ..ReviewVersions::default()
    };
    for path in paths {
        remaining = remaining
            .checked_sub(path.len())
            .ok_or(CaptureFailure::OverLimit)?;
        let before = read_blob(directory, scope, head.get(&path), &mut remaining).await?;
        let staged = if head.get(&path) == index.get(&path) {
            // Debit before cloning, not after the allocation.
            remaining = remaining
                .checked_sub(before.as_ref().map_or(0, |v| v.bytes.len()))
                .ok_or(CaptureFailure::OverLimit)?;
            before.clone()
        } else {
            read_blob(directory, scope, index.get(&path), &mut remaining).await?
        };
        let current = crate::agentic::capture_review_relative_optional(
            directory,
            Path::new(&path),
            remaining,
        )?;
        remaining = remaining
            .checked_sub(current.as_ref().map_or(0, |v| v.bytes.len()))
            .ok_or(CaptureFailure::OverLimit)?;
        // Compare raw bytes/mode before requiring text. Git index flags, clean
        // filters and core.filemode cannot conceal actual working versions.
        if before == staged && before == current {
            continue;
        }
        if let Some(file) = before {
            versions.baseline.insert(path.clone(), file);
        }
        if let Some(file) = staged {
            versions
                .index
                .as_mut()
                .expect("index exists")
                .insert(path.clone(), file);
        }
        if let Some(file) = current {
            versions.current.insert(path, file);
        }
    }
    PresentedSubject::new(
        objective,
        Some(ReviewSubject::ExistingDiff),
        versions,
        max_bytes,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn read_blob(
    directory: &crate::fs_cap::WorkspaceDir,
    scope: &Scope<String>,
    entry: Option<&super::git_metadata::Entry>,
    remaining: &mut usize,
) -> Result<Option<ReviewFile>, CaptureFailure> {
    let Some(entry) = entry else {
        return Ok(None);
    };
    // Establish object size before fetching bytes; a remaining allowance below
    // that size never starts a blob read. Both metadata and bytes are charged.
    let size =
        super::git_metadata::output(directory, scope, &["cat-file", "-s", &entry.oid], remaining)
            .await?;
    let size = std::str::from_utf8(&size)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .ok_or_else(|| CaptureFailure::Incomplete("Git blob size is invalid".into()))?;
    if size > *remaining {
        return Err(CaptureFailure::OverLimit);
    }
    let bytes = super::git_metadata::output(
        directory,
        scope,
        &["cat-file", "blob", &entry.oid],
        remaining,
    )
    .await?;
    if bytes.len() != size {
        return Err(CaptureFailure::Incomplete(
            "Git blob changed during capture".into(),
        ));
    }
    Ok(Some(ReviewFile {
        bytes,
        executable: entry.executable,
    }))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn admit_path(paths: &mut BTreeSet<String>, path: &str) -> Result<(), CaptureFailure> {
    if !paths.contains(path) {
        if paths.len() >= crate::agentic::self_verify::MAX_TREE_ENTRIES {
            return Err(CaptureFailure::OverLimit);
        }
        paths.insert(path.to_owned());
    }
    Ok(())
}
