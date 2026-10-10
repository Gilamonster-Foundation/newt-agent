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
    if let Some(reason) = wrapper_refusal(policy, source) {
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
    // Only standalone commands may reach the bounded branch broker. Even
    // presentation grammar accepted by other brokers is a wrapper here.
    if super::standalone_git_words(source).is_some() {
        return None;
    }
    let requested = requested_branch(source)?;
    let name = requested
        .as_deref()
        .map_or_else(|| "<name>".to_owned(), crate::mcp::shell_quote_arg);
    Some(format!(
        "capability denied: unsupported branch-creation command in an adopted worktree; run `git checkout -b {name}` as a standalone command in {} (set run_command cwd to that worktree). Run output filters and follow-up commands separately.",
        crate::worktree_adoption::task_path_literal(&policy.worktree)
    ))
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
        // Classify the verb, not its creation flags. Unknown global options
        // can only widen this refusal trigger, never the broker's write grammar.
        let (verb, args) = match super::invocation(&words) {
            Ok((verb, args, _)) => (verb, args),
            Err(_) => {
                let index = words
                    .iter()
                    .position(|word| matches!(word.as_str(), "checkout" | "switch" | "branch"))?;
                (words[index].as_str(), &words[index + 1..])
            }
        };
        if !matches!(verb, "checkout" | "switch" | "branch") {
            return None;
        }
        // Best-effort presentation only: failure to recover a name must NEVER
        // suppress refusal. Attached short flags are useful for the retry hint.
        let name = args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .find_map(|arg| {
                if !arg.starts_with('-') {
                    return super::literal(arg);
                }
                let flag = match verb {
                    "checkout" => "-b",
                    "switch" => "-c",
                    _ => return None,
                };
                arg.strip_prefix(flag)
                    .filter(|name| !name.is_empty())
                    .and_then(super::literal)
            });
        Some(name)
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
    match agent_bridle::inspect_shell(source) {
        Ok(inspection) => inspect(&inspection),
        Err(_) => {
            // Opaque dispatchers (including env) or siblings can prevent the
            // entire inventory. As with worktree creation admission, text may
            // force refusal here, but can never authorize a native write.
            static MARKER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
                regex::Regex::new(r"(?is)\bgit(?:\.exe)?\b.*\b(?:checkout|switch|branch)\b")
                    .expect("fixed branch wrapper marker")
            });
            let text: String = source
                .replace("\\\n", "")
                .chars()
                .filter(|c| !matches!(c, '\'' | '"' | '\\'))
                .collect();
            MARKER.is_match(&text).then_some(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wrapper detection is refusal-only and must distinguish actual Git argv
    /// from quoted prose. Bare query behavior is preserved by execute.
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
            "git switch --create task | tail -3",
            "git -C. checkout -b task | tail -3",
            "git -C./ checkout -b task | tail -3",
            "git branch --no-track task HEAD | tail -3",
            "LANG=C git checkout -b task; echo done",
        ] {
            assert_eq!(
                requested_branch(source),
                Some(Some("task".into())),
                "{source}"
            );
        }
        // Unknown creation flags and queries still trigger wrapper refusal;
        // detecting a name is not a prerequisite.
        for source in [
            "git switch --future-create-option | tail -3",
            "git branch --show-current | tail -3",
            "env LANG=C git checkout -b task; echo done",
        ] {
            assert!(requested_branch(source).is_some(), "{source}");
        }
        for source in [
            "echo 'git checkout -b task'",
            "printf '%s' 'git branch task'",
            "git log -- branch",
            "git -C branch status",
        ] {
            assert_eq!(requested_branch(source), None, "{source}");
        }
    }
}
