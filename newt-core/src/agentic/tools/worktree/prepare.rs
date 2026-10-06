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
            reason: "Create this task worktree. In a confined session, adoption moves workspace access to this destination for the task and makes the original read-only. No neighboring directory is granted.".into(),
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
        authority = allowed.meet(&crate::agentic::permissions::widen_caveats(
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
