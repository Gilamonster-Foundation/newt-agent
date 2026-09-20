//! Bounded metadata membership; never asks Git to compare working bytes.
use super::CaptureFailure;
use crate::caveats::Scope;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub oid: String,
    pub executable: bool,
}

pub(super) async fn output(
    directory: &crate::fs_cap::WorkspaceDir,
    scope: &Scope<String>,
    args: &[&str],
    remaining: &mut usize,
) -> Result<Vec<u8>, CaptureFailure> {
    let bytes = super::git_read::output(directory, scope, args, *remaining).await?;
    *remaining = remaining
        .checked_sub(bytes.len())
        .ok_or(CaptureFailure::OverLimit)?;
    Ok(bytes)
}

pub(super) fn records(bytes: &[u8]) -> Result<Vec<&str>, CaptureFailure> {
    if !bytes.is_empty() && bytes.last() != Some(&0) {
        return Err(CaptureFailure::Incomplete(
            "Git returned incomplete path records".into(),
        ));
    }
    let mut result = Vec::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        if result.len() >= crate::agentic::self_verify::MAX_TREE_ENTRIES {
            return Err(CaptureFailure::OverLimit);
        }
        result.push(
            std::str::from_utf8(record)
                .map_err(|_| CaptureFailure::Incomplete("Git subject path is not UTF-8".into()))?,
        );
    }
    Ok(result)
}

pub(super) fn validate_path(path: &str) -> Result<(), CaptureFailure> {
    if path.is_empty()
        || Path::new(path)
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(CaptureFailure::Incomplete(
            "Git returned a non-relative subject path".into(),
        ));
    }
    Ok(())
}

pub(super) fn entries(
    bytes: &[u8],
    index: bool,
) -> Result<BTreeMap<String, Entry>, CaptureFailure> {
    let mut result = BTreeMap::new();
    for record in records(bytes)? {
        let (header, path) = record
            .split_once('\t')
            .ok_or_else(|| CaptureFailure::Incomplete("Git file metadata is malformed".into()))?;
        validate_path(path)?;
        let fields: Vec<_> = header.split_whitespace().collect();
        if fields.len() != 3
            || !matches!(fields[0], "100644" | "100755")
            || if index {
                fields[2] != "0"
            } else {
                fields[1] != "blob"
            }
        {
            return Err(CaptureFailure::Incomplete(
                "Git subject is not an unambiguous regular file".into(),
            ));
        }
        let oid = if index { fields[1] } else { fields[2] };
        // Git's object ID is a lookup input, never a new review identity.
        if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(CaptureFailure::Incomplete(
                "Git returned an invalid object identifier".into(),
            ));
        }
        if result
            .insert(
                path.to_owned(),
                Entry {
                    oid: oid.to_owned(),
                    executable: fields[0] == "100755",
                },
            )
            .is_some()
        {
            return Err(CaptureFailure::Incomplete(
                "ambiguous Git subject membership".into(),
            ));
        }
    }
    Ok(result)
}

pub(super) async fn tracked(
    directory: &crate::fs_cap::WorkspaceDir,
    scope: &Scope<String>,
    remaining: &mut usize,
) -> Result<(BTreeMap<String, Entry>, BTreeMap<String, Entry>), CaptureFailure> {
    let head = output(
        directory,
        scope,
        &["ls-tree", "-r", "-z", "HEAD", "--"],
        remaining,
    )
    .await?;
    let index = output(
        directory,
        scope,
        &["ls-files", "--stage", "-z", "--"],
        remaining,
    )
    .await?;
    Ok((entries(&head, false)?, entries(&index, true)?))
}

