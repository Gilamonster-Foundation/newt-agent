//! Transitional native Git checks, not a replacement for a repository broker.
//! Keep the caller's shell source unchanged and preserve the embedded adapter's
//! destructive-operation confirmations while the command interface migrates.

use super::PermissionGate;
use crate::caveats::Caveats;
#[cfg(target_os = "windows")]
use crate::caveats::CaveatsExt;
use crate::git_caveats::GitCaveats;
use agent_bridle::{inspect_shell, ShellInspection};
use std::path::Path;

pub(super) fn preflight(
    source: &str,
    cwd: &Path,
    caveats: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
    native_commit_broker: bool,
) -> Result<(), String> {
    let inspection = match inspect_shell(source) {
        Ok(inspection) => inspection,
        // Interpreter/dispatcher execution remains the confined executor's
        // responsibility. Text mentioning Git is not proof of execution.
        // Descendant/dynamic ref semantics still require a repository broker;
        // this direct-command safeguard must not pretend to supply one.
        Err(_) => return Ok(()),
    };
    let caps = GitCaveats::from_session(caveats);
    inspect_commands(
        &inspection,
        true,
        cwd,
        caveats,
        &caps,
        gate,
        native_commit_broker,
    )
}

pub(super) fn needs_commit_broker(source: &str) -> bool {
    fn contains(inspection: &ShellInspection) -> bool {
        inspection.commands.iter().any(|command| {
            command.program.as_deref().is_some_and(is_git)
                && invocation(&command.argv)
                    .ok()
                    .and_then(|(verb, _, _)| literal(verb))
                    .as_deref()
                    == Some("commit")
        }) || inspection
            .constructs
            .iter()
            .any(|construct| construct.inspection.as_deref().is_some_and(contains))
    }
    inspect_shell(source).is_ok_and(|inspection| contains(&inspection))
}

fn unresolved(detail: &str) -> String {
    format!("refused: {detail}; native Git ref mutation is not yet supported by the repository authority adapter")
}

fn is_git(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    name.eq_ignore_ascii_case("git") || name.eq_ignore_ascii_case("git.exe")
}

/// Git for Windows resolves its startup current directory through every
/// profile ancestor. AppContainer deliberately cannot read those ancestors
/// merely because the repository itself is admitted, so executing native Git
/// would otherwise produce an opaque child error after the authority decision.
///
/// Keep this a named, pre-spawn refusal instead of widening the filesystem
/// fence or falling back to the host. An operator may still make the explicit
/// `--disable-ocap` / `--full-access` choice, which selects a non-AppContainer
/// route before this check runs.
#[cfg(any(target_os = "windows", test))]
pub(super) const WINDOWS_APPCONTAINER_GIT_UNAVAILABLE: &str =
    "native Git is unavailable under Windows AppContainer confinement: Git for Windows requires current-directory ancestry traversal outside the granted roots; no command ran";

/// A single, literal direct native-Git command. Compound and dynamic forms
/// retain their normal shell semantics: refusing a whole compound before its
/// earlier stages run would be a separate behavior change.
#[cfg(any(target_os = "windows", test))]
fn literal_native_git_program(source: &str) -> Option<String> {
    let inspection = inspect_shell(source).ok()?;
    let command = inspection.commands.first()?;
    if inspection.commands.len() == 1
        && inspection.constructs.is_empty()
        && command.redirects.is_empty()
        && command.descendant_execs.is_empty()
    {
        command
            .program
            .as_deref()
            .filter(|program| is_git(program))
            .map(str::to_owned)
    } else {
        None
    }
}

#[cfg(any(target_os = "windows", test))]
fn appcontainer_native_git_refusal_for(
    program: Option<&str>,
    effective_sandbox: agent_bridle::SandboxKind,
    exec_allowed: bool,
) -> Option<&'static str> {
    (program.is_some_and(is_git)
        && exec_allowed
        && effective_sandbox == agent_bridle::SandboxKind::AppContainer)
        .then_some(WINDOWS_APPCONTAINER_GIT_UNAVAILABLE)
}

