//! A narrow accident guard for explicit Git worktree administration and
//! unverified Git verbs during adoption, not a security/preservation guarantee.
//! Script files, runtime-generated commands, and ordinary task file deletion
//! (`rm -rf`, `find -delete`, interpreters) remain possible under existing grants.
use super::{invocation, literal, resembles_git};

pub(in crate::agentic::tools) const NOTICE: &str = "capability denied: adopted worktree administration accident guard refuses this destructive or unverified Git command (including aliases). Use an explicit supported Git built-in, or ask the operator for /permissions worktree-lift. This is not a preservation guarantee: script files, runtime-generated commands, and ordinary task file deletion (rm -rf, find -delete, interpreters) remain possible.";

/// Check direct/routed arguments. Lifecycle checks its retained source at the
/// execution boundary instead; resolving here would introduce a second read.
pub(in crate::agentic::tools) fn refuses(name: &str, args: &serde_json::Value) -> bool {
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
        _ => false,
    }
}

/// Recognize explicit commands and known descendants, never quoted prose.
/// This does not inspect script files or predict runtime-generated commands.
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
            // Git built-ins take precedence over aliases. Everything else is
            // unverified: repo/worktree/global config, -c, and external git-*
            // helpers can all hide worktree administration without spelling it.
            return !BUILTIN_VERBS.contains(&verb.as_str());
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
    // Uninspectable source is outside this narrow admission vocabulary.
    agent_bridle::inspect_shell(source).map_or(true, |inspection| contains(&inspection))
}

// Deliberately limited to established built-ins used in ordinary repository
// work. This is recognition, NOT a safety catalogue: existing mutation checks
// still apply, and built-ins can themselves run scripts/hooks.
const BUILTIN_VERBS: &[&str] = &[
    "add",
    "am",
    "apply",
    "archive",
    "blame",
    "branch",
    "bundle",
    "cat-file",
    "check-ignore",
    "check-ref-format",
    "checkout",
    "cherry",
    "cherry-pick",
    "clean",
    "clone",
    "commit",
    "config",
    "describe",
    "diff",
    "diff-files",
    "diff-index",
    "diff-tree",
    "fetch",
    "for-each-ref",
    "format-patch",
    "fsck",
    "gc",
    "grep",
    "hash-object",
    "help",
    "init",
    "log",
    "ls-files",
    "ls-remote",
    "ls-tree",
    "merge",
    "merge-base",
    "mv",
    "notes",
    "prune",
    "pull",
    "push",
    "range-diff",
    "rebase",
    "reflog",
    "remote",
    "repack",
    "reset",
    "restore",
    "rev-list",
    "rev-parse",
    "revert",
    "rm",
    "shortlog",
    "show",
    "show-ref",
    "stash",
    "status",
    "switch",
    "symbolic-ref",
    "tag",
    "update-index",
    "update-ref",
    "version",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adoption_administration_checks_routed_argv_too() {
        assert!(refuses(
            "run_command",
            &serde_json::json!({"command":"git worktree prune"})
        ));
        assert!(refuses(
            "build_exec",
            &serde_json::json!({"argv":["git", "worktree", "remove", "../task", "--force"]})
        ));
        assert!(!refuses(
            "build_exec",
            &serde_json::json!({"argv":["echo", "git worktree prune"]})
        ));
        assert!(!refuses(
            "lifecycle",
            &serde_json::json!({"phase":"test", "action":"list"})
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
            "git -c 'alias.wipe=worktree remove --force' wipe ../task",
            "git wipe ../task",
            "git custom-helper ../task",
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
            "git -c 'alias.status=worktree prune' status --short",
            "git worktree list --porcelain",
            "git worktree add -b other ../other",
            "git worktree lock ../task",
        ] {
            assert!(!destructive(source), "false positive {source}");
        }
    }
}

#[cfg(test)]
#[path = "administration_lifecycle_tests.rs"]
mod lifecycle_tests;