/// Working baseline supports genuinely absent Git administration and an unborn
/// branch. Explicit ExistingDiff continues to require the ordinary HEAD path.
pub(super) async fn tracked_working(
    directory: &crate::fs_cap::WorkspaceDir,
    scope: &Scope<String>,
    remaining: &mut usize,
) -> Result<(BTreeMap<String, Entry>, BTreeMap<String, Entry>), CaptureFailure> {
    let discovered = status_output(
        directory,
        scope,
        &["rev-parse", "--is-inside-work-tree"],
        remaining,
    )
    .await?;
    if !discovered.status.success() {
        if administration_absent(directory, scope, remaining)? {
            return Ok((BTreeMap::new(), BTreeMap::new()));
        }
        return Err(super::git_read::failure(&discovered));
    }
    if discovered.stdout != b"true\n" {
        return Err(CaptureFailure::Incomplete(
            "review root is not a Git working tree".into(),
        ));
    }
    let head = status_output(
        directory,
        scope,
        &["ls-tree", "-r", "-z", "HEAD", "--"],
        remaining,
    )
    .await?;
    let head = if head.status.success() {
        entries(&head.stdout, false)?
    } else {
        let symbolic = status_output(
            directory,
            scope,
            &["symbolic-ref", "--quiet", "HEAD"],
            remaining,
        )
        .await?;
        if !symbolic.status.success() {
            return Err(super::git_read::failure(&head));
        }
        let branch = std::str::from_utf8(&symbolic.stdout)
            .ok()
            .and_then(|value| value.strip_suffix('\n'))
            .filter(|value| value.starts_with("refs/heads/") && !value.contains('\n'))
            .ok_or_else(|| {
                CaptureFailure::Incomplete("Git HEAD is not a valid unborn branch".into())
            })?;
        validate_path(branch)?;
        let reference = status_output(
            directory,
            scope,
            &["show-ref", "--verify", "--quiet", branch],
            remaining,
        )
        .await?;
        if reference.status.code() != Some(1) {
            return Err(super::git_read::failure(&head));
        }
        BTreeMap::new()
    };
    let index = output(
        directory,
        scope,
        &["ls-files", "--stage", "-z", "--"],
        remaining,
    )
    .await?;
    Ok((head, entries(&index, true)?))
}

async fn status_output(
    directory: &crate::fs_cap::WorkspaceDir,
    scope: &Scope<String>,
    args: &[&str],
    remaining: &mut usize,
) -> Result<std::process::Output, CaptureFailure> {
    let result = super::git_read::command_output(directory, scope, args, *remaining).await?;
    *remaining = remaining
        .checked_sub(result.stdout.len())
        .ok_or(CaptureFailure::OverLimit)?;
    Ok(result)
}

/// Only absence of every administrative entry can turn failed Git discovery
/// into a non-Git workspace. Parent repositories remain Git's discovery owner.
/// Metadata paths start at the retained handle, never the workspace pathname;
/// this Scope::All-only check does not expose a broader filesystem capability.
fn administration_absent(
    directory: &crate::fs_cap::WorkspaceDir,
    scope: &Scope<String>,
    remaining: &mut usize,
) -> Result<bool, CaptureFailure> {
    use std::os::unix::fs::MetadataExt;
    crate::agentic::check_git_read_scope("metadata", scope).map_err(|_| CaptureFailure::Denied)?;
    let unavailable = |error: std::io::Error| {
        CaptureFailure::Incomplete(format!("Git discovery metadata unavailable: {error}"))
    };
    let mut ancestor = directory.command_directory().map_err(unavailable)?;
    for _ in 0..crate::agentic::self_verify::MAX_TREE_ENTRIES {
        // Bound traversal/path growth with the caller's remaining capture budget.
        *remaining = remaining.checked_sub(8).ok_or(CaptureFailure::OverLimit)?;
        match std::fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => return Ok(false), // even a broken symlink is administration
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(unavailable(error)),
        }
        let here = std::fs::metadata(&ancestor).map_err(unavailable)?;
        ancestor.push("..");
        let parent = std::fs::metadata(&ancestor).map_err(unavailable)?;
        if (here.dev(), here.ino()) == (parent.dev(), parent.ino()) {
            return Ok(true);
        }
    }
    Err(CaptureFailure::Incomplete(
        "Git discovery traversal limit exceeded".into(),
    ))
}
