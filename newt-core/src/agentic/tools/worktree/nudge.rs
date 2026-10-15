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
    // #2778: this is advisory syntax recognition, not evidence that a
    // conditional segment ran. Inspect every literal segment, never quoted
    // operands, shell substitutions or delegated child commands.
    if !inspection.constructs.is_empty()
        || !inspection.commands.iter().any(|command| {
            if !command.descendant_execs.is_empty()
                || !command.program.as_deref().is_some_and(resembles_git)
                || !command
                    .argv
                    .first()
                    .is_some_and(|word| command.source.trim_start().starts_with(word))
            {
                return false;
            }
            let Some(words) = command
                .argv
                .iter()
                .map(|word| literal(word))
                .collect::<Option<Vec<_>>>()
            else {
                return false;
            };
            let Ok((verb, args, same_repository)) = invocation(&words) else {
                return false;
            };
            same_repository
                && matches!(args, [flag, _] | [flag, _, _]
                if (verb == "checkout" && flag == "-b") || (verb == "switch" && flag == "-c"))
        })
    {
        return None;
    }
    normal_checkout(cwd, read)?;
    if session.branch_nudge_shown.swap(true, Relaxed) {
        return None;
    }
    Some(HINT)
}

/// Advisory-only discovery on every platform: a normal .git directory is
/// enough evidence for a reminder. Gitfiles (linked/submodule/separate-git-dir),
/// symlinks and unknown metadata receive none. No file contents or grants are
/// read here; confinement/adoption still uses its own held-handle verification.
fn normal_checkout(cwd: &Path, read: &crate::Scope<String>) -> Option<()> {
    let cwd = cwd.canonicalize().ok()?;
    for dir in cwd.ancestors() {
        let dot_git = dir.join(".git");
        if !crate::permits_path(read, &dot_git.to_string_lossy()) {
            return None;
        }
        match std::fs::symlink_metadata(&dot_git) {
            Ok(meta) => {
                if !meta.is_dir() {
                    return None;
                }
                let common = dot_git.join("commondir");
                if !crate::permits_path(read, &common.to_string_lossy()) {
                    return None;
                }
                return match std::fs::symlink_metadata(common) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(()),
                    _ => None,
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    None
}

/// #2766: record the existing destination and observed branch after a successful
/// standalone creation in ambient mode. This is advisory evidence only.
/// Never enter confinement verification here (#2763), or infer success from
/// an aggregate pipeline/compound result. Unknown commands retain prior state.
pub(super) fn record_unarmed_creation(
    session: &WorktreeSession,
    args: &serde_json::Value,
    workspace: &str,
    outcome: Option<&crate::ExecOutcome>,
) {
    if outcome != Some(&crate::ExecOutcome::Passed) {
        return;
    }
    let Some((path, branch)) = unarmed_task_location(args, workspace) else {
        return;
    };
    session.record_task_worktree(&path, &branch);
}

fn unarmed_task_location(args: &serde_json::Value, workspace: &str) -> Option<(PathBuf, String)> {
    let (cd, source) = split_leading_cd(args.get("command")?.as_str()?);
    let words = standalone_git_words(&source)?;
    let cwd = resolve_exec_cwd(workspace, args.get("cwd").and_then(|value| value.as_str()));
    let mut cwd = PathBuf::from(resolve_exec_cwd(&cwd, cd.as_deref()));
    let mut global = words.iter().skip(1);
    let mut verb = global.next()?;
    while verb == "-C" {
        cwd = cwd.join(global.next()?);
        verb = global.next()?;
    }
    if verb != "worktree" {
        return None;
    }
    let args = worktree_add_args(&words)?;
    let (branch, path) = match args {
        [flag, branch, path] | [flag, branch, path, _] if flag == "-b" || flag == "-B" => {
            (branch, path)
        }
        _ => return None,
    };
    crate::git_staging::validate_branch_name(branch).ok()?;
    if path.starts_with('-') || path.is_empty() {
        return None;
    }
    // Resolve through the filesystem: lexical `..` removal changes the meaning
    // when cwd or -C passes through a symlink.
    let path = cwd.join(path).canonicalize().ok()?;
    let gitlink = std::fs::read_to_string(path.join(".git")).ok()?;
    let admin = gitlink.strip_prefix("gitdir:")?.trim();
    if admin.is_empty() {
        return None;
    }
    let head = std::fs::read_to_string(path.join(admin).join("HEAD")).ok()?;
    let branch = head.trim().strip_prefix("ref: refs/heads/")?;
    crate::git_staging::validate_branch_name(branch).ok()?;
    Some((path, branch.to_owned()))
}

#[cfg(test)]
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
            "git worktree add ../task task 2>&1 || git checkout -b task 2>&1",
            "git status && git switch -c task",
            "false && git checkout -b task",
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
            "false || echo 'git checkout -b task'",
            "echo 'git switch -c task' && git status",
            "git -C elsewhere checkout -b task",
            "sh -c 'git checkout -b task'",
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

    /// #2778: advisory discovery is portable, scoped, and conservative about
    /// unknown or shared admin metadata, including from a child cwd.
    #[test]
    fn worktree_nudge_2778_metadata_scope_and_unknown_checkout() {
        let temp = fixture();
        let child = temp.path().join("src");
        std::fs::create_dir(&child).unwrap();
        let root = temp.path().canonicalize().unwrap();
        let scope = crate::Scope::only([root.to_string_lossy().into_owned()]);
        assert_eq!(normal_checkout(&child, &scope), Some(()));
        assert!(normal_checkout(&child, &crate::Scope::none()).is_none());
        std::fs::write(root.join(".git/commondir"), "../shared").unwrap();
        assert!(normal_checkout(&child, &scope).is_none());
        let unknown = tempfile::tempdir().unwrap();
        let scope = crate::Scope::only([unknown
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned()]);
        assert!(normal_checkout(unknown.path(), &scope).is_none());
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
    /// #2761/#2778: actual dispatch carries the reminder after a failed
    /// worktree add and compound branch fallback, only once across later commands.
    #[cfg(unix)]
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
                "cd '{}' && git worktree add task first 2>&1 || git checkout -b first 2>&1",
                temp.path().display()
            ),
            "git checkout -b second 2>&1 | tail -3 && git branch --show-current".into(),
            "git switch -c third".into(),
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

#[cfg(test)]
mod handoff_tests {
    use super::*;

    /// #2766: ambient advisory state requires a successful standalone literal
    /// creation, resolves the dispatched cwd, and never arms adoption.
    #[test]
    fn compaction_task_worktree_ambient_evidence_and_reset() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().to_str().unwrap();
        let session = WorktreeSession::default();
        let args = serde_json::json!({"command":"git worktree add -b task ../task", "cwd":"repo"});
        for outcome in [
            None,
            Some(&crate::ExecOutcome::Failed),
            Some(&crate::ExecOutcome::Denied),
        ] {
            record_unarmed_creation(&session, &args, workspace, outcome);
            assert!(session.task_hint().is_none());
        }
        // A Passed outcome without an existing linked checkout is insufficient.
        record_unarmed_creation(
            &session,
            &args,
            workspace,
            Some(&crate::ExecOutcome::Passed),
        );
        assert!(session.task_hint().is_none());
        std::fs::create_dir(temp.path().join("repo")).unwrap();
        let task = temp.path().join("task");
        let admin = temp.path().join("admin");
        std::fs::create_dir(&task).unwrap();
        std::fs::create_dir(&admin).unwrap();
        std::fs::write(task.join(".git"), "gitdir: ../admin\n").unwrap();
        // Use observed HEAD, not the requested branch operand.
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/observed-task\n").unwrap();
        for args in [
            args,
            serde_json::json!({"command":"cd repo && git worktree add -b task ../task"}),
            serde_json::json!({"command":"git -C repo worktree add -b task ../task"}),
        ] {
            record_unarmed_creation(
                &session,
                &args,
                workspace,
                Some(&crate::ExecOutcome::Passed),
            );
            let expected = format!("Task worktree: `{}` (branch observed-task) — run commands there, not in the original checkout.", dunce::simplified(&task.canonicalize().unwrap()).display());
            assert_eq!(session.task_hint().as_deref(), Some(expected.as_str()));
            assert!(session.snapshot().is_none());
            session.lift();
            assert!(session.task_hint().is_none());
        }
        for head in [
            "",
            "not a HEAD",
            "ref: refs/heads/bad..branch\n",
            "0123456789012345678901234567890123456789\n",
        ] {
            std::fs::write(admin.join("HEAD"), head).unwrap();
            record_unarmed_creation(
                &session,
                &serde_json::json!({"command":"git worktree add -b task task"}),
                workspace,
                Some(&crate::ExecOutcome::Passed),
            );
            assert!(session.task_hint().is_none(), "{head}");
        }
        for command in [
            "echo 'git worktree add -b fake ../fake'",
            "git worktree add -b fake ../fake && echo success",
            "git worktree add -b fake ../fake | tail -3",
            "git worktree add -b fake ../fake || true",
            "git --work-tree=elsewhere worktree add -b fake ../fake",
            "git worktree add -b fake --detach",
        ] {
            record_unarmed_creation(
                &session,
                &serde_json::json!({"command":command}),
                workspace,
                Some(&crate::ExecOutcome::Passed),
            );
            assert!(session.task_hint().is_none(), "{command}");
        }
    }
}
