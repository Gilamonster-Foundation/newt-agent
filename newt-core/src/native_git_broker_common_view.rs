//! #2813: native Git commits in a private repository view, so AUTO_MERGE
//! deletion cannot lock shared packed-refs. Only a verified candidate crosses
//! back into the held real admin directory; shared refs remain broker-owned.
use super::{denied, BrokerControl, RepositoryProbe};
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
    probe: &RepositoryProbe,
    control: &BrokerControl,
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
    // The admin anchor must itself be outside replaceable child ancestors.
    // Otherwise even allocating the temporary directory could follow a swap.
    if admin
        .parent()
        .is_none_or(|parent| context.check_path_write(parent).is_ok())
    {
        return Err(denied("private commit view needs an independently confined admin directory; run standalone git commit from the task worktree"));
    }
    let admin_root = agent_bridle_fdguard::GrantedRoot::acquire(admin).map_err(denied)?;
    let root = crate::fs_cap::WorkspaceDir::from_granted_root(&admin_root).map_err(denied)?;
    let directory = tempfile::Builder::new()
        .prefix(".newt-commit-view-")
        .tempdir_in(admin)
        .map_err(denied)?;
    let view = directory.path();
    let view_root = agent_bridle_fdguard::GrantedRoot::acquire(view).map_err(denied)?;
    seed(&view_root, common, head, packed.as_deref())?;
    // Have Git encode includes naming the original config files. Symlinking
    // config would resolve relative include.path entries against this view.
    for (name, source) in [("config", common), ("config.worktree", admin)] {
        let destination = view.join(name);
        let source = source.join(name);
        probe.run(
            &[
                "config",
                "--file",
                destination
                    .to_str()
                    .ok_or_else(|| denied("non-UTF8 config path"))?,
                "--add",
                "include.path",
                source
                    .to_str()
                    .ok_or_else(|| denied("non-UTF8 config path"))?,
            ],
            control,
        )?;
    }
    // Git reads repository-format fields before processing config includes.
    // Preserve those fields directly (notably SHA-256 and worktree config).
    for key in [
        "core.repositoryformatversion",
        "extensions.objectformat",
        "extensions.worktreeconfig",
    ] {
        if let Some(value) = probe.optional_text(
            &["config", "--local", "--no-includes", "--get", key],
            control,
        )? {
            let config = view.join("config");
            probe.run(
                &[
                    "config",
                    "--file",
                    config
                        .to_str()
                        .ok_or_else(|| denied("non-UTF8 config path"))?,
                    key,
                    &value,
                ],
                control,
            )?;
        }
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
    _probe: &RepositoryProbe,
    _control: &BrokerControl,
) -> ToolResult<CommitView> {
    Err(denied(
        "private commit views are unavailable on this platform",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn seed(
    view: &agent_bridle_fdguard::GrantedRoot,
    common: &Path,
    head: &str,
    packed: Option<&str>,
) -> ToolResult<()> {
    use std::io::Write as _;
    let files = crate::fs_cap::WorkspaceDir::from_granted_root(view).map_err(denied)?;
    for name in ["objects", "refs", "info", "hooks", "shallow"] {
        rustix::fs::symlinkat(common.join(name), view.as_fd(), name).map_err(denied)?;
    }
    files
        .create_new(Path::new("HEAD"))
        .and_then(|mut file| writeln!(file, "{head}"))
        .map_err(denied)?;
    if let Some(packed) = packed {
        files
            .create_new(Path::new("packed-refs"))
            .and_then(|mut file| file.write_all(packed.as_bytes()))
            .map_err(denied)?;
    }
    Ok(())
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    /// #2813: an earlier child may replace the view pathname while the broker
    /// prepares it. Host writes must stay in the held directory, not the link.
    #[test]
    fn private_view_seed_cannot_follow_a_swapped_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let view = root.join("view");
        let held_path = root.join("held");
        let outside = root.join("outside");
        std::fs::create_dir(&view).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let held = agent_bridle_fdguard::GrantedRoot::acquire(&view).unwrap();
        std::fs::rename(&view, &held_path).unwrap();
        std::os::unix::fs::symlink(&outside, &view).unwrap();
        seed(
            &held,
            &root.join("common"),
            &"1".repeat(40),
            Some("packed snapshot"),
        )
        .unwrap();
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert!(held_path.join("HEAD").is_file());
        assert_eq!(
            std::fs::read_to_string(held_path.join("packed-refs")).unwrap(),
            "packed snapshot"
        );
    }
}
