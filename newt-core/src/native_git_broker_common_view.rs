//! #2813: native Git commits in a private repository view, so AUTO_MERGE
//! deletion cannot lock shared packed-refs. Only a verified candidate crosses
//! back into the held real admin directory; shared refs remain broker-owned.
use super::denied;
use agent_bridle::{ToolContext, ToolResult};
use std::path::Path;

pub(super) struct CommitView {
    pub directory: tempfile::TempDir,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    admin: crate::fs_cap::WorkspaceDir,
}

impl CommitView {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn publish(&self, old: &str, new: &str) -> Result<(), String> {
        crate::git_hardening::publish_detached_head(&self.admin, old, new)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn publish(&self, _old: &str, _new: &str) -> Result<(), String> {
        Err("private commit views are unavailable on this platform".into())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn create(
    admin: &Path,
    common: &Path,
    head: &str,
    context: &ToolContext,
) -> ToolResult<CommitView> {
    context.check_path_read(admin)?;
    let held = crate::git_staging::HeldRoots::bind(&context.caveats().fs_read).map_err(denied)?;
    let packed = held
        .read_to_string(common, Path::new("packed-refs"))
        .map_err(denied)?;
    // These workflows carry mutable per-worktree state. Do not silently lose
    // it while serving an ordinary detached append/amend in a private view.
    for name in [
        "AUTO_MERGE",
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
    ] {
        match std::fs::symlink_metadata(admin.join(name)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(denied(error)),
            Ok(_) => {
                return Err(denied(
                    "finish the in-progress Git operation before running standalone git commit",
                ))
            }
        }
    }
    let root = crate::fs_cap::WorkspaceDir::open_root(admin).map_err(denied)?;
    let directory = tempfile::Builder::new()
        .prefix(".newt-commit-view-")
        .tempdir_in(admin)
        .map_err(denied)?;
    let view = directory.path();
    // Links retain the original filesystem fence. Packed refs are a snapshot:
    // Git canonicalizes a symlink before deriving its lock-file path.
    for name in ["objects", "refs", "config", "info", "hooks", "shallow"] {
        std::os::unix::fs::symlink(common.join(name), view.join(name)).map_err(denied)?;
    }
    std::os::unix::fs::symlink(admin.join("config.worktree"), view.join("config.worktree"))
        .map_err(denied)?;
    std::fs::write(view.join("HEAD"), format!("{head}\n")).map_err(denied)?;
    if let Some(packed) = packed {
        std::fs::write(view.join("packed-refs"), packed).map_err(denied)?;
    }
    Ok(CommitView {
        directory,
        admin: root,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn create(
    _admin: &Path,
    _common: &Path,
    _head: &str,
    _context: &ToolContext,
) -> ToolResult<CommitView> {
    Err(denied(
        "private commit views are unavailable on this platform",
    ))
}
