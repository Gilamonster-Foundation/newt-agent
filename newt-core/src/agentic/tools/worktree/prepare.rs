//! #2757: approve creating and adopting the exact destination before host mkdir.
use super::*;
use crate::Scope;

pub(super) fn creation(
    candidate: &mut Creation,
    args: &serde_json::Value,
    workspace: &str,
    base: &Caveats,
    gate: &mut Option<&mut dyn PermissionGate>,
) -> Result<Caveats, String> {
    let mut authority = match gate.as_deref_mut() {
        Some(gate) => match gate.refresh_caveats(base) {
            PermissionDecision::Allow(current) => current,
            PermissionDecision::Deny => {
                return Err("current permissions refuse worktree creation".into())
            }
        },
        None => base.clone(),
    };
    // Validate the complete caller manifest before prompting or creating paths.
    let mut requests = shell::declared_filesystem_requests(
        args,
        args["command"].as_str().unwrap_or(""),
        workspace,
    )?;
    let target = candidate.destination().to_string_lossy().into_owned();
    for kind in [DenialKind::FsRead, DenialKind::FsWrite] {
        requests.retain(|request| request.kind != kind || request.target != target);
        requests.push(PermissionRequest {
            tool: "run_command".into(), kind, target: target.clone(),
            reason: format!("Create this task worktree, including its new branch ref and worktree registration in {} for this command only. No standing Git metadata write access is granted. In a confined session, adoption moves workspace access to this destination for the task and makes the original read-only. No neighboring directory is granted.", task_path_literal(candidate.repository().1)),
            harness_bound: true,
        });
    }
    let missing: Vec<_> = requests
        .iter()
        .filter(|request| !shell::permits_filesystem_request(&authority, request))
        .cloned()
        .collect();
    if !missing.is_empty() {
        let gate = gate
            .as_deref_mut()
            .ok_or("worktree destination requires operator approval")?;
        let PermissionDecision::Allow(allowed) = gate.ask_with_caveats(&authority, &missing) else {
            return Err("worktree destination was not approved".into());
        };
        let grants: Vec<_> = missing.iter().map(|r| (r.kind, r.target.clone())).collect();
        authority = allowed.meet(&newt_policy::permissions::widen_caveats(
            &authority, &grants,
        ));
        if !requests
            .iter()
            .all(|request| shell::permits_filesystem_request(&authority, request))
        {
            return Err("worktree destination approval did not include the required access".into());
        }
        for request in &missing {
            gate.consume_pending_once(request.kind, &request.target);
        }
    }
    candidate.prepare(&authority)?;
    Ok(authority)
}

/// The existing shell path remains available with preexisting metadata write
/// authority. Otherwise only the bounded native creation can use the approval.
pub(in crate::agentic::tools) async fn broker_creation(
    candidate: &Creation,
    source: &str,
    authority: &Caveats,
) -> Result<crate::confined_exec::ConfinedOutput, String> {
    #[cfg(all(feature = "confine", any(target_os = "linux", target_os = "macos")))]
    {
        let (branch, start) = broker_operands(source).ok_or(
            "read-only Git metadata requires a standalone `git worktree add -b <new-branch> <path> [<start>]`; run display commands separately",
        )?;
        let git = git_identity::resolve(authority)
            .map_err(|()| "cannot authenticate Git for approved worktree creation")?;
        crate::native_git_broker::create_worktree(candidate, &git, &branch, &start, authority).await
    }
    #[cfg(not(all(feature = "confine", any(target_os = "linux", target_os = "macos"))))]
    {
        let _ = (candidate, source, authority);
        Err("confined worktree creation is unavailable in this build".into())
    }
}

#[cfg(any(
    all(feature = "confine", any(target_os = "linux", target_os = "macos")),
    test
))]
fn broker_operands(source: &str) -> Option<(String, String)> {
    let (_, source) = split_leading_cd(source);
    let words = standalone_git_words(&source)?;
    // The admission parser permits only -C before worktree; keep the broker's
    // authority predicate equally narrow even if another caller is added.
    let mut global = words.iter().skip(1);
    let mut verb = global.next()?;
    while verb == "-C" {
        global.next()?;
        verb = global.next()?;
    }
    if verb != "worktree" {
        return None;
    }
    let args: Vec<_> = worktree_add_args(&words)?
        .iter()
        .map(String::as_str)
        .collect();
    match args.as_slice() {
        ["-b", branch, _] => Some(((*branch).into(), "HEAD".into())),
        ["-b", branch, _, start] => Some(((*branch).into(), (*start).into())),
        _ => None,
    }
}