/// Mirror Bridle's executable authority check for a direct program word. A
/// bare `git.exe` grant also authorizes a PATH-resolved `...\\git.exe`, so the
/// pre-spawn refusal must recognize that exact effective authority rather than
/// letting the eventual interceptor reach AppContainer first.
#[cfg(target_os = "windows")]
fn exec_scope_allows_program(caveats: &Caveats, program: &str) -> bool {
    caveats.permits_exec(program)
        || Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| caveats.permits_exec(name))
}

/// A Windows-only pre-spawn refusal for the one native Git shape known to be
/// incompatible with the restricted AppContainer backend. Its backend choice
/// is calculated from the same policy as the eventual shell dispatch; an
/// unrestricted grant therefore stays on its existing non-AppContainer path.
pub(super) fn windows_appcontainer_native_git_refusal(
    source: &str,
    caveats: &Caveats,
) -> Option<&'static str> {
    #[cfg(target_os = "windows")]
    {
        let program = literal_native_git_program(source);
        let exec_allowed = program
            .as_deref()
            .is_some_and(|program| exec_scope_allows_program(caveats, program));
        let policy = std::sync::Arc::new(crate::confined_exec::runtime_sandbox_policy());
        let effective_sandbox = agent_bridle::effective_sandbox_kind(
            agent_bridle::best_available_sandbox(&policy).kind(),
            caveats,
        );
        appcontainer_native_git_refusal_for(program.as_deref(), effective_sandbox, exec_allowed)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (source, caveats);
        None
    }
}

/// Reuse Bridle's static executable-word resolution for a single literal
/// argument. No shell is run; expansions, multiple words, and redirects fail.
fn literal(word: &str) -> Option<String> {
    let parsed = inspect_shell(word).ok()?;
    let command = parsed.commands.first()?;
    (parsed.commands.len() == 1
        && parsed.constructs.is_empty()
        && command.argv.len() == 1
        && command.redirects.is_empty()
        && command.descendant_execs.is_empty())
    .then(|| command.program.clone())
    .flatten()
}

/// Find the native verb without translating argv. Repository/config selectors
/// remain usable for reads, but mutation cannot rely on the caller's cwd then.
fn invocation(argv: &[String]) -> Result<(&str, &[String], bool), String> {
    let mut index = 1;
    let mut same_repository = true;
    while let Some(word) = argv.get(index) {
        match word.as_str() {
            "--no-pager" | "--paginate" | "--no-optional-locks" => index += 1,
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env" => {
                same_repository = false;
                index += 2;
            }
            option
                if option.starts_with("--git-dir=")
                    || option.starts_with("--work-tree=")
                    || option.starts_with("--namespace=")
                    || option.starts_with("--config-env=")
                    || option.starts_with("--exec-path=")
                    || option == "--bare" =>
            {
                same_repository = false;
                index += 1;
            }
            "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs"
            | "--no-replace-objects" => index += 1,
            "--version" | "--help" | "--html-path" | "--man-path" | "--info-path" => {
                return Ok(("help", &[], true));
            }
            verb if !verb.starts_with('-') => {
                return Ok((verb, &argv[index + 1..], same_repository));
            }
            _ => return Err(unresolved("unresolved Git global options")),
        }
    }
    Ok(("help", &[], true))
}

