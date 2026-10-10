//! #2748: exact new-branch commands use bounded native writes, never broad
//! refs-directory grants to a shell. All other commands retain the fence.
use super::super::{native_git::push_command::GovernedCommand, shell, PermissionRequest};
use crate::{caveats::CaveatsExt, worktree_adoption::AdoptedWorktree, Caveats, ExecOutcome};
use std::path::Path;

#[path = "branch_intent.rs"]
mod intent;

pub(in crate::agentic::tools) fn execute(
    policy: &AdoptedWorktree,
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    requests: &[PermissionRequest],
) -> Option<(String, ExecOutcome)> {
    if let Some(reason) = wrapper_refusal_with(policy, source, &|name, checkout| {
        known_operand(cwd, caveats, name, checkout)
    }) {
        return Some((reason, ExecOutcome::Denied));
    }
    let command = GovernedCommand::parse(source).ok()?;
    let words: Vec<_> = command.argv.iter().map(String::as_str).collect();
    let branch = match words.as_slice() {
        ["git", "checkout", "-b", branch]
        | ["git", "switch", "-c", branch]
        | ["git", "checkout", "-b", branch, "HEAD"]
        | ["git", "switch", "-c", branch, "HEAD"] => branch,
        // This repair governs wrappers only; unsupported standalone Git
        // forms retain their existing confined execution and diagnostics.
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

/// Shared diagnostic for both the early administration refusal and branch
/// dispatch. This helper never runs a command or grants native write authority.
pub(in crate::agentic::tools) fn wrapper_refusal(
    policy: &AdoptedWorktree,
    source: &str,
) -> Option<String> {
    wrapper_refusal_with(policy, source, &|_, _| false)
}

fn known_operand(cwd: &Path, caveats: &Caveats, name: &str, checkout: bool) -> bool {
    let Ok(held) = crate::git_staging::HeldRoots::bind(&caveats.fs_read) else {
        return false;
    };
    let Ok(cwd) = cwd.canonicalize() else {
        return false;
    };
    let Ok((common, _)) = crate::git_staging::discover_git_dirs(&cwd, &held) else {
        return false;
    };
    if crate::git_staging::read_branch_oid(&common, name, &held).is_ok() {
        return true;
    }
    // Recognition only: the command still runs under its original fence.
    // Reuse a bounded nonblocking open; never stat an escaping operand.
    checkout
        && held.is_regular_file(&crate::caveats::lexically_normalize(
            &cwd.join(name).to_string_lossy(),
        ))
}

fn wrapper_refusal_with(
    policy: &AdoptedWorktree,
    source: &str,
    known: &impl Fn(&str, bool) -> bool,
) -> Option<String> {
    if super::standalone_git_words(source).is_some() {
        return None;
    }
    let (reason, retry) = match intent::classify(source, known)? {
        intent::Intent::Create(retry) => (
            "unsupported branch-creation command",
            format!("run `{retry}` as a standalone command"),
        ),
        intent::Intent::Ambiguous(Some(retry)) => (
            "ambiguous Git command",
            format!("run `{retry}` as a standalone command"),
        ),
        intent::Intent::Ambiguous(None) => (
            "ambiguous Git command",
            "run the same Git operation as a standalone command".to_owned(),
        ),
    };
    Some(format!(
        "capability denied: {reason} in an adopted worktree; {retry} in {} (set run_command cwd to that worktree). Run output filters and follow-up commands separately.",
        crate::worktree_adoption::task_path_literal(&policy.worktree)
    ))
}
