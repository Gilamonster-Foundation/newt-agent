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
    let requested = requested_branch(source)?;
    let refuse = || {
        let name = requested
            .as_deref()
            .map_or_else(|| "<name>".to_owned(), crate::mcp::shell_quote_arg);
        Some((
            format!(
                "capability denied: unsupported branch-creation command in an adopted worktree; run `git checkout -b {name}` as a standalone command in {} (set run_command cwd to that worktree). Run output filters and follow-up commands separately.",
                crate::worktree_adoption::task_path_literal(&policy.worktree)
            ),
            ExecOutcome::Denied,
        ))
    };
    let Ok(command) = GovernedCommand::parse(source) else {
        return refuse();
    };
    let words: Vec<_> = command.argv.iter().map(String::as_str).collect();
    let branch = match words.as_slice() {
        ["git", "checkout", "-b", branch]
        | ["git", "switch", "-c", branch]
        | ["git", "checkout", "-b", branch, "HEAD"]
        | ["git", "switch", "-c", branch, "HEAD"] => branch,
        _ => return refuse(),
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

/// A refusal trigger, never authority. Inspect command argv rather than prose;
/// the existing exact parser remains the only path to native branch writes.
fn requested_branch(source: &str) -> Option<Option<String>> {
    fn words(program: Option<&str>, argv: &[String]) -> Option<Option<String>> {
        if !program.is_some_and(super::resembles_git) {
            return None;
        }
        let words: Vec<_> = argv
            .iter()
            .map(|w| super::literal(w).unwrap_or_else(|| w.clone()))
            .collect();
        let (verb, args, _) = super::invocation(&words).ok()?;
        if verb == "branch" {
            let first = args.first()?;
            return (!args.iter().any(|arg| arg.starts_with('-'))).then(|| super::literal(first));
        }
        let flag = match verb {
            "checkout" => "-b",
            "switch" => "-c",
            _ => return None,
        };
        for (index, arg) in args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .enumerate()
        {
            if arg == flag {
                return Some(args.get(index + 1).and_then(|name| super::literal(name)));
            }
            if let Some(name) = arg.strip_prefix(flag).filter(|name| !name.is_empty()) {
                return Some(super::literal(name));
            }
        }
        None
    }
    fn inspect(inspection: &agent_bridle::ShellInspection) -> Option<Option<String>> {
        inspection
            .commands
            .iter()
            .find_map(|command| {
                words(command.program.as_deref(), &command.argv).or_else(|| {
                    command
                        .descendant_execs
                        .iter()
                        .find_map(|child| words(Some(&child.program), &child.argv))
                })
            })
            .or_else(|| {
                inspection
                    .constructs
                    .iter()
                    .find_map(|construct| construct.inspection.as_deref().and_then(inspect))
            })
    }
    inspect(&agent_bridle::inspect_shell(source).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrapper detection is refusal-only and must distinguish actual Git argv
    /// from quoted prose, branch queries, and checkout path operands.
    #[test]
    fn branch_wrapper_detection_uses_commands_not_mentions() {
        for source in [
            "git checkout -b task 2>&1 | tail -3",
            "git switch -c task && git branch --show-current",
            "git branch task; echo done",
            "git branch task > output",
            "git -C sub checkout -b task && echo done",
            "'/usr/bin/git' checkout '-b' task | tail -3",
            "git checkout -btask | tail -3",
        ] {
            assert_eq!(
                requested_branch(source),
                Some(Some("task".into())),
                "{source}"
            );
        }
        for source in [
            "echo 'git checkout -b task'",
            "printf '%s' 'git branch task'",
            "git branch",
            "git branch --show-current",
            "git branch --list task",
            "git branch task --list",
            "git checkout -- -b",
            "git switch task",
        ] {
            assert_eq!(requested_branch(source), None, "{source}");
        }
    }
}