fn inspect_commands(
    inspection: &ShellInspection,
    top_level: bool,
    cwd: &Path,
    caveats: &Caveats,
    caps: &GitCaveats,
    gate: &mut Option<&mut dyn PermissionGate>,
    native_commit_broker: bool,
) -> Result<(), String> {
    for command in &inspection.commands {
        for descendant in &command.descendant_execs {
            if is_git(&descendant.program) {
                let mut delegated = command.clone();
                delegated.source = descendant.source.clone();
                delegated.program = Some(descendant.program.clone());
                delegated.argv = descendant.argv.clone();
                delegated.redirects.clear();
                delegated.descendant_execs.clear();
                let nested = ShellInspection {
                    schema_version: inspection.schema_version,
                    source: descendant.source.clone(),
                    commands: vec![delegated],
                    constructs: vec![],
                    warnings: vec![],
                };
                inspect_commands(
                    &nested, false, cwd, caveats, caps, gate,
                    // Native dispatchers spawn outside Brush's final command
                    // filter; another direct command's broker cannot cover them.
                    false,
                )?;
            }
        }
        if !command.program.as_deref().is_some_and(is_git) {
            continue;
        }
        let Ok((raw_verb, args, same_repository)) = invocation(&command.argv) else {
            // Unknown CLI forms retain Git's own interpretation and the
            // existing confined executor's authority checks.
            continue;
        };
        let Some(verb) = literal(raw_verb) else {
            continue;
        };
        match verb.as_str() {
            "commit" if native_commit_broker => continue,
            // Complement the preexisting lexical attribution guard for
            // quoted executable/verb spellings resolved by Bridle.
            "commit" | "merge" | "rebase" | "cherry-pick" | "revert" => {
                let aborting = verb != "commit"
                    && args
                        .iter()
                        .filter_map(|arg| literal(arg))
                        .any(|arg| matches!(arg.as_str(), "--abort" | "--quit"));
                if !aborting {
                    return Err("refused: native commit creation requires harness-managed attribution and signing integration".into());
                }
                continue;
            }
            "checkout" | "switch" => {
                // -b/-c consumes its attached branch name; characters in
                // `-bfeature` / `-cfeature` are not bundled force options.
                let create_option = if verb == "checkout" { "-b" } else { "-c" };
                let words: Vec<String> = args
                    .iter()
                    .filter_map(|arg| literal(arg))
                    .filter(|word| !word.starts_with(create_option))
                    .collect();
                if has_option(
                    &words,
                    "BCf",
                    &["--force", "--force-create", "--discard-changes"],
                ) {
                    return Err(unresolved("forced branch replacement or checkout"));
                }
                continue;
            }
            "update-ref" => return Err(unresolved("direct ref update")),
            "symbolic-ref" => {
                let words = literal_arguments(args)?;
                let query_flags = ["-q", "--quiet", "--short", "--recurse", "--no-recurse"];
                let operands: Vec<_> = words
                    .iter()
                    .filter(|word| !query_flags.contains(&word.as_str()))
                    .collect();
                if operands.len() != 1 || operands[0].starts_with('-') {
                    return Err(unresolved("symbolic ref mutation"));
                }
                continue;
            }
            "reflog" => {
                if let Some(first) = args.first() {
                    let action =
                        literal(first).ok_or_else(|| unresolved("dynamic reflog operation"))?;
                    if matches!(action.as_str(), "expire" | "delete" | "drop" | "write") {
                        return Err(unresolved("reflog mutation"));
                    }
                }
                continue;
            }
            "stash" => {
                let action = args
                    .first()
                    .map(|word| literal(word).ok_or_else(|| unresolved("dynamic stash operation")))
                    .transpose()?;
                if !matches!(action.as_deref(), Some("drop" | "clear")) {
                    continue;
                }
            }
            "branch" => {}
            // The existing attribution guard runs before this preflight.
            // This is a small destructive-operation guard, not a replacement
            // command catalog: all other Git verbs execute normally.
            _ => continue,
        }
        let words = literal_arguments(args)?;
        if verb == "branch" {
            if has_option(&words, "fmMcC", &["--force", "--move", "--copy"]) {
                return Err(unresolved("branch overwrite, rename, or copy"));
            }
            if !has_option(&words, "dD", &["--delete"]) {
                continue;
            }
        }
        // The flattened inventory deliberately does not promise shell state
        // or control-flow edges. Only a single literal invocation may use the
        // cwd-bound destructive check; never guess after cd/env/substitution.
        let standalone = top_level
            && inspection.commands.len() == 1
            && inspection.constructs.is_empty()
            && command.redirects.is_empty()
            && command.descendant_execs.is_empty()
            && command.source.trim_start().starts_with(&command.argv[0]);
        if !standalone || !same_repository {
            return Err(unresolved("composed or redirected Git mutation"));
        }
        let op = if verb == "branch" {
            let branches = deleted_branches(&words)?;
            for branch in branches {
                if !caps.permits_ref(&format!("refs/heads/{branch}")) {
                    return Err("refused: native branch deletion requires git-ref authority".into());
                }
                refuse_protected_branch(cwd, branch, caveats)?;
            }
            "branch-delete"
        } else {
            if !caps.permits_stage() {
                return Err("refused: native stash deletion requires git-write authority".into());
            }
            "stash-drop"
        };
        if !gate
            .as_deref_mut()
            .is_some_and(|gate| super::git_data_loss_confirmed(gate, op))
        {
            return Err(format!(
                "refused: git {op} requires explicit destructive-operation confirmation"
            ));
        }
    }
    for construct in &inspection.constructs {
        if let Some(nested) = &construct.inspection {
            inspect_commands(
                nested,
                false,
                cwd,
                caveats,
                caps,
                gate,
                native_commit_broker,
            )?;
        }
    }
    Ok(())
}

