//! The shared dispatch guard for session worktree adoption (#2733).
use super::*;
use crate::worktree_adoption::{AdoptedWorktree, Creation};
use crate::Caveats;
use std::path::{Path, PathBuf};

use super::native_git::{invocation, is_git as resembles_git, literal};
pub(super) mod branch;
#[path = "worktree_git.rs"]
mod git_identity;
mod nudge;
mod prepare;

/// Match the actual Git subcommand, never words appearing in path operands.
fn worktree_add_args(words: &[String]) -> Option<&[String]> {
    let (verb, args, _) = invocation(words).ok()?;
    let (action, args) = args.split_first()?;
    (verb == "worktree" && action == "add").then_some(args)
}

/// #2753: advice only, never authority. Inspect the dispatched standalone Git
/// shape and its stderr separately from stdout, before output is combined/spilled.
/// Unrecognized option shapes deliberately receive no error-specific advice.
pub(super) fn creation_failure_hint(
    source: &str,
    envelope: &serde_json::Value,
) -> Option<&'static str> {
    if shell::envelope_outcome(envelope) != crate::ExecOutcome::Failed {
        return None;
    }
    let words = standalone_git_words(source)?;
    // Admission allows only -C before the subcommand, not config overrides.
    let mut global = words.iter().skip(1);
    let mut word = global.next()?;
    while word == "-C" {
        global.next()?;
        word = global.next()?;
    }
    if word != "worktree" {
        return None;
    }
    // Reuse the admission parser's Git/subcommand interpretation. Limit the
    // hint to explicit branch operands, avoiding guesses about HEAD or -d.
    let args = worktree_add_args(&words)?;
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    let branch = match args.as_slice() {
        [path, branch] if !path.starts_with('-') => *branch,
        ["-b" | "-B", branch, _] | ["-b" | "-B", branch, _, _] => *branch,
        _ => return None,
    };
    let stderr = envelope.get("stderr")?.as_str()?;
    // #2778: without -b the final operand must already name a ref. Do not
    // suggest creating it when -b was present and the missing ref is a start point.
    let invalid_reference = format!("fatal: invalid reference: {branch}");
    if args.len() == 2 && stderr.lines().any(|line| line == invalid_reference) {
        return Some("\nWorktree hint: To create a new branch for the worktree, use `git worktree add -b <name> <path>`.");
    }
    let prefix = format!("fatal: '{branch}' is already used by worktree at ");
    stderr.lines().any(|line| line.starts_with(&prefix)).then_some(
        "\nWorktree hint: if you switched the original to that branch, switch it back with `git switch -`. To create a new task branch, pick a new branch name and run `git worktree add -b <new-branch> <path> [<start>]` as a standalone command."
    )
}

/// Literal standalone Git argv, shared by advisory notices and task recording.
fn standalone_git_words(source: &str) -> Option<Vec<String>> {
    let inspection = agent_bridle::inspect_shell(source).ok()?;
    let [command] = inspection.commands.as_slice() else {
        return None;
    };
    if !inspection.constructs.is_empty()
        || !inspection.warnings.is_empty()
        || !command.descendant_execs.is_empty()
        || !command.redirects.is_empty()
        || source.trim() != command.source.trim()
        || !command.program.as_deref().is_some_and(resembles_git)
    {
        return None;
    }
    let mut remaining = command.source.as_str();
    for word in &command.argv {
        remaining = remaining.trim_start().strip_prefix(word)?;
    }
    if !remaining.trim().is_empty() {
        return None;
    }
    let words = command
        .argv
        .iter()
        .map(|word| literal(word))
        .collect::<Option<Vec<_>>>()?;
    if words.iter().any(|word| word.contains(['\n', '\r'])) {
        return None;
    }
    Some(words)
}

