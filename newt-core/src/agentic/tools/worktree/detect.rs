//! #2791/#2792: bind a directory delta only to an inspected creation destination.
//! This is advisory task routing, never confinement adoption or a grant.
use super::{invocation, literal, resembles_git, shell};
use crate::worktree_adoption::{common_git_dir, task_path_literal, WorktreeSession};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    path::{Path, PathBuf},
};

pub(super) struct Snapshot {
    directory: PathBuf,
    names: BTreeSet<OsString>,
}

fn names(directory: &Path) -> Option<BTreeSet<OsString>> {
    match std::fs::read_dir(directory) {
        Ok(entries) => entries
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<Result<_, _>>()
            .ok(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(BTreeSet::new()),
        Err(_) => None,
    }
}

impl Snapshot {
    pub(super) fn before(workspace: &Path, read: &crate::Scope<String>) -> Option<Self> {
        let directory = common_git_dir(workspace)?.join("worktrees");
        if !crate::permits_path(read, &directory.to_string_lossy()) {
            return None;
        }
        Some(Self {
            names: names(&directory)?,
            directory,
        })
    }

    pub(super) fn record(
        self,
        session: &WorktreeSession,
        workspace: &Path,
        args: &serde_json::Value,
        outcome: Option<&crate::ExecOutcome>,
    ) -> Option<String> {
        if outcome != Some(&crate::ExecOutcome::Passed)
            || session.snapshot().is_some()
            || common_git_dir(workspace)?.join("worktrees") != self.directory
        {
            return None;
        }
        let after = names(&self.directory)?;
        let mut added = after.difference(&self.names);
        let name = added.next()?;
        if added.next().is_some() {
            return Some("Multiple new worktrees appeared; task worktree routing is unchanged because the destination is ambiguous.".into());
        }
        let admin = self.directory.join(name);
        if !std::fs::symlink_metadata(&admin).ok()?.is_dir() {
            return None;
        }
        let backlink = std::fs::read_to_string(admin.join("gitdir")).ok()?;
        if backlink.trim().is_empty() {
            return None;
        }
        let gitfile = admin.join(backlink.trim()).canonicalize().ok()?;
        let root = WorktreeSession::bound_task_root(gitfile.parent()?, workspace)?;
        // The discovered entry must be the checkout's own admin, not a new
        // directory pointing at some previously existing linked checkout.
        if crate::workspace_key::resolve_gitdir_file(&root.join(".git"), &root)? != admin {
            return None;
        }
        let head = std::fs::read_to_string(admin.join("HEAD")).ok()?;
        let branch = head.trim().strip_prefix("ref: refs/heads/")?;
        crate::git_staging::validate_branch_name(branch).ok()?;
        if session.task_root(workspace).as_ref() == Some(&root) {
            return None; // The literal/adoption recorder already handled it.
        }
        if !destination_matches(args, workspace, &root) {
            return Some(format!(
                "Discovered worktree {}; no matching creation destination in this invocation, so task routing is unchanged.",
                task_path_literal(&root)
            ));
        }
        session.record_task_worktree(&root, branch);
        session.task_hint()
    }
}

/// Inspection supplies real command stages, not substrings in echo operands.
/// Unknown cwd changes/expansions/options remain advisory rather than guessed.
fn destination_matches(args: &serde_json::Value, workspace: &Path, root: &Path) -> bool {
    let Some(source) = args.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    let (cd, source) = shell::split_leading_cd(source);
    let base = shell::resolve_exec_cwd(
        &workspace.to_string_lossy(),
        args.get("cwd").and_then(|v| v.as_str()),
    );
    let cwd = PathBuf::from(shell::resolve_exec_cwd(&base, cd.as_deref()));
    let Ok(inspection) = agent_bridle::inspect_shell(&source) else {
        return false;
    };
    // Topology warnings are expected for pipelines and &&/||; the
    // invocation-specific destination, not aggregate status, attributes the delta.
    if !inspection.constructs.is_empty()
        || inspection.commands.iter().any(|c| {
            matches!(
                c.program.as_deref(),
                Some("cd" | "pushd" | "popd" | "eval" | "source" | ".")
            )
        })
    {
        return false;
    }
    inspection.commands.iter().any(|command| {
        if !command.program.as_deref().is_some_and(resembles_git)
            || !command.descendant_execs.is_empty()
            || !command
                .argv
                .first()
                .is_some_and(|word| command.source.trim_start().starts_with(word))
        {
            return false;
        }
        let Some(mut words) = command
            .argv
            .iter()
            .map(|word| literal(word))
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        let mut cwd = cwd.clone();
        // Git applies repeated -C selectors successively, without lexical
        // normalization through symlinks. Other repository selectors decline.
        while words.get(1).is_some_and(|word| word == "-C") {
            if words.len() < 3 {
                return false;
            }
            cwd = cwd.join(&words[2]);
            words.drain(1..3);
        }
        let Ok(("worktree", rest, true)) = invocation(&words) else {
            return false;
        };
        let Some(("add", rest)) = rest.split_first().map(|(a, rest)| (a.as_str(), rest)) else {
            return false;
        };
        let Some(destination) = destination_operand(rest) else {
            return false;
        };
        cwd.join(destination).canonicalize().ok().as_deref() == Some(root)
    })
}

fn destination_operand(mut args: &[String]) -> Option<&str> {
    while let Some((word, rest)) = args.split_first() {
        match word.as_str() {
            "--" => return rest.first().map(String::as_str),
            "-b" | "-B" | "--reason" => args = rest.get(1..)?,
            "-f" | "--force" | "-d" | "--detach" | "--checkout" | "--no-checkout" | "--lock"
            | "--orphan" | "-q" | "--quiet" | "--track" | "--no-track" | "--guess-remote"
            | "--no-guess-remote" => args = rest,
            word if word.starts_with("--reason=")
                || word.starts_with("--track=")
                || (word.len() > 2 && (word.starts_with("-b") || word.starts_with("-B"))) =>
            {
                args = rest;
            }
            word if !word.starts_with('-') && !word.is_empty() => return Some(word),
            _ => return None,
        }
    }
    None
}