fn literal_arguments(args: &[String]) -> Result<Vec<String>, String> {
    args.iter()
        .map(|arg| literal(arg).ok_or_else(|| unresolved("dynamic Git ref or stash arguments")))
        .collect()
}

fn has_option(words: &[String], short: &str, long: &[&str]) -> bool {
    words
        .iter()
        .take_while(|word| word.as_str() != "--")
        .any(|word| {
            long.contains(&word.as_str())
                || (word.starts_with('-')
                    && !word.starts_with("--")
                    && word[1..].chars().any(|flag| short.contains(flag)))
        })
}

fn deleted_branches(words: &[String]) -> Result<Vec<&str>, String> {
    let mut deleting = false;
    let mut positional = false;
    let mut branches = vec![];
    for word in words {
        match word.as_str() {
            "--" if !positional => positional = true,
            "-d" | "-D" | "--delete" if !positional => deleting = true,
            "-q" | "--quiet" if !positional => {}
            option if !positional && option.starts_with('-') => {
                return Err(unresolved("unsupported branch mutation options"))
            }
            branch => branches.push(branch),
        }
    }
    if !deleting || branches.is_empty() {
        return Err(unresolved("unclassified branch mutation"));
    }
    Ok(branches)
}

fn refuse_protected_branch(cwd: &Path, branch: &str, caveats: &Caveats) -> Result<(), String> {
    if matches!(branch, "main" | "master") {
        return Err(format!(
            "refused: cannot delete protected default branch '{branch}'"
        ));
    }
    // metadata_git checks read authority before constructing or launching any
    // process. Do not turn a bounded read grant into ambient repository reads.
    let output = crate::git_hardening::metadata_git(
        cwd,
        &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"],
        &caveats.fs_read,
    )
    .and_then(|mut command| command.output())
    .map_err(|_| {
        unresolved("cannot establish the protected default branch under current read authority")
    })?;
    if output.status.success() {
        if String::from_utf8_lossy(&output.stdout)
            .trim()
            .strip_prefix("refs/remotes/origin/")
            == Some(branch)
        {
            return Err(format!(
                "refused: cannot delete protected default branch '{branch}'"
            ));
        }
    } else if output.status.code() != Some(1) {
        return Err(unresolved("cannot establish the repository default branch"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{
        disable_ocap_tests::{env_lock, EnvVar},
        execute_tool_with_collaborators, PermissionDecision, PermissionRequest, ToolCollaborators,
    };
    use super::*;
    use crate::agentic::{NoMcp, PromptDisposition};

    #[test]
    fn python_text_mentioning_git_keeps_the_existing_execution_path() {
        for source in [
            "python -c 'print(\"git branch -D example\")'",
            "git status && python -m pytest",
        ] {
            assert!(
                preflight(source, Path::new("."), &Caveats::top(), &mut None, false).is_ok(),
                "{source}"
            );
        }
    }

    #[test]
    fn native_broker_selection_preserves_compound_and_quoted_commit_forms() {
        for source in [
            "git add .gitignore && git commit -m 'Ignore bytecode'",
            "'git' 'commit' -m message",
            "git -C directory -c user.email=author@example.invalid commit -F message.txt",
            "printf 'message' | git commit -F -",
        ] {
            assert!(needs_commit_broker(source), "{source}");
            assert!(
                preflight(source, Path::new("."), &Caveats::top(), &mut None, true).is_ok(),
                "{source}"
            );
        }
        assert!(!needs_commit_broker("python -c 'print(\"git commit\")'"));
        assert!(preflight(
            "git rebase HEAD~1",
            Path::new("."),
            &Caveats::top(),
            &mut None,
            true
        )
        .is_err());
    }

    /// Windows AppContainer cannot run Git for Windows from a profile-backed
    /// workspace: Git's startup cwd resolution needs ancestor access outside
    /// the admitted filesystem roots.  The route must name that limitation
    /// before spawning, while ordinary text that merely mentions Git remains
    /// eligible for the normal shell path.
    #[test]
    fn appcontainer_refuses_recognized_native_git_without_matching_text() {
        assert_eq!(
            literal_native_git_program("git status").as_deref(),
            Some("git")
        );
        assert_eq!(literal_native_git_program("echo git status"), None);
        assert_eq!(literal_native_git_program("git status && echo done"), None);
        assert_eq!(
            appcontainer_native_git_refusal_for(
                Some("git"),
                agent_bridle::SandboxKind::AppContainer,
                true,
            ),
            Some(WINDOWS_APPCONTAINER_GIT_UNAVAILABLE),
        );
        assert_eq!(
            appcontainer_native_git_refusal_for(
                Some("git"),
                agent_bridle::SandboxKind::AppContainer,
                false,
            ),
            None,
        );
        assert_eq!(
            appcontainer_native_git_refusal_for(
                Some("git"),
                agent_bridle::SandboxKind::Landlock,
                true,
            ),
            None,
        );
        assert_eq!(
            appcontainer_native_git_refusal_for(
                Some("git.exe"),
                agent_bridle::SandboxKind::None,
                true,
            ),
            None,
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn appcontainer_git_refusal_mirrors_bare_name_exec_grants() {
        let caveats = Caveats {
            exec: crate::Scope::only(["git.exe".to_owned()]),
            ..Caveats::top()
        };
        assert!(exec_scope_allows_program(
            &caveats,
            r"C:\Program Files\Git\cmd\git.exe"
        ));
        assert!(!exec_scope_allows_program(
            &caveats,
            r"C:\Program Files\Git\cmd\not-git.exe"
        ));
    }

    struct Gate {
        allow: bool,
        requests: Vec<PermissionRequest>,
    }

    #[test]
    fn native_descendants_cannot_borrow_a_direct_commits_broker() {
        for source in [
            "git commit -m direct; find . -exec git commit -m delegated \\;",
            "git commit -m direct; timeout 30 git commit -m delegated",
        ] {
            assert!(
                preflight(source, Path::new("."), &Caveats::top(), &mut None, true).is_err(),
                "a native descendant has no Brush spawn registration: {source}"
            );
        }
        assert!(
            preflight(
                "echo \"$(git commit -m direct)\"",
                Path::new("."),
                &Caveats::top(),
                &mut None,
                true,
            )
            .is_ok(),
            "actual nested Brush commands keep broker coverage"
        );
    }

    impl PermissionGate for Gate {
        fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
            self.requests.extend_from_slice(requests);
            if self.allow {
                PermissionDecision::Allow(Caveats::top())
            } else {
                PermissionDecision::Deny
            }
        }
        fn ask_question(&mut self, _question: &str) -> crate::agentic::HumanQuestionOutcome {
            crate::agentic::HumanQuestionOutcome::Unavailable
        }
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let mut cmd = crate::git_hardening::hardened_git(cwd, args).unwrap();
        // Windows CI: temp fixture owned by a different principal than the git process.
        // Add safe.directory only for this test fixture's command, not in production.
        #[cfg(windows)]
        if let Ok(canonical) = cwd.canonicalize() {
            if let Some(dir) = canonical.to_str() {
                cmd.env("GIT_CONFIG_COUNT", "1")
                    .env("GIT_CONFIG_KEY_0", "safe.directory")
                    .env("GIT_CONFIG_VALUE_0", dir);
            }
        }
        let output = cmd.output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn repository() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(repo.path().join("tracked.txt"), "original\n").unwrap();
        git(repo.path(), &["add", "tracked.txt"]);
        git(
            repo.path(),
            &[
                "-c",
                "user.name=Native fixture",
                "-c",
                "user.email=native@example.invalid",
                "commit",
                "--no-gpg-sign",
                "-qm",
                "fixture",
            ],
        );
        for branch in ["master", "stable", "victim", "task"] {
            git(repo.path(), &["branch", branch]);
        }
        git(
            repo.path(),
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/stable",
            ],
        );
        git(repo.path(), &["checkout", "-q", "task"]);
        std::fs::write(repo.path().join("tracked.txt"), "saved work\n").unwrap();
        git(
            repo.path(),
            &[
                "-c",
                "user.name=Native fixture",
                "-c",
                "user.email=native@example.invalid",
                "stash",
                "push",
                "-qm",
                "saved work",
            ],
        );
        repo
    }

    async fn run(source: &str, repo: &Path, gate: Option<&mut dyn PermissionGate>) -> String {
        execute_tool_with_collaborators(
            "run_command",
            &serde_json::json!({"command": source}),
            &repo.to_string_lossy(),
            false,
            40,
            &Caveats::top(),
            &mut NoMcp,
            ToolCollaborators {
                permission_gate: gate,
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap()
    }

    /// Grounds permission refresh in Git's real index: a workspace grant
    /// approved during the turn must reach the confined child, while a later
    /// mutation without that grant must leave the index unchanged.
    #[tokio::test]
    async fn native_staging_uses_current_workspace_grants() {
        let _lock = env_lock().await;
        let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
        let _full = EnvVar::unset("NEWT_FULL_ACCESS");
        let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
        let repo = repository();
        let workspace = repo.path().canonicalize().unwrap();
        let workspace = workspace.to_string_lossy().into_owned();
        std::fs::write(repo.path().join("pending.txt"), "pending work\n").unwrap();
        let baseline = Caveats {
            fs_write: crate::Scope::none(),
            ..Caveats::top()
        };

        struct RecalledWorkspaceGrant {
            workspace: Option<String>,
            refreshes: usize,
            requests: Vec<PermissionRequest>,
        }
        impl PermissionGate for RecalledWorkspaceGrant {
            fn refresh_caveats(&mut self, baseline: &Caveats) -> PermissionDecision {
                self.refreshes += 1;
                let mut current = baseline.clone();
                if let Some(workspace) = &self.workspace {
                    current.fs_write = crate::Scope::only([workspace.clone()]);
                }
                PermissionDecision::Allow(current)
            }
            fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
                self.requests.extend_from_slice(requests);
                PermissionDecision::Deny
            }
            fn ask_question(&mut self, _: &str) -> crate::agentic::HumanQuestionOutcome {
                crate::agentic::HumanQuestionOutcome::Unavailable
            }
        }

        for allowed in [true, false] {
            let mut gate = RecalledWorkspaceGrant {
                workspace: allowed.then(|| workspace.clone()),
                refreshes: 0,
                requests: vec![],
            };
            let command = if allowed {
                "git add -- pending.txt"
            } else {
                "git rm --cached -- pending.txt"
            };
            let out = execute_tool_with_collaborators(
                "run_command",
                &serde_json::json!({
                    "command": command,
                    "fs_write": [workspace],
                }),
                &workspace,
                false,
                40,
                &baseline,
                &mut NoMcp,
                ToolCollaborators {
                    permission_gate: Some(&mut gate),
                    ..Default::default()
                },
                false,
                PromptDisposition::Act,
                None,
            )
            .await
            .unwrap()
            .unwrap();
            let staged = git(repo.path(), &["diff", "--cached", "--name-only"]);
            #[cfg(target_os = "windows")]
            {
                if allowed {
                    assert!(
                        out.contains(WINDOWS_APPCONTAINER_GIT_UNAVAILABLE),
                        "the recalled filesystem grant must reach the pre-spawn Windows guard: {out}"
                    );
                } else {
                    assert!(out.contains("capability denied"), "{out}");
                    assert!(
                        !out.contains(WINDOWS_APPCONTAINER_GIT_UNAVAILABLE),
                        "a denied filesystem declaration must win before the Windows guard: {out}"
                    );
                }
                assert_eq!(staged.trim(), "", "{out}");
            }
            #[cfg(not(target_os = "windows"))]
            assert_eq!(staged.trim(), "pending.txt", "{out}");
            assert_eq!(gate.refreshes, 1, "allowed={allowed}: {out}");
            if allowed {
                assert!(gate.requests.is_empty(), "recalled grant must be reused");
            } else {
                assert!(out.contains("capability denied"), "{out}");
                assert_eq!(gate.requests.len(), 1);
                assert_eq!(gate.requests[0].kind, super::super::DenialKind::FsWrite);
                assert_eq!(gate.requests[0].target, workspace);
            }
        }
    }

    #[tokio::test]
    async fn denied_native_deletion_preserves_real_branches_and_stash() {
        let _lock = env_lock().await;
        let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
        let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
        let repo = repository();
        let before = git(repo.path(), &["show-ref"]);
        let stash = git(repo.path(), &["stash", "list"]);
        for source in [
            "git branch -D victim",
            "git branch -d victim",
            "git stash drop 'stash@{0}'",
            "git stash clear",
        ] {
            let mut gate = Gate {
                allow: false,
                requests: vec![],
            };
            let out = run(source, repo.path(), Some(&mut gate)).await;
            assert!(out.starts_with("refused:"), "{source}: {out}");
            assert_eq!(gate.requests.len(), 1, "{source}: {out}");
            assert!(gate.requests[0].reason.contains("DESTROYS"));
            assert_eq!(git(repo.path(), &["show-ref"]), before, "{source}");
            assert_eq!(git(repo.path(), &["stash", "list"]), stash, "{source}");
        }
        let out = run("git branch -D victim", repo.path(), None).await;
        assert!(out.starts_with("refused:"), "{out}");
        assert_eq!(git(repo.path(), &["show-ref"]), before);
    }

    #[tokio::test]
    async fn protected_or_unresolved_native_ref_mutations_preserve_real_refs() {
        let _lock = env_lock().await;
        let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
        let repo = repository();
        let before = git(repo.path(), &["show-ref"]);
        for source in [
            "git branch -D main",
            "git branch -D master",
            "git branch -D stable",
            "git update-ref -d refs/heads/main",
            "git symbolic-ref HEAD refs/heads/main",
            "git branch -f main HEAD",
            "git branch -M main",
            "git switch -C main HEAD",
            "git checkout -B main HEAD",
            "git checkout --force main",
            "git reflog delete 'HEAD@{0}'",
            "git -C . branch -D victim",
            "git branch -D \"$TARGET\"",
            "git stash \"$ACTION\"",
            "echo before && git branch -D victim",
            "git branch --list -D victim",
        ] {
            let mut gate = Gate {
                allow: true,
                requests: vec![],
            };
            let out = run(source, repo.path(), Some(&mut gate)).await;
            assert!(out.starts_with("refused:"), "{source}: {out}");
            assert!(
                gate.requests.is_empty(),
                "unresolved policy must not be approvable: {source}"
            );
            assert_eq!(git(repo.path(), &["show-ref"]), before, "{source}");
            assert_eq!(
                git(repo.path(), &["symbolic-ref", "--short", "HEAD"]).trim(),
                "task"
            );
        }
    }

    #[tokio::test]
    async fn ordinary_native_branch_creation_switching_and_queries_keep_git_behavior() {
        let _lock = env_lock().await;
        let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
        let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
        let repo = repository();
        git(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/native-fixture.git",
            ],
        );
        git(repo.path(), &["config", "native.fixture", "configured"]);
        let main_before = git(repo.path(), &["rev-parse", "refs/heads/main"]);
        let mut gate = Gate {
            allow: false,
            requests: vec![],
        };
        for source in [
            "git branch 'native-created' HEAD",
            "git switch native-created",
            "git switch -c native-switched",
            "git checkout -b native-checkout",
            "git checkout -bfeature-attached-checkout",
            "git switch -cfeature-attached-switch",
        ] {
            let out = run(source, repo.path(), Some(&mut gate)).await;
            assert!(!out.contains("refused:"), "{source}: {out}");
        }
        for branch in [
            "native-created",
            "native-switched",
            "native-checkout",
            "feature-attached-checkout",
            "feature-attached-switch",
        ] {
            assert_eq!(
                git(repo.path(), &["rev-parse", &format!("refs/heads/{branch}")]),
                main_before
            );
        }
        assert_eq!(
            git(repo.path(), &["symbolic-ref", "--short", "HEAD"]).trim(),
            "feature-attached-switch"
        );
        for (source, expected) in [
            (
                "git remote -v",
                "https://example.invalid/native-fixture.git",
            ),
            ("git config --get native.fixture", "configured"),
            ("git status --short --branch", "feature-attached-switch"),
            ("git reflog show -1 --format=%gs", "moving from"),
            ("git symbolic-ref --short HEAD", "feature-attached-switch"),
        ] {
            let out = run(source, repo.path(), Some(&mut gate)).await;
            assert!(out.contains(expected), "{source}: {out}");
            assert!(!out.contains("refused:"), "{source}: {out}");
        }
        assert!(
            gate.requests.is_empty(),
            "ordinary commands must not trigger destructive confirmation"
        );
        assert_eq!(
            git(repo.path(), &["rev-parse", "refs/heads/main"]),
            main_before
        );
    }

    #[tokio::test]
    async fn quoted_native_commit_preserves_head_and_staged_work() {
        let _lock = env_lock().await;
        let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
        let repo = repository();
        std::fs::write(repo.path().join("pending.txt"), "pending work\n").unwrap();
        git(repo.path(), &["add", "pending.txt"]);
        let before = git(repo.path(), &["rev-parse", "HEAD"]);
        for source in [
            "git 'commit' --no-gpg-sign -m 'must not publish'",
            "'git' commit --no-gpg-sign -m 'must not publish'",
        ] {
            let out = run(source, repo.path(), None).await;
            assert!(out.contains("attribution"), "{source}: {out}");
            assert_eq!(git(repo.path(), &["rev-parse", "HEAD"]), before);
            assert_eq!(
                git(repo.path(), &["diff", "--cached", "--name-only"]).trim(),
                "pending.txt"
            );
        }
        for verb in ["merge", "rebase", "cherry-pick", "revert"] {
            assert!(preflight(
                &format!("git '{verb}' HEAD"),
                repo.path(),
                &Caveats::top(),
                &mut None,
                false
            )
            .unwrap_err()
            .contains("attribution"));
            for flag in ["--abort", "--quit"] {
                assert!(preflight(
                    &format!("git '{verb}' '{flag}'"),
                    repo.path(),
                    &Caveats::top(),
                    &mut None,
                    false
                )
                .is_ok());
            }
        }
    }

    #[tokio::test]
    async fn approved_native_deletion_changes_only_requested_branch_or_stash() {
        let _lock = env_lock().await;
        let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
        let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
        let repo = repository();
        let mut gate = Gate {
            allow: true,
            requests: vec![],
        };
        let out = run("git branch -D 'victim'", repo.path(), Some(&mut gate)).await;
        assert!(out.contains("Deleted branch victim"), "{out}");
        assert!(!git(repo.path(), &["branch", "--list", "victim"]).contains("victim"));
        assert_eq!(gate.requests.len(), 1);
        assert_eq!(gate.requests[0].target, "branch-delete");
        assert!(
            git(repo.path(), &["show-ref", "--verify", "refs/heads/main"])
                .contains("refs/heads/main")
        );
        let out = run("git stash drop 'stash@{0}'", repo.path(), Some(&mut gate)).await;
        assert!(out.contains("Dropped stash@{0}"), "{out}");
        assert!(git(repo.path(), &["stash", "list"]).is_empty());
        assert_eq!(gate.requests.len(), 2);
        assert_eq!(gate.requests[1].target, "stash-drop");
    }
}