/// A conservative refusal trigger, NEVER evidence authorizing adoption. Scan
/// every command independently of cwd/operand resolution and inspect children.
fn possible_creation(source: &str) -> bool {
    fn words(program: Option<&str>, argv: &[String]) -> bool {
        program.is_some_and(resembles_git)
            && argv.windows(2).any(|pair| {
                literal(&pair[0]).as_deref() == Some("worktree")
                    && literal(&pair[1]).as_deref() == Some("add")
            })
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
    match agent_bridle::inspect_shell(source) {
        Ok(inspection) => contains(&inspection),
        Err(_) => {
            // An opaque sibling or malformed syntax can prevent ANY inventory.
            // Text can only force refusal here; it can never mint a candidate.
            static MARKER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
                regex::Regex::new(r"(?is)\bgit(?:\.exe)?\b.*\bworktree\b.*\badd\b")
                    .expect("fixed creation marker")
            });
            let text: String = source
                .replace("\\\n", "")
                .chars()
                .filter(|c| !matches!(c, '\'' | '"' | '\\'))
                .collect();
            MARKER.is_match(&text)
        }
    }
}

/// Keep absence distinct from an unresolved attempt. Once creation appears,
/// neither an ambiguous sibling nor failed candidate resolution may fall open.
fn creation_admission(
    bypass: bool,
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    caveats: &Caveats,
    engine: crate::ShellEngine,
    trusted_git: impl FnOnce(&Caveats) -> Result<PathBuf, ()>,
) -> Result<Option<(Creation, String)>, ()> {
    // Creation verification exists to arm confinement, which ambient execution
    // deliberately disables. Do not probe metadata or pin Git in that mode.
    if bypass
        || name != "run_command"
        || !args
            .get("command")
            .and_then(|v| v.as_str())
            .is_some_and(possible_creation)
    {
        return Ok(None);
    }
    if !creation_batch_is_read_only_after_add(args, engine) {
        return Err(());
    }
    let candidate = creation(name, args, workspace, caveats).ok_or(())?;
    let source = git_identity::pin(args["command"].as_str().ok_or(())?, &trusted_git(caveats)?)?;
    Ok(Some((candidate, source)))
}

fn creation(
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    caveats: &Caveats,
) -> Option<Creation> {
    if name != "run_command" {
        return None;
    }
    let (cd, cmd) = split_leading_cd(args.get("command")?.as_str()?);
    let cwd = resolve_exec_cwd(workspace, args.get("cwd").and_then(|v| v.as_str()));
    let cwd = resolve_exec_cwd(&cwd, cd.as_deref());
    let inspection = agent_bridle::inspect_shell(&cmd).ok()?;
    for command in inspection.commands {
        if command.program.as_deref() == Some("cd") {
            return None;
        }
        if command.program.as_deref()? != "git" {
            continue;
        }
        let words = command
            .argv
            .iter()
            .map(|s| literal(s))
            .collect::<Option<Vec<_>>>()?;
        let Some(add_args) = worktree_add_args(&words) else {
            continue;
        };
        let mut args = words.iter().skip(1);
        let mut git_cwd = PathBuf::from(&cwd);
        let mut word = args.next()?;
        while word == "-C" {
            git_cwd = git_cwd.join(args.next()?);
            word = args.next()?;
        }
        if word != "worktree" {
            return None; // Only the supported -C selector may change Git's cwd.
        }
        let mut args = add_args.iter();
        let mut destination = None;
        while let Some(word) = args.next() {
            match word.as_str() {
                "-b" | "-B" | "--reason" => {
                    args.next()?;
                }
                "--" => {
                    destination = args.next();
                    break;
                }
                "-f" | "--force" | "-d" | "--detach" | "--checkout" | "--no-checkout"
                | "--lock" | "-q" | "--quiet" => {}
                flag if flag.starts_with('-') => return None,
                _ => {
                    destination = Some(word);
                    break;
                }
            }
        }
        // The actual creation repository must also be this session's original.
        let candidate =
            Creation::before(Path::new(workspace), &git_cwd.join(destination?), caveats)?;
        if !candidate.matches_source(&git_cwd) {
            return None;
        }
        // Once found, the whole batch must pass the mutation check below.
        // A later cd/dynamic command must not erase this execution boundary.
        return Some(candidate);
    }
    None
}

