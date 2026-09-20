use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use content_addressable::ContentId;

use super::{CaptureFailure, PresentedSubject, ReviewFile, ReviewVersions};
use crate::caveats::Scope;

/// Complete observed working-file state, retained at accepted-turn entry.
/// Git administration is excluded; user files (including pre-existing dirty
/// bytes) are not silently replaced with HEAD. This is ephemeral capture data,
/// not a second persisted store or independently minted identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    files: BTreeMap<String, ReviewFile>,
    excluded_roots: BTreeSet<String>,
    tracked_overrides: BTreeSet<String>,
}

impl WorkspaceSnapshot {
    /// Recapture this accepted turn's source membership. Previously included
    /// tracked source cannot disappear from review merely by changing the index.
    ///
    /// # Errors
    /// Preserves the same authority, bounded reads and incomplete outcomes.
    pub async fn recapture(
        &self,
        workspace: &Path,
        read_scope: &Scope<String>,
        max_bytes: usize,
    ) -> Result<Self, CaptureFailure> {
        capture_with_retained_paths(workspace, read_scope, max_bytes, &self.tracked_overrides).await
    }

    /// Present only actual changes from this turn's retained baseline. Unchanged
    /// files remain baseline observations, not extra model-review material.
    ///
    /// # Errors
    /// Refuses an over-limit or unencodable subject rather than truncating it.
    pub fn changes_to(
        &self,
        current: &Self,
        objective: ContentId,
        max_bytes: usize,
    ) -> Result<PresentedSubject, CaptureFailure> {
        let paths: BTreeSet<_> = self.files.keys().chain(current.files.keys()).collect();
        let mut versions = ReviewVersions::default();
        let mut remaining = max_bytes;
        for path in paths {
            let before = self.files.get(path);
            let after = current.files.get(path);
            if before == after {
                continue;
            }
            for (version, target) in [
                (before, &mut versions.baseline),
                (after, &mut versions.current),
            ] {
                if let Some(version) = version {
                    remaining = remaining
                        .checked_sub(path.len())
                        .and_then(|left| left.checked_sub(version.bytes.len()))
                        .ok_or(CaptureFailure::OverLimit)?;
                    target.insert(path.clone(), version.clone());
                }
            }
        }
        PresentedSubject::new(objective, None, versions, max_bytes)?.with_implementation_scope(
            super::subject::ImplementationScope {
                excluded_directory_names: crate::verify_gate::SKIP_DIRS
                    .iter()
                    .map(|v| (*v).to_owned())
                    .collect(),
                baseline_excluded_roots: self.excluded_roots.clone(),
                current_excluded_roots: current.excluded_roots.clone(),
                baseline_tracked_overrides: self.tracked_overrides.clone(),
                current_tracked_overrides: current.tracked_overrides.clone(),
            },
            max_bytes,
        )
    }
}

/// Capture a complete authorized working tree twice to detect concurrent
/// membership/byte changes. Each pass has the caller's existing capture bound.
/// Enumeration errors, non-UTF8 paths, links and unsupported files fail closed.
///
/// # Errors
/// Returns denied/incomplete/binary/over-limit, never a partial tree.
pub async fn capture_workspace(
    workspace: &Path,
    read_scope: &Scope<String>,
    max_bytes: usize,
) -> Result<WorkspaceSnapshot, CaptureFailure> {
    capture_with_retained_paths(workspace, read_scope, max_bytes, &BTreeSet::new()).await
}