/// Recheck after late permission callbacks as well as at the dispatch entry.
/// A prompt/refresh may yield control after the directory was prepared.
pub(super) struct Guard<'c, 'g> {
    pub(super) candidate: &'c Creation,
    pub(super) inner: Option<&'g mut dyn PermissionGate>,
}
impl Guard<'_, '_> {
    fn checked(&self, decision: PermissionDecision) -> PermissionDecision {
        if self.candidate.ready().is_err() {
            PermissionDecision::Deny
        } else {
            decision
        }
    }
}
impl PermissionGate for Guard<'_, '_> {
    fn refresh_caveats(&mut self, base: &Caveats) -> PermissionDecision {
        let decision = self.inner.as_deref_mut().map_or_else(
            || PermissionDecision::Allow(base.clone()),
            |g| g.refresh_caveats(base),
        );
        self.checked(decision)
    }
    fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
        let decision = self
            .inner
            .as_deref_mut()
            .map_or(PermissionDecision::Deny, |g| g.ask(requests));
        self.checked(decision)
    }
    fn ask_with_caveats(
        &mut self,
        base: &Caveats,
        requests: &[PermissionRequest],
    ) -> PermissionDecision {
        let decision = self
            .inner
            .as_deref_mut()
            .map_or(PermissionDecision::Deny, |g| {
                g.ask_with_caveats(base, requests)
            });
        self.checked(decision)
    }
    fn ask_dependency_fetch(
        &mut self,
        prepared: &Caveats,
        request: &PermissionRequest,
    ) -> PermissionDecision {
        let decision = self
            .inner
            .as_deref_mut()
            .map_or(PermissionDecision::Deny, |g| {
                g.ask_dependency_fetch(prepared, request)
            });
        self.checked(decision)
    }
    fn ask_question(&mut self, q: &str) -> HumanQuestionOutcome {
        self.inner
            .as_deref_mut()
            .map_or(HumanQuestionOutcome::Unavailable, |g| g.ask_question(q))
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
    fn queue_command_retry(&mut self, command: &str, cwd: &str, requests: &[PermissionRequest]) {
        if let Some(g) = self.inner.as_deref_mut() {
            g.queue_command_retry(command, cwd, requests);
        }
    }
    fn apply_command_retry(&mut self, command: &str, cwd: &str, base: &Caveats) -> Caveats {
        let current = self.inner.as_deref_mut().map_or_else(
            || base.clone(),
            |g| g.apply_command_retry(command, cwd, base),
        );
        if self.candidate.ready().is_ok() {
            current
        } else {
            Caveats {
                exec: Scope::none(),
                fs_read: Scope::none(),
                fs_write: Scope::none(),
                net: Scope::none(),
                ..current
            }
        }
    }
    fn apply_pending_once(&mut self, kind: DenialKind, target: &str, base: &Caveats) -> Caveats {
        let current = self.inner.as_deref_mut().map_or_else(
            || base.clone(),
            |g| g.apply_pending_once(kind, target, base),
        );
        if self.candidate.ready().is_ok() {
            current
        } else {
            Caveats {
                exec: Scope::none(),
                fs_read: Scope::none(),
                fs_write: Scope::none(),
                net: Scope::none(),
                ..current
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::broker_operands;

    /// The real creation pipeline grounds this mocked argv authority boundary.
    #[test]
    fn broker_admits_only_standalone_new_branch_creation() {
        for source in [
            "git worktree add -b task ../task",
            "cd /repo && git worktree add -b task ../task HEAD",
            "'/usr/bin/git' -C /repo worktree add -b task ../task HEAD",
        ] {
            assert_eq!(
                broker_operands(source),
                Some(("task".into(), "HEAD".into())),
                "{source}"
            );
        }
        for source in [
            "git worktree add -B main ../task",
            "git -c core.hooksPath=hooks worktree add -b task ../task",
            "git --git-dir=/other worktree add -b task ../task",
            "git worktree add -b task ../task; echo extra",
            "git worktree add -b task ../task | tail -5",
            "git worktree add -b task ../task > output",
            "git worktree add -b task $(pwd)",
        ] {
            assert_eq!(broker_operands(source), None, "{source}");
        }
    }
}