// SafeSubset spawns these names externally; only actual shell builtins qualify.
fn display_builtin(program: &str, engine: crate::ShellEngine) -> bool {
    matches!(program, "echo" | "printf" | "pwd") && engine != crate::ShellEngine::SafeSubset
}

// Adoption is an execution boundary: a compound creation must not write the
// original checkout later in the SAME shell before the outer guard can run.
fn creation_batch_is_read_only_after_add(
    args: &serde_json::Value,
    engine: crate::ShellEngine,
) -> bool {
    let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
        return false;
    };
    let (_, command) = split_leading_cd(command);
    let Ok(inspection) = agent_bridle::inspect_shell(&command) else {
        return false;
    };
    if !inspection.constructs.is_empty() {
        return false; // Nested evaluation is not an exact read-only sibling.
    }
    let mut additions = 0;
    let safe = inspection.commands.iter().all(|c| {
        if c.redirects.iter().any(redirect_has_effect) {
            return false;
        }
        let Some(words) = c
            .argv
            .iter()
            .map(|s| literal(s))
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        let args: Vec<_> = words.iter().skip(1).map(String::as_str).collect();
        match c.program.as_deref() {
            Some("git") => {
                if worktree_add_args(&words).is_some() {
                    additions += 1;
                    true
                } else {
                    matches!(
                        args.as_slice(),
                        ["branch", "--show-current"]
                            | ["worktree", "list"]
                            | ["status"]
                            | ["status", "--short"]
                    )
                }
            }
            // Only literal bare builtins have a known implementation here.
            // Exec authority (even for a basename like tail) does not certify
            // read-only behavior. External displays run separately, AFTER
            // adoption; refusing them also closes PATH substitution (#2733).
            Some(program) => display_builtin(program, engine),
            None => false,
        }
    });
    safe && additions == 1
}

struct Guard<'p, 'g> {
    policy: &'p AdoptedWorktree,
    inner: Option<&'g mut dyn PermissionGate>,
}
impl Guard<'_, '_> {
    fn decision(&self, decision: PermissionDecision) -> PermissionDecision {
        match decision {
            PermissionDecision::Allow(c) => PermissionDecision::Allow(self.policy.attenuate(&c)),
            PermissionDecision::Deny => PermissionDecision::Deny,
        }
    }
    fn blocked(&self, requests: &[PermissionRequest]) -> bool {
        requests.iter().any(|r| {
            matches!(r.kind, DenialKind::FsWrite | DenialKind::Build)
                && self.policy.blocked(Path::new(&r.target))
        })
    }
}
impl PermissionGate for Guard<'_, '_> {
    fn refresh_caveats(&mut self, baseline: &Caveats) -> PermissionDecision {
        let decision = self.inner.as_deref_mut().map_or_else(
            || PermissionDecision::Allow(baseline.clone()),
            |g| g.refresh_caveats(baseline),
        );
        self.decision(decision)
    }
    fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
        if self.blocked(requests) {
            return PermissionDecision::Deny;
        }
        let decision = self
            .inner
            .as_deref_mut()
            .map_or(PermissionDecision::Deny, |g| g.ask(requests));
        self.decision(decision)
    }
    fn ask_with_caveats(
        &mut self,
        base: &Caveats,
        requests: &[PermissionRequest],
    ) -> PermissionDecision {
        if self.blocked(requests) {
            return PermissionDecision::Deny;
        }
        let decision = self
            .inner
            .as_deref_mut()
            .map_or(PermissionDecision::Deny, |g| {
                g.ask_with_caveats(base, requests)
            });
        self.decision(decision)
    }
    fn ask_question(&mut self, q: &str) -> super::super::permissions::HumanQuestionOutcome {
        self.inner.as_deref_mut().map_or(
            super::super::permissions::HumanQuestionOutcome::Unavailable,
            |g| g.ask_question(q),
        )
    }
    fn consume_pending_once(&mut self, kind: DenialKind, target: &str) {
        if let Some(g) = self.inner.as_deref_mut() {
            g.consume_pending_once(kind, target);
        }
    }
    fn queue_pending_once(&mut self, kind: DenialKind, target: &str) {
        if let Some(g) = self.inner.as_deref_mut() {
            g.queue_pending_once(kind, target);
        }
    }
    fn apply_pending_once(&mut self, kind: DenialKind, target: &str, base: &Caveats) -> Caveats {
        let c = self.inner.as_deref_mut().map_or_else(
            || base.clone(),
            |g| g.apply_pending_once(kind, target, base),
        );
        self.policy.attenuate(&c)
    }
}