async fn capture_with_retained_paths(
    workspace: &Path,
    read_scope: &Scope<String>,
    max_bytes: usize,
    retained: &BTreeSet<String>,
) -> Result<WorkspaceSnapshot, CaptureFailure> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let directory = crate::agentic::open_review_directory(read_scope, workspace)?;
        let capture = async {
            let first = capture_once(&directory, read_scope, max_bytes, retained).await?;
            if first != capture_once(&directory, read_scope, max_bytes, retained).await? {
                return Err(CaptureFailure::Incomplete(
                    "workspace changed during review capture".into(),
                ));
            }
            Ok(first)
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), capture)
            .await
            .map_err(|_| CaptureFailure::Incomplete("workspace capture timed out".into()))?
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (workspace, read_scope, max_bytes, retained);
        Err(CaptureFailure::Incomplete(
            "complete confined tree capture is unavailable on this platform".into(),
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn capture_once(
    directory: &crate::fs_cap::WorkspaceDir,
    scope: &Scope<String>,
    max_bytes: usize,
    retained: &BTreeSet<String>,
) -> Result<WorkspaceSnapshot, CaptureFailure> {
    let mut files = BTreeMap::new();
    let mut excluded_roots = BTreeSet::new();
    let mut tracked_overrides = BTreeSet::new();
    let mut remaining = max_bytes;
    let mut entries = crate::agentic::self_verify::MAX_TREE_ENTRIES;
    visit(
        directory,
        Path::new(""),
        &mut remaining,
        &mut files,
        &mut excluded_roots,
        &mut entries,
    )?;
    if excluded_roots.iter().any(|path| {
        Path::new(path)
            .file_name()
            .is_some_and(|name| name != ".git")
    }) {
        // Only real metadata can distinguish tracked source under an excluded
        // build/dependency root. Narrow authority cannot be silently widened.
        let (head, index) =
            super::git_metadata::tracked_working(directory, scope, &mut remaining).await?;
        let tracked: BTreeSet<_> = head
            .keys()
            .chain(index.keys())
            .chain(retained.iter())
            .collect();
        for path in tracked {
            if !excluded_roots
                .iter()
                .any(|root| Path::new(path).starts_with(root))
            {
                continue;
            }
            entries = entries.checked_sub(1).ok_or(CaptureFailure::OverLimit)?;
            remaining = remaining
                .checked_sub(path.len())
                .ok_or(CaptureFailure::OverLimit)?;
            if let Some(version) = crate::agentic::capture_review_relative_optional(
                directory,
                Path::new(path),
                remaining,
            )? {
                remaining = remaining
                    .checked_sub(version.bytes.len())
                    .ok_or(CaptureFailure::OverLimit)?;
                files.insert(path.clone(), version);
            }
            tracked_overrides.insert(path.clone());
        }
    }
    Ok(WorkspaceSnapshot {
        files,
        excluded_roots,
        tracked_overrides,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn visit(
    directory: &crate::fs_cap::WorkspaceDir,
    prefix: &Path,
    remaining: &mut usize,
    files: &mut BTreeMap<String, ReviewFile>,
    excluded_roots: &mut BTreeSet<String>,
    entries: &mut usize,
) -> Result<(), CaptureFailure> {
    let mut names = directory
        .read_dir_bounded(Path::new("."), *entries, *remaining)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::FileTooLarge {
                CaptureFailure::OverLimit
            } else {
                CaptureFailure::Incomplete(format!("review directory listing failed: {error}"))
            }
        })?;
    names.sort();
    for name in names {
        *entries = entries.checked_sub(1).ok_or(CaptureFailure::OverLimit)?;
        let name = name.to_str().ok_or_else(|| {
            CaptureFailure::Incomplete("review directory has a non-UTF8 name".into())
        })?;
        let relative = prefix.join(name);
        let label = relative
            .to_str()
            .ok_or_else(|| CaptureFailure::Incomplete("review path is not UTF-8".into()))?;
        *remaining = remaining
            .checked_sub(label.len())
            .ok_or(CaptureFailure::OverLimit)?;
        if name == ".git" {
            excluded_roots.insert(label.to_owned());
            continue;
        }
        match directory.open_regular(Path::new(name), true) {
            Ok(file) => {
                let version = crate::agentic::capture_review_opened(file, *remaining)?;
                *remaining = remaining
                    .checked_sub(version.bytes.len())
                    .ok_or(CaptureFailure::OverLimit)?;
                files.insert(label.to_owned(), version);
            }
            Err(_) => {
                // Directory classification also resolves through the retained
                // descriptor. No ambient stat/read and no silent error skip.
                let child = directory
                    .open_dir_nofollow(Path::new(name))
                    .map_err(|error| {
                        CaptureFailure::Incomplete(format!("review entry unavailable: {error}"))
                    })?;
                if crate::verify_gate::SKIP_DIRS.contains(&name) {
                    excluded_roots.insert(label.to_owned());
                } else {
                    visit(&child, &relative, remaining, files, excluded_roots, entries)?;
                }
            }
        }
    }
    Ok(())
}
