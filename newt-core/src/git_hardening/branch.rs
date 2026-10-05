//! #2748: create one absent branch at this linked worktree's HEAD, using the
//! same held-descriptor CAS and reflog protocol as commit publication.
use super::*;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn create_worktree_branch(workspace: &Path, branch: &str) -> Result<(), String> {
    use std::io::Read;
    crate::git_staging::validate_branch_name(branch)?;
    if branch.starts_with('-')
        || branch == "HEAD"
        || branch.ends_with('.')
        || branch.split('/').any(|part| part.ends_with(".lock"))
        || is_default_branch(workspace, branch)
    {
        return Err("refused: choose a new non-default branch name".into());
    }
    let (common, admin) =
        verified_identity(workspace).ok_or("refused: worktree Git identity changed")?;
    if common == admin {
        return Err("refused: branch creation requires the adopted linked worktree".into());
    }
    let identity = BoundGitIdentity::bind(common, admin)?;
    identity.verify()?;
    let mut head = String::new();
    identity
        .git_root
        .open_read(Path::new("HEAD"))
        .and_then(|mut file| file.read_to_string(&mut head))
        .map_err(|e| e.to_string())?;
    let tip = if let Some(current) = head.trim().strip_prefix("ref: refs/heads/") {
        crate::git_staging::validate_branch_name(current)?;
        read_ref_natively(&identity, current)?.ok_or("refused: current branch is unborn")?
    } else {
        head.trim().to_owned()
    };
    if !crate::git_staging::is_hex_oid(&tip) || !is_real_commit_object(&identity, &tip) {
        return Err("refused: worktree HEAD does not identify a commit".into());
    }
    // None is an absence assertion, checked under the destination's lock and
    // against loose AND packed refs. Existing refs can never be overwritten.
    advance_branch_ref_natively_seamed(
        &identity,
        branch,
        None,
        &tip,
        "branch: Created from HEAD",
        &|| {},
    )?;
    reattach_own_head(&identity, branch)
        .map_err(|e| format!("branch was created, but worktree HEAD could not be attached: {e}"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn create_worktree_branch(_workspace: &Path, _branch: &str) -> Result<(), String> {
    Err(unsupported_native_write())
}
