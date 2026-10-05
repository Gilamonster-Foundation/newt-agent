//! The shared dispatch guard for session worktree adoption (#2733).
use super::*;
use crate::worktree_adoption::{AdoptedWorktree, Creation};
use crate::Caveats;
use std::path::{Path, PathBuf};

use super::native_git::{invocation, is_git, literal};

/// Match the actual Git subcommand, never words appearing in path operands.
fn worktree_add_args(words: &[String]) -> Option<&[String]> {
    let (verb, args, _) = invocation(words).ok()?;
    let (action, args) = args.split_first()?;
    (verb == "worktree" && action == "add").then_some(args)
}

/// A conservative refusal trigger, NEVER evidence authorizing adoption. Scan
/// every command independently of cwd/operand resolution and inspect children.
fn possible_creation(source: &str) -> bool {
    fn words(program: Option<&str>, argv: &[String]) -> bool {
        program.is_some_and(is_git)
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
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
    caveats: &Caveats,
) -> Result<Option<Creation>, ()> {
    if name != "run_command"
        || !args
            .get("command")
            .and_then(|v| v.as_str())
            .is_some_and(possible_creation)
    {
        return Ok(None);
    }
    if !creation_batch_is_read_only_after_add(args) {
        return Err(());
    }
    creation(name, args, workspace, caveats).map(Some).ok_or(())
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
        if !is_git(command.program.as_deref()?) {
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
fn creation_batch_is_read_only_after_add(args: &serde_json::Value) -> bool {
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
            Some(program) if is_git(program) => {
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
            Some(program) => display_builtin(program, shell::shell_engine()),
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
    let session = collab.worktree_session;
    let policy = session.and_then(crate::worktree_adoption::WorktreeSession::snapshot);
    let normalized =
        shell::command_args_with_default_cwd(name, args, workspace, collab.default_command_cwd)
            .unwrap_or(std::borrow::Cow::Borrowed(args));
    let admission = session.filter(|_| policy.is_none()).map_or(Ok(None), |_| {
        creation_admission(name, &normalized, workspace, caveats)
    });
    let Ok(candidate) = admission else {
        if let Some(invocation) = collab.invocation {
            invocation.host();
        }
        if let Some(slot) = collab.execution {
            let _ = slot.set(crate::ExecOutcome::Denied);
        }
        return "capability denied: cannot verify this worktree creation and its surrounding commands; run git worktree add as a standalone literal command in the original checkout so it can become read-only before other mutations run".into();
    };
    let execution = collab.execution;
    let mut collab = collab;
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
            || (ocap_disabled() && matches!(name, "run_command" | "lifecycle" | "build_exec"))
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
    if let (Some(session), Some(candidate)) = (session, candidate) {
        if matches!(
            execution.and_then(|slot| slot.get()),
            Some(crate::ExecOutcome::Passed | crate::ExecOutcome::Failed)
        ) {
            if let Some(adopted) = candidate.verify() {
                // Reuse the existing bind-once identity cache for later native
                // Git dispatch; never replace an identity another call pinned.
                let _ = crate::git_hardening::ambient_gitdir_write_grant(&adopted.worktree);
                result.push_str(&format!("\nAdopted task worktree: {}. The original checkout is now read-only for this task.", adopted.worktree.display()));
                session.adopt(adopted);
            }
        }
    }
    if let Some(policy) = &policy {
        if execution.and_then(|slot| slot.get()) == Some(&crate::ExecOutcome::Denied) {
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