const UNARMED_NOTICE: &str = "Warning: worktree protection is not armed because OCAP is disabled; the original checkout remains writable under existing permissions. Use a confined session for automatic original-checkout protection.";

/// Verified creation can only arm a fence when the executor honors it. The
/// bypass is frozen launch policy; broad grants alone do not disable adoption.
fn record_verified_creation(
    session: &crate::worktree_adoption::WorktreeSession,
    adopted: AdoptedWorktree,
    bypass: bool,
) -> String {
    if let Some(branch) = &adopted.task_branch {
        session.record_task_worktree(&adopted.worktree, branch);
    }
    if bypass {
        format!(
            "Task worktree created: {}. {UNARMED_NOTICE}",
            adopted.worktree.display()
        )
    } else {
        let notice = format!(
            "Adopted task worktree: {}. The original checkout and shared config are now read-only for this task. Use git -c user.name=… -c user.email=… for per-command identity. Any uncommitted changes in the original checkout are now read-only; copy them into the new worktree and commit there, or ask the operator for /permissions worktree-lift.",
            adopted.worktree.display()
        );
        session.adopt(adopted);
        notice
    }
}

fn report_leftover(
    presentation: &mut dyn ToolPresentation,
    candidate: &Creation,
    mut result: String,
) -> String {
    if let Some(notice) = candidate.leftover_notice() {
        presentation.preview(&notice, 0);
        result.push_str(&format!("\n{notice}"));
    }
    result
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute(
    presentation: &mut dyn ToolPresentation,
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    color: bool,
    tool_output_lines: usize,
    caveats: &Caveats,
    mcp: &mut dyn McpTools,
    collab: ToolCollaborators<'_, '_>,
    tool_offload: bool,
    disposition: PromptDisposition,
) -> String {
    let bypass = ocap_disabled();
    let session = collab.worktree_session;
    // #2771: choose the file-tool base once, before both the adoption fence
    // and native dispatch. This changes path resolution, never authority.
    // Shell calls (including routed file reads) retain their explicit cwd.
    let task_root = session.and_then(|session| {
        matches!(
            name,
            "read_file" | "write_file" | "edit_file" | "delete_file" | "list_dir" | "find" | "grep"
        )
        .then(|| session.task_root(Path::new(workspace)))
        .flatten()
    });
    // Absolute searches retain the original workspace-only boundary. Only
    // relative operands select the task's search root; no fence is widened.
    let absolute_search = matches!(name, "find" | "grep")
        && args
            .get("path")
            .and_then(|p| p.as_str())
            .is_some_and(|p| Path::new(p).is_absolute());
    let workspace = task_root
        .as_deref()
        .filter(|_| !absolute_search)
        .and_then(Path::to_str)
        .unwrap_or(workspace);
    let objective = collab.prompt_context.map(|context| context.active_text());
    let command_directory = collab.command_directory;
    let policy = session.and_then(crate::worktree_adoption::WorktreeSession::snapshot);
    let mut collab = collab;
    // Project the approved workspace switch before refreshing current ceilings.
    // Native file tools must see the same limits as the shell; never re-project
    // after a gate has removed an axis or refused the current policy.
    let task_authority = if let Some(policy) = &policy {
        let projected = policy.attenuate(&policy.task_authority(caveats));
        let current = match collab.permission_gate.as_deref_mut() {
            Some(gate) => match gate.refresh_caveats(&projected) {
                PermissionDecision::Allow(current) => current,
                PermissionDecision::Deny => {
                    if let Some(invocation) = collab.invocation {
                        invocation.host();
                    }
                    if let Some(slot) = collab.execution {
                        let _ = slot.set(crate::ExecOutcome::Denied);
                    }
                    return "capability denied: current permissions refuse task worktree access"
                        .into();
                }
            },
            None => projected,
        };
        Some(policy.attenuate(&current))
    } else {
        None
    };
    let caveats = task_authority.as_ref().unwrap_or(caveats);
    let mut normalized =
        shell::command_args_with_default_cwd(name, args, workspace, collab.default_command_cwd)
            .unwrap_or(std::borrow::Cow::Borrowed(args));
    let admission = session.filter(|_| policy.is_none()).map_or(Ok(None), |_| {
        creation_admission(
            bypass,
            name,
            &normalized,
            workspace,
            caveats,
            shell::shell_engine(),
            git_identity::resolve,
        )
    });
    let Ok(candidate) = admission else {
        if let Some(invocation) = collab.invocation {
            invocation.host();
        }
        if let Some(slot) = collab.execution {
            let _ = slot.set(crate::ExecOutcome::Denied);
        }
        let protection = if bypass {
            "; OCAP is disabled, so automatic worktree protection will not be armed"
        } else {
            " so it can become read-only before other mutations run"
        };
        return format!("capability denied: cannot verify this worktree creation and its surrounding commands; run `git worktree add -b <new-branch> <path> [<start>]` as a standalone literal command in the original checkout{protection}. Do not create the branch in the original checkout first; -b creates it for the new worktree");
    };
    if candidate
        .as_ref()
        .is_some_and(|(candidate, _)| candidate.nested_in_original())
    {
        if let Some(invocation) = collab.invocation {
            invocation.host();
        }
        if let Some(slot) = collab.execution {
            let _ = slot.set(crate::ExecOutcome::Denied);
        }
        return "capability denied: the task worktree must be outside the original checkout; choose a sibling destination, for example `git worktree add -b <new-branch> ../<name> [<start>]`".into();
    }
    let mut candidate = candidate.map(|(candidate, source)| {
        normalized.to_mut()["command"] = source.into();
        candidate
    });
    let creation_authority = if let Some(candidate) = candidate.as_mut() {
        match prepare::creation(
            candidate,
            &normalized,
            workspace,
            caveats,
            &mut collab.permission_gate,
        ) {
            Ok(authority) => Some(authority),
            Err(reason) => {
                if let Some(invocation) = collab.invocation {
                    invocation.host();
                }
                if let Some(slot) = collab.execution {
                    let _ = slot.set(crate::ExecOutcome::Denied);
                }
                return report_leftover(
                    presentation,
                    candidate,
                    format!("capability denied: {reason}"),
                );
            }
        }
    } else {
        None
    };
    let caveats = creation_authority.as_ref().unwrap_or(caveats);
    let args = if candidate.is_some() {
        normalized.as_ref()
    } else {
        args
    };
    let execution = collab.execution;
    if let Some(candidate) = &candidate {
        if let Err(reason) = candidate.ready() {
            if let Some(invocation) = collab.invocation {
                invocation.host();
            }
            if let Some(slot) = execution {
                let _ = slot.set(crate::ExecOutcome::Denied);
            }
            return report_leftover(
                presentation,
                candidate,
                format!("capability denied: {reason}"),
            );
        }
    }
    let mut result = if let Some(policy) = &policy {
        let refuse_git = name == "git"
            && !matches!(
                args.get("op").and_then(|v| v.as_str()).unwrap_or(""),
                "status" | "log" | "diff" | "show" | "branch-list" | "blame" | "rev-parse"
            )
            && policy.blocked(
                &Path::new(workspace).join(args.get("cwd").and_then(|v| v.as_str()).unwrap_or(".")),
            );
        let refuse = matches!(name, "write_file" | "edit_file" | "delete_file")
            && args
                .get("path")
                .and_then(|v| v.as_str())
                .is_some_and(|p| policy.blocked(&Path::new(workspace).join(p)));
        // These modes deliberately bypass kernel enforcement. A task guard
        // must not silently become advisory under that explicit bypass.
        if refuse_git
            || !policy.valid(caveats)
            || refuse
            || (bypass && matches!(name, "run_command" | "lifecycle" | "build_exec"))
        {
            if let Some(invocation) = collab.invocation {
                invocation.host();
            }
            if let Some(slot) = execution {
                let _ = slot.set(crate::ExecOutcome::Denied);
            }
            return policy.notice();
        }
        let narrowed = policy.attenuate(caveats);
        let mut gate = Guard {
            policy,
            inner: collab.permission_gate.take(),
        };
        execute_tool_unadopted(
            presentation,
            name,
            args,
            workspace,
            color,
            tool_output_lines,
            &narrowed,
            mcp,
            ToolCollaborators {
                permission_gate: Some(&mut gate),
                ..collab
            },
            tool_offload,
            disposition,
        )
        .await
    } else if let Some(candidate) = &candidate {
        let mut guard = prepare::Guard {
            candidate,
            inner: collab.permission_gate.take(),
        };
        execute_tool_unadopted(
            presentation,
            name,
            args,
            workspace,
            color,
            tool_output_lines,
            caveats,
            mcp,
            ToolCollaborators {
                permission_gate: Some(&mut guard),
                ..collab
            },
            tool_offload,
            disposition,
        )
        .await
    } else {
        execute_tool_unadopted(
            presentation,
            name,
            args,
            workspace,
            color,
            tool_output_lines,
            caveats,
            mcp,
            collab,
            tool_offload,
            disposition,
        )
        .await
    };
    if name == "run_command"
        && execution.and_then(|slot| slot.get()) == Some(&crate::ExecOutcome::Passed)
    {
        if let (Some(session), Some(objective), Some(raw)) = (
            session,
            objective,
            normalized.get("command").and_then(|value| value.as_str()),
        ) {
            let (cd, command) = shell::split_leading_cd(raw);
            let base = shell::resolve_exec_cwd(
                workspace,
                normalized.get("cwd").and_then(|value| value.as_str()),
            );
            let fallback = PathBuf::from(shell::resolve_exec_cwd(&base, cd.as_deref()));
            let cwd = command_directory
                .and_then(|slot| slot.get())
                .unwrap_or(&fallback);
            if let Some(hint) =
                nudge::branch_in_place(session, objective, &command, cwd, &caveats.fs_read)
            {
                result.push_str(&format!("\n{hint}"));
            }
        }
    }
    // This describes launch policy, not verified creation success. Recognition
    // is text-only so even unsupported platforms never enter the verifier.
    if bypass
        && session.is_some()
        && name == "run_command"
        && normalized
            .get("command")
            .and_then(|value| value.as_str())
            .is_some_and(possible_creation)
    {
        if let Some(session) = session {
            nudge::record_unarmed_creation(
                session,
                &normalized,
                workspace,
                execution.and_then(|slot| slot.get()),
            );
        }
        presentation.preview(UNARMED_NOTICE, 0);
        result.push_str(&format!("\n{UNARMED_NOTICE}"));
    }
    if let Some(candidate) = &candidate {
        result = report_leftover(presentation, candidate, result);
    }
    if let (Some(session), Some(candidate)) = (session, candidate) {
        if matches!(
            execution.and_then(|slot| slot.get()),
            Some(crate::ExecOutcome::Passed | crate::ExecOutcome::Failed)
        ) {
            if let Some(adopted) = candidate.verify() {
                // Reuse the existing bind-once identity cache for later native
                // Git dispatch; never replace an identity another call pinned.
                let _ = crate::git_hardening::ambient_gitdir_write_grant(&adopted.worktree);
                let notice = record_verified_creation(session, adopted, bypass);
                if bypass {
                    // Visible to the operator even when shell output has an
                    // independently folded/overridden result presentation.
                    presentation.preview(&notice, 0);
                }
                result.push_str(&format!("\n{notice}"));
            }
        }
    }
    if let Some(policy) = &policy {
        // Shell denials are classified from their structured filesystem evidence
        // at dispatch. An exec miss or pending grant is not an adoption fence.
        if name != "run_command"
            && execution.and_then(|slot| slot.get()) == Some(&crate::ExecOutcome::Denied)
        {
            result.push_str(&format!("\n{}", policy.notice()));
        } else if execution.and_then(|slot| slot.get()) == Some(&crate::ExecOutcome::Failed) {
            result.push_str(&format!(
                "\nTask worktree: {}. The original checkout remains read-only.",
                policy.worktree.display()
            ));
        }
    }
    result
}

#[cfg(test)]
#[path = "../tools_tests/worktree_adoption.rs"]
mod tests;

#[cfg(test)]
#[path = "../tools_tests/worktree_adoption_round2.rs"]
mod round2_tests;

#[cfg(test)]
#[path = "../tools_tests/worktree_adoption_round3.rs"]
mod round3_tests;

#[cfg(test)]
#[path = "../tools_tests/worktree_adoption_round5.rs"]
mod round5_tests;

#[cfg(test)]
#[path = "../tools_tests/worktree_adoption_round6.rs"]
mod round6_tests;

#[cfg(test)]
#[path = "../tools_tests/worktree_adoption_fullaccess.rs"]
mod fullaccess_tests;

#[cfg(all(test, target_os = "linux"))]
#[path = "../tools_tests/worktree_adoption_refs.rs"]
mod refs_tests;

#[cfg(all(test, target_os = "linux"))]
#[path = "../tools_tests/worktree_adoption_nested.rs"]
mod nested_tests;

#[cfg(all(test, target_os = "linux"))]
#[path = "../tools_tests/worktree_adoption_sibling.rs"]
mod sibling_tests;

#[cfg(test)]
mod invalid_reference_tests {
    use super::*;

    /// #2778: only the missing explicit branch of worktree add gets -b advice.
    #[test]
    fn worktree_invalid_reference_2778_requires_matching_git_failure() {
        let hint = "\nWorktree hint: To create a new branch for the worktree, use `git worktree add -b <name> <path>`.";
        let failed = |stderr: &str| serde_json::json!({"exit_code":128,"stderr":stderr});
        for command in [
            "git worktree add ../task task",
            "git -C repo worktree add '../task path' task",
        ] {
            assert_eq!(
                creation_failure_hint(command, &failed("fatal: invalid reference: task\r\n")),
                Some(hint)
            );
        }
        for (command, envelope) in [
            (
                "git worktree add ../task task",
                failed("prefix fatal: invalid reference: task"),
            ),
            (
                "git worktree add ../task task",
                failed("fatal: invalid reference: task-extra"),
            ),
            (
                "git worktree add ../task task",
                failed("fatal: invalid reference: other"),
            ),
            (
                "git checkout task",
                failed("fatal: invalid reference: task"),
            ),
            (
                "echo 'git worktree add ../task task'",
                failed("fatal: invalid reference: task"),
            ),
            (
                "git worktree add -b task ../task missing",
                failed("fatal: invalid reference: missing"),
            ),
            (
                "git worktree add ../task task",
                serde_json::json!({"exit_code":0,"stderr":"fatal: invalid reference: task"}),
            ),
            (
                "git worktree add ../task task",
                serde_json::json!({"exit_code":128,"stdout":"fatal: invalid reference: task","stderr":""}),
            ),
            (
                "git worktree add ../task task",
                serde_json::json!({"exit_code":128,"stderr":"fatal: invalid reference: task","denied":true}),
            ),
            (
                "echo fake; git worktree add ../task task",
                failed("fatal: invalid reference: task"),
            ),
        ] {
            assert!(
                creation_failure_hint(command, &envelope).is_none(),
                "{command}: {envelope}"
            );
        }
    }
}
