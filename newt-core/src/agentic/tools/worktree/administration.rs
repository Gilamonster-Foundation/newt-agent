//! Adoption binds a live checkout and its administrative directory. Neither is
//! disposable until the operator lifts adoption. Kernel write grants needed for
//! ordinary edits/index updates cannot by themselves preserve this lifetime.
use super::{invocation, literal, resembles_git};

pub(in crate::agentic::tools) const NOTICE: &str = "capability denied: adopted worktree administration cannot remove, prune, move, repair, or unlock worktrees. Uncommitted work and the session binding must be preserved. Only the operator can lift adoption with /permissions worktree-lift.";

/// The shared dispatcher calls this after aliases/routing, before tool execution or
/// permission approval. Lifecycle uses the same resolved phase as execution.
pub(in crate::agentic::tools) fn refuses(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
) -> bool {
    match name {
        "run_command" => args
            .get("command")
            .and_then(|v| v.as_str())
            .is_some_and(destructive),
        "build_exec" => {
            let argv: Vec<_> = args
                .get("argv")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str())
                .map(|word| format!("'{}'", word.replace('\'', "'\\''")))
                .collect();
            destructive(&argv.join(" "))
        }
        "lifecycle" if args.get("action").and_then(|v| v.as_str()) != Some("list") => {
            let Some(phase) = args
                .get("phase")
                .and_then(|v| v.as_str())
                .and_then(crate::tooling::Phase::from_key)
            else {
                return false;
            };
            let dir =
                super::super::resolve_exec_cwd(workspace, args.get("dir").and_then(|v| v.as_str()));
            crate::tooling::resolved_phase_commands(std::path::Path::new(&dir), phase)
                .iter()
                .any(|command| destructive(command))
        }
        _ => false,
    }
}

/// Refusal only: no path spelling, cwd selector, or permission approval may
/// authorize dismantling an adopted checkout. Other sessions retain Git's
/// existing behavior. Inspect command nodes, never prose in an echo operand.
pub(in crate::agentic::tools) fn destructive(source: &str) -> bool {
    fn words(program: Option<&str>, argv: &[String]) -> bool {
        if !program.is_some_and(resembles_git) {
            return false;
        }
        let words: Vec<_> = argv
            .iter()
            .map(|word| literal(word).unwrap_or_else(|| word.clone()))
            .collect();
        let Ok((verb, args, _)) = invocation(&words) else {
            // Unknown Git option syntax cannot prove a safe administration call.
            return true;
        };
        let Some(verb) = literal(verb) else {
            return true;
        };
        if verb != "worktree" {
            return false;
        }
        // Only the non-destructive forms are admitted. Creation still passes
        // through the separate creation/adoption guard, never this predicate.
        !matches!(
            args.first().map(String::as_str),
            Some("list" | "add" | "lock")
        )
    }
    fn contains(inspection: &agent_bridle::ShellInspection) -> bool {
        inspection.commands.iter().any(|command| {
            words(command.program.as_deref(), &command.argv)
                || command
                    .descendant_execs
                    .iter()
                    .any(|child| words(Some(&child.program), &child.argv))
        }) || inspection
            .constructs
            .iter()
            .any(|construct| construct.inspection.as_deref().is_some_and(contains))
    }
    // An uninspectable command cannot prove that it preserves the session.
    agent_bridle::inspect_shell(source).map_or(true, |inspection| contains(&inspection))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adoption_administration_checks_routed_argv_too() {
        assert!(refuses(
            "run_command",
            &serde_json::json!({"command":"git worktree prune"}),
            "."
        ));
        assert!(refuses(
            "build_exec",
            &serde_json::json!({"argv":["git", "worktree", "remove", "../task", "--force"]}),
            "."
        ));
        assert!(!refuses(
            "build_exec",
            &serde_json::json!({"argv":["echo", "git worktree prune"]}),
            "."
        ));
        assert!(!refuses(
            "lifecycle",
            &serde_json::json!({"phase":"test", "action":"list"}),
            "."
        ));
    }

    /// Regression: worktree removal previously fell through native Git checks.
    #[test]
    fn adopted_administration_recognizes_commands_not_quoted_prose() {
        for source in [
            "git worktree remove ../task --force",
            "git worktree remove --force ../main/.git/worktrees/task",
            "git -C ../main worktree remove -ff ../task",
            "'/usr/bin/git' 'worktree' 'prune' --expire now",
            "git --git-dir=../main/.git worktree repair",
            "git worktree move ../task ../elsewhere",
            "git worktree unlock ../task",
            "git worktree $ACTION ../task",
            "git $VERB remove ../task",
            "echo before; git worktree prune",
            "echo $(git worktree prune)",
            "sh -c 'git worktree prune'",
            "env git worktree remove ../task --force",
            "timeout 5 git worktree remove ../task --force",
        ] {
            assert!(destructive(source), "missed {source}");
        }
        for source in [
            "echo 'git worktree remove ../task --force'",
            "printf '%s' 'git worktree prune'",
            "git diff -- 'worktree remove'",
            "git status --short",
            "git worktree list --porcelain",
            "git worktree add -b other ../other",
            "git worktree lock ../task",
        ] {
            assert!(!destructive(source), "false positive {source}");
        }
    }
}
