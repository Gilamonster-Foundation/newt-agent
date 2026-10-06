//! #2761: advisory worktree reminder on the existing result annotation path.
use super::*;
use crate::worktree_adoption::WorktreeSession;

const HINT: &str = "The task asks for a new git worktree. Instead of branching in place, run `git worktree add -b <branch> ../<name>` as a standalone command and work there.";

pub(super) fn branch_in_place(
    session: &WorktreeSession,
    objective: &str,
    command: &str,
    cwd: &Path,
    read: &crate::Scope<String>,
) -> Option<&'static str> {
    use std::sync::atomic::Ordering::Relaxed;
    if !objective.to_ascii_lowercase().contains("worktree")
        || session.branch_nudge_shown.load(Relaxed)
    {
        return None;
    }
    let inspection = agent_bridle::inspect_shell(command).ok()?;
    let first = inspection.commands.first()?;
    // Only the leading, literal command establishes this advisory. Trailing
    // output filters (including the reported tail pipeline) are harmless.
    if !inspection.constructs.is_empty()
        || !first.descendant_execs.is_empty()
        || !first.program.as_deref().is_some_and(resembles_git)
        || !command.trim_start().starts_with(first.source.trim_start())
        || !first.source.trim_start().starts_with(first.argv.first()?)
    {
        return None;
    }
    let words = first
        .argv
        .iter()
        .map(|word| literal(word))
        .collect::<Option<Vec<_>>>()?;
    let (verb, args, same_repository) = invocation(&words).ok()?;
    if !same_repository
        || !matches!(args, [flag, _] | [flag, _, _]
        if (verb == "checkout" && flag == "-b") || (verb == "switch" && flag == "-c"))
    {
        return None;
    }
    // Reuse bounded metadata reads; an unknown checkout silently gets no hint.
    let held = crate::git_staging::HeldRoots::bind(read).ok()?;
    let (common, admin) =
        crate::git_staging::discover_git_dirs(&cwd.canonicalize().ok()?, &held).ok()?;
    if common != admin || session.branch_nudge_shown.swap(true, Relaxed) {
        return None;
    }
    Some(HINT)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join(".git")).unwrap();
        temp
    }

    /// #2761: an objective requesting a worktree gets one reminder across calls
    /// and permission lifts, for both branch-creation verbs and the live wrapper.
    #[test]
    fn worktree_nudge_is_once_per_session() {
        for command in [
            "git checkout -b task",
            "git switch -c task",
            "git checkout -b task 2>&1 | tail -3 && git branch --show-current",
        ] {
            let temp = fixture();
            let session = WorktreeSession::default();
            assert_eq!(
                branch_in_place(
                    &session,
                    "Work in a new Git WORKTREE",
                    command,
                    temp.path(),
                    &crate::Scope::All
                ),
                Some(HINT)
            );
            session.lift();
            assert!(branch_in_place(
                &session,
                "Work in a worktree",
                command,
                temp.path(),
                &crate::Scope::All
            )
            .is_none());
        }
    }

    /// #2761: negative calls must not consume the reminder; echoed text, ordinary
    /// checkouts and unreadable/unknown repository metadata are not evidence.
    #[test]
    fn worktree_nudge_requires_objective_and_real_branch_command() {
        let temp = fixture();
        let session = WorktreeSession::default();
        assert!(branch_in_place(
            &session,
            "Refactor this code",
            "git checkout -b task",
            temp.path(),
            &crate::Scope::All
        )
        .is_none());
        for command in [
            "echo 'git checkout -b task'",
            "git checkout task",
            "git switch task",
            "git log --grep='checkout -b task'",
            "false && git checkout -b task",
        ] {
            assert!(
                branch_in_place(
                    &session,
                    "Use a worktree",
                    command,
                    temp.path(),
                    &crate::Scope::All
                )
                .is_none(),
                "{command}"
            );
        }
        assert!(branch_in_place(
            &session,
            "Use a worktree",
            "git switch -c task",
            temp.path(),
            &crate::Scope::none()
        )
        .is_none());
        assert_eq!(
            branch_in_place(
                &session,
                "Use a worktree",
                "git switch -c task",
                temp.path(),
                &crate::Scope::All
            ),
            Some(HINT)
        );
    }

    /// #2761: linked worktrees use a distinct Git admin directory and need no
    /// reminder, including from a child cwd. No subprocess or authority grant.
    #[test]
    fn worktree_nudge_skips_linked_worktrees() {
        let temp = fixture();
        let linked = temp.path().join("linked");
        let admin = temp.path().join(".git/worktrees/task");
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(linked.join("src")).unwrap();
        std::fs::write(linked.join(".git"), format!("gitdir: {}", admin.display())).unwrap();
        std::fs::write(admin.join("commondir"), "../..").unwrap();
        let session = WorktreeSession::default();
        assert!(branch_in_place(
            &session,
            "Use a worktree",
            "git checkout -b task",
            &linked.join("src"),
            &crate::Scope::All
        )
        .is_none());
        assert_eq!(
            branch_in_place(
                &session,
                "Use a worktree",
                "git checkout -b task",
                temp.path(),
                &crate::Scope::All
            ),
            Some(HINT)
        );
    }
    /// #2761: the actual dispatch result carries the reminder after the live
    /// leading-cd/output-filter shape, and only once across later commands.
    #[tokio::test]
    #[ignore = "real Git and host shell; explicit result annotation proof"]
    async fn worktree_nudge_dispatch_result() {
        use crate::agentic::tools::disable_ocap_tests::{env_lock, EnvVar};
        let _lock = env_lock().await;
        let _bypass = EnvVar::set("NEWT_DISABLE_OCAP", "1");
        let _full = EnvVar::set("NEWT_FULL_ACCESS", "1");
        let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "host");
        let _paths = EnvVar::set("NEWT_EXEC_PATHS", "/usr/bin:/bin");
        let _venv = EnvVar::unset("NEWT_VENV");
        let _virtual_env = EnvVar::unset("VIRTUAL_ENV");
        let temp = tempfile::tempdir().unwrap();
        let output =
            crate::agentic::tools::tests::git_shell_grant::hermetic_git(temp.path(), temp.path())
                .args(["init", "-q", "-b", "main"])
                .output()
                .unwrap();
        assert!(output.status.success());
        let session = WorktreeSession::default();
        let objective =
            crate::agentic::prompt_read::PromptReadContext::new(None, "Use a new worktree", None);
        struct Quiet;
        impl ToolPresentation for Quiet {
            fn preview(&mut self, _: &str, _: usize) {}
            fn document(&mut self, _: &str) {}
            fn override_result(&mut self, _: String) {}
        }
        for (index, command) in [
            format!(
                "cd '{}' && git checkout -b first 2>&1 | tail -3 && git branch --show-current",
                temp.path().display()
            ),
            "git switch -c second".into(),
        ]
        .iter()
        .enumerate()
        {
            let outcome = std::sync::OnceLock::new();
            let directory = std::sync::OnceLock::new();
            let result = execute(
                &mut Quiet,
                "run_command",
                &serde_json::json!({"command":command}),
                temp.path().to_str().unwrap(),
                false,
                20,
                &Caveats::top(),
                &mut crate::agentic::NoMcp,
                ToolCollaborators {
                    worktree_session: Some(&session),
                    prompt_context: Some(objective),
                    execution: Some(&outcome),
                    command_directory: Some(&directory),
                    ..Default::default()
                },
                false,
                PromptDisposition::Act,
            )
            .await;
            assert_eq!(outcome.get(), Some(&crate::ExecOutcome::Passed), "{result}");
            assert_eq!(result.contains(HINT), index == 0, "{result}");
        }
    }
}
