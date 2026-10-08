//! #2813: native Git commits in a private repository view, so AUTO_MERGE
//! deletion cannot lock shared packed-refs. Only a verified candidate crosses
//! back into the held real admin directory; shared refs remain broker-owned.
use super::{denied, BrokerControl, RepositoryProbe};
use agent_bridle::{ToolContext, ToolResult};
use std::path::Path;

pub(super) struct CommitView {
    // TempDir cleans up ordinary exits on a best-effort basis; crashes,
    // filesystem errors, or a concurrent rename can leave the view behind.
    pub directory: tempfile::TempDir,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    admin: agent_bridle_fdguard::GrantedRoot,
}

impl CommitView {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub fn publish(&self, old: &str, new: &str, verified_commit: &[u8]) -> Result<(), String> {
        // Derive the entry only from the policy-verified object, never from
        // logs/HEAD in the child-writable view. Preserve its committer/date.
        if verified_commit.contains(&0) {
            return Err("commit contains embedded NUL".into());
        }
        let commit = std::str::from_utf8(verified_commit).map_err(|e| e.to_string())?;
        let (headers, message) = commit.split_once("\n\n").ok_or("commit lacks message")?;
        let mut committers = headers
            .split('\n')
            .filter_map(|line| line.strip_prefix("committer "));
        let committer = committers.next().ok_or("commit lacks committer")?;
        if committer.is_empty()
            || committer.chars().any(char::is_control)
            || committers.next().is_some()
        {
            return Err("commit has invalid reflog committer".into());
        }
        let summary = message
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let action = if super::commit_parents(verified_commit)? == [old] {
            "commit"
        } else {
            "commit (amend)"
        };
        let entry = format!("{old} {new} {committer}\t{action}: {summary}\n");
        crate::git_hardening::publish_detached_head(&self.admin, old, new, &entry)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn publish(&self, _old: &str, _new: &str, _verified_commit: &[u8]) -> Result<(), String> {
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
        admin: admin_root,
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

    /// #2813 / PR #2818 round 4: formatter rejection must be local, even if
    /// native Git already rejects NUL messages before calling the publisher.
    #[test]
    fn private_commit_formatter_refuses_nul_with_regular_control() {
        for message in [
            "subject\0injection",
            "subject\n\nbody\0injection",
            "normal subject",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let old = "1".repeat(40);
            let new = "2".repeat(40);
            std::fs::write(directory.path().join("HEAD"), &old).unwrap();
            let admin = agent_bridle_fdguard::GrantedRoot::acquire(
                &directory.path().canonicalize().unwrap(),
            )
            .unwrap();
            let view = CommitView { directory, admin };
            let commit = format!("tree {}\nparent {old}\nauthor Fixture <fixture@example.test> 1 +0000\ncommitter Fixture <fixture@example.test> 1 +0000\n\n{message}\n", "3".repeat(40));
            let result = view.publish(&old, &new, commit.as_bytes());
            if message.contains('\0') {
                assert!(result.is_err(), "formatter accepted embedded NUL");
                assert_eq!(
                    std::fs::read_to_string(view.directory.path().join("HEAD")).unwrap(),
                    old
                );
                assert!(!view.directory.path().join("logs/HEAD").exists());
                assert!(!view.directory.path().join("HEAD.lock").exists());
            } else {
                result.unwrap();
                assert_eq!(std::fs::read_to_string(view.directory.path().join("logs/HEAD")).unwrap(),
                    format!("{old} {new} Fixture <fixture@example.test> 1 +0000\tcommit: normal subject\n"));
            }
        }
    }

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
