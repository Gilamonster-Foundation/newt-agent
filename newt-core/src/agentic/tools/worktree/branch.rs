//! #2748: exact new-branch commands use bounded native writes, never broad
//! refs-directory grants to a shell. All other commands retain the fence.
use super::super::{native_git::push_command::GovernedCommand, shell, PermissionRequest};
use crate::{caveats::CaveatsExt, worktree_adoption::AdoptedWorktree, Caveats, ExecOutcome};
use std::path::Path;

pub(in crate::agentic::tools) fn execute(
    policy: &AdoptedWorktree,
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    requests: &[PermissionRequest],
) -> Option<(String, ExecOutcome)> {
    let command = GovernedCommand::parse(source).ok()?;
    let words: Vec<_> = command.argv.iter().map(String::as_str).collect();
    let branch = match words.as_slice() {
        ["git", "checkout", "-b", branch]
        | ["git", "switch", "-c", branch]
        | ["git", "checkout", "-b", branch, "HEAD"]
        | ["git", "switch", "-c", branch, "HEAD"] => branch,
        _ => return None,
    };
    let result = if !caveats.permits_exec("git")
        || requests
            .iter()
            .any(|r| !shell::permits_filesystem_request(caveats, r))
    {
        Err("capability denied: branch creation requires Git execution and the declared filesystem permissions".into())
    } else {
        policy.create_branch(cwd, branch, caveats)
    };
    Some(match result {
        Ok(()) => (
            command.render(format!("Switched to a new branch '{branch}'"), true),
            ExecOutcome::Passed,
        ),
        Err(reason) => (
            command.render(format!("capability denied: {reason}"), false),
            ExecOutcome::Denied,
        ),
    })
}
