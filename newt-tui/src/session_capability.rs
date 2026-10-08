//! The session's signed operating capability: establish it once, narrow it
//! only.
//!
//! Rooted in the per-user key and the configured preset, and enforcing
//! in-session monotonic narrowing — a reapplied policy can tighten the live
//! authority but never widen it. The policy it is built from (`policy_for`,
//! `mint_operating_key`, `resolve_tui`) stays in `super`: those are consumed
//! by permissions.rs and chat.rs independently of this type.

use super::*;

/// The session's signed operating capability.
///
/// Established once from the per-user key (`~/.newt/identity.pem`) and the
/// configured preset, it enforces **in-session monotonic narrowing**:
/// re-applying a policy (e.g. after a config reload) can only ever *narrow* the live
/// authority, never widen it — widening would require re-rooting from the user
/// key, which only happens on a fresh session. The running agent can tighten its
/// own leash but never loosen it.
///
/// Safe by default: an absent config, or any identity error, yields read-only —
/// never `Caveats::top()`. The one exception is the explicit per-invocation
/// `--full-access` / `NEWT_FULL_ACCESS=1` preset override (see `policy_for`),
/// which is operator-asserted per run and loudly surfaced at session start.
pub(crate) struct SessionCapability {
    /// The live operating key. `None` if the per-user key is unavailable; the
    /// capability then degrades to a plain caveats floor (still narrowing-only).
    op: Option<newt_identity::AgentKey>,
    caveats: newt_core::caveats::Caveats,
    delegation: Option<newt_identity::VerifiedDelegation>,
    frozen_at_launch: bool,
    #[cfg(target_os = "macos")]
    exec_pins: newt_core::exec_grants::ExecPins,
    #[cfg(target_os = "macos")]
    exec_notice: Option<String>,
}

impl SessionCapability {
    /// Establish the session capability from the configured policy + per-user key.
    #[cfg(test)]
    pub(crate) fn establish(
        tui: Option<newt_core::TuiConfig>,
        key_path: Option<&std::path::Path>,
        workspace: &str,
        delegation: Option<newt_identity::VerifiedDelegation>,
    ) -> Self {
        let policy = policy_for(tui, workspace);
        Self::from_policy(policy, key_path, delegation, None)
    }

    /// Establish an operator-reviewed launch policy independently of mutable
    /// configuration. Saved profile edits are applied by a new process only.
    #[cfg(test)]
    pub(crate) fn establish_frozen(
        policy: newt_core::Caveats,
        key_path: Option<&std::path::Path>,
        delegation: Option<newt_identity::VerifiedDelegation>,
    ) -> Self {
        let mut session = Self::from_policy(policy, key_path, delegation, None);
        session.frozen_at_launch = true;
        session
    }

    /// Saved approvals must participate in pin exclusions before signing.
    /// They remain recalled grants, not additions to the signed base policy.
    pub(crate) fn establish_launch(
        policy: newt_core::Caveats,
        key_path: Option<&std::path::Path>,
        frozen: bool,
        permissions: &crate::permissions::PermissionPromptState,
    ) -> anyhow::Result<Self> {
        let writes = permissions.recalled_caveats(&policy, None)?.fs_write;
        let mut session = Self::from_policy(policy, key_path, None, Some(&writes));
        session.frozen_at_launch = frozen;
        Ok(session)
    }

    fn from_policy(
        policy: newt_core::Caveats,
        key_path: Option<&std::path::Path>,
        delegation: Option<newt_identity::VerifiedDelegation>,
        effective_writes: Option<&newt_core::Scope<String>>,
    ) -> Self {
        #[cfg(not(target_os = "macos"))]
        let _ = effective_writes;
        // A delegated session inherits its ceiling; it does not establish one.
        //
        // `key_path` is deliberately still accepted and deliberately still
        // ignored here. The dangerous shape is not "a delegated session with no
        // key" — it is a delegated session that CAN see an operator key, on
        // disk, at a path it was handed, and must decline to root itself in it
        // anyway. So the enforcement is written for that case rather than for
        // the absence of the temptation: no key is minted, no existing key is
        // read or rewritten, and `op` stays `None`, which is what makes
        // `plugin_envelope_for` refuse to mint nested envelopes.
        //
        // The ceiling is MET with the local policy, never taken from it: a
        // delegated session may narrow itself further (a tighter preset still
        // applies) but the parent's signed ceiling is the cap. That is
        // attenuate-never-amplify at the session boundary — the same law
        // `enforced_caveats` provides on the ordinary path.
        if let Some(d) = delegation {
            let caveats = policy.meet(d.caveats());
            return Self {
                op: None,
                caveats,
                delegation: Some(d),
                frozen_at_launch: false,
                #[cfg(target_os = "macos")]
                exec_pins: Default::default(),
                #[cfg(target_os = "macos")]
                exec_notice: None,
            };
        }
        // Resolve only at the root session boundary, after all write grants.
        // Delegated sessions above retain their signed ceiling unchanged.
        #[cfg(target_os = "macos")]
        let (policy, exec_pins, exec_notice) = {
            let mut policy = policy;
            let path = newt_core::exec_grants::dispatch_path();
            let writes = effective_writes.unwrap_or(&policy.fs_write).clone();
            let (pins, unresolved) = pin_exec_policy(&mut policy, &writes, path.as_deref());
            let notice = (!unresolved.is_empty()).then(|| format!(
                "Executable grants kept as basenames (not found or untrusted on PATH): {}. Confined execution may need an explicit trusted executable path.", unresolved.join(", ")));
            (policy, pins, notice)
        };
        let op = key_path.and_then(|p| mint_operating_key(p, &policy).ok());
        let caveats = match &op {
            Some(k) => newt_identity::enforced_caveats(k).unwrap_or(policy),
            None => policy,
        };
        Self {
            op,
            caveats,
            delegation: None,
            frozen_at_launch: false,
            #[cfg(target_os = "macos")]
            exec_pins,
            #[cfg(target_os = "macos")]
            exec_notice,
        }
    }

    /// Render once through the chat surface, never directly over its terminal.
    #[cfg(target_os = "macos")]
    pub(crate) fn take_exec_notice(&mut self) -> Option<String> {
        self.exec_notice.take()
    }

    /// The inherited ceiling is distinct from a removable named posture.
    pub(crate) fn delegation(&self) -> Option<&newt_identity::VerifiedDelegation> {
        self.delegation.as_ref()
    }

    /// The active enforcement caveats the tool loop consults.
    pub(crate) fn caveats(&self) -> &newt_core::caveats::Caveats {
        &self.caveats
    }

    /// Mint a plugin-side envelope for a subprocess running under `role`
    /// with `child_caveats`, by delegating from the live operating key.
    ///
    /// **Issue #93:** when the TUI eventually spawns a subprocess
    /// plugin (today: in-process tool calls only), the resulting
    /// `AgentKey` it hands the plugin MUST chain back to the operator's
    /// `UserKey` from `~/.newt/identity.pem` — never a synthetic key
    /// minted at spawn time. This helper is the chokepoint the TUI's
    /// future subprocess-spawn path will route through.
    ///
    /// Returns:
    /// - `Some(Ok(envelope))` when the operating key is present and the
    ///   delegation succeeded — the envelope's cert chain roots back to
    ///   the operator.
    /// - `Some(Err(_))` when delegation refused (`child_caveats` would
    ///   amplify the operating key's authority).
    /// - `None` when the per-user key is unavailable (`SessionCapability`
    ///   degraded to a plain caveats floor).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn plugin_envelope_for(
        &self,
        role: &str,
        child_caveats: newt_core::Caveats,
    ) -> Option<std::result::Result<String, newt_identity::EnvelopeError>> {
        let op = self.op.as_ref()?;
        Some(newt_identity::delegate_for_plugin(op, role, child_caveats))
    }

    /// Re-apply a (possibly changed) policy, **narrowing-only**. Returns `true`
    /// if the request asked for *more* authority than the session currently
    /// holds and was therefore clamped (so the caller can tell the user a
    /// restart is required to widen).
    pub(crate) fn reapply(&mut self, tui: Option<newt_core::TuiConfig>, workspace: &str) -> bool {
        if self.frozen_at_launch {
            return false;
        }
        let requested = policy_for(tui, workspace);
        #[cfg(target_os = "macos")]
        let requested = {
            let mut requested = requested;
            self.exec_pins.apply(&mut requested);
            requested
        };
        let narrowed = requested.meet(&self.caveats);
        let clamped = narrowed != requested;
        if let Some(op) = self.op.take() {
            match newt_identity::attenuate(&op, &narrowed)
                .and_then(|child| newt_identity::enforced_caveats(&child).map(|c| (child, c)))
            {
                Ok((child, c)) => {
                    self.op = Some(child);
                    self.caveats = c;
                }
                // Unreachable in practice (narrowed ⊑ op): keep the old key but
                // still apply the narrowed caveats.
                Err(_) => {
                    self.op = Some(op);
                    self.caveats = narrowed;
                }
            }
        } else {
            self.caveats = narrowed;
        }
        clamped
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    /// agent-bridle #421: the actual session constructor must pin before mint,
    /// including an operator-reviewed frozen profile.
    #[test]
    fn macos_session_pins_before_mint() {
        let _env = newt_core::process_env::lock();
        let directory = tempfile::tempdir().unwrap();
        let key = directory.path().join("identity.pem");
        let policy = newt_core::Caveats {
            exec: newt_core::Scope::only(["echo".to_owned()]),
            fs_write: newt_core::Scope::none(),
            net: newt_core::Scope::none(),
            ..newt_core::Caveats::top()
        };
        let permissions = crate::permissions::PermissionPromptState::default();
        let session =
            SessionCapability::establish_launch(policy.clone(), Some(&key), true, &permissions)
                .unwrap();
        let newt_core::Scope::Only(grants) = &session.caveats().exec else {
            panic!("session must retain restricted exec");
        };
        assert!(grants.contains("echo"));
        assert!(grants
            .iter()
            .any(|path| std::path::Path::new(path).is_absolute()));
        assert_eq!(session.caveats().net, policy.net);
        assert_eq!(session.caveats().fs_write, policy.fs_write);
        let signed = newt_identity::enforced_caveats(session.op.as_ref().unwrap()).unwrap();
        assert_eq!(signed.exec, session.caveats().exec);
        let unrestricted_writes = newt_core::Caveats {
            fs_write: newt_core::Scope::All,
            ..policy.clone()
        };
        let mut session = SessionCapability::establish_launch(
            unrestricted_writes.clone(),
            None,
            true,
            &permissions,
        )
        .unwrap();
        assert_eq!(session.caveats(), &unrestricted_writes);
        assert!(session.take_exec_notice().unwrap().contains("echo"));
        assert!(session.take_exec_notice().is_none());
        let missing = newt_core::Caveats {
            exec: newt_core::Scope::only(["newt-nonexistent-exec-fixture-421".into()]),
            ..policy
        };
        let mut session =
            SessionCapability::establish_launch(missing.clone(), None, true, &permissions).unwrap();
        assert_eq!(session.caveats().exec, missing.exec);
        assert!(session
            .take_exec_notice()
            .unwrap()
            .contains("newt-nonexistent-exec-fixture-421"));
        assert!(session.take_exec_notice().is_none());
    }
}

// Keep the trust calculation portable even though only macOS expands sessions.
#[cfg(any(all(test, unix), target_os = "macos"))]
fn pin_exec_policy(
    policy: &mut newt_core::Caveats,
    effective_writes: &newt_core::Scope<String>,
    path: Option<&std::ffi::OsStr>,
) -> (newt_core::exec_grants::ExecPins, Vec<String>) {
    let mut lookup = policy.clone();
    lookup.fs_write = effective_writes.clone();
    let result = newt_core::exec_grants::resolve_basenames(&mut lookup, path);
    // Recalled authority excludes paths; it does not widen the signed base.
    policy.exec = lookup.exec;
    result
}

#[cfg(all(test, unix))]
mod pin_tests {
    use super::*;
    use newt_core::{Caveats, DenialKind, Scope};
    use std::os::unix::fs::PermissionsExt;

    /// PR #2816: a recalled saved write approval must exclude its executable
    /// directory before the session captures any implicit absolute exec grant.
    #[test]
    fn saved_write_grant_excludes_exec_pin() {
        let _env = newt_core::process_env::lock();
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let bin = temp.path().canonicalize().unwrap();
        let program = bin.join("gh");
        std::fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let policy = Caveats {
            exec: Scope::only(["gh".into()]),
            fs_write: Scope::none(),
            ..Caveats::top()
        };
        let mut state = crate::permissions::PermissionPromptState::default();
        let mut trusted = policy.clone();
        let writes = state.recalled_caveats(&policy, None).unwrap().fs_write;
        assert!(
            pin_exec_policy(&mut trusted, &writes, Some(bin.as_os_str()))
                .1
                .is_empty()
        );
        assert_ne!(trusted.exec, policy.exec);
        state
            .durable_grants
            .insert((DenialKind::FsWrite, bin.to_string_lossy().into_owned()));
        let writes = state.recalled_caveats(&policy, None).unwrap().fs_write;
        assert!(newt_core::caveats::permits_path(
            &writes,
            program.to_str().unwrap()
        ));
        let mut denied = policy.clone();
        assert_eq!(
            pin_exec_policy(&mut denied, &writes, Some(bin.as_os_str())).1,
            ["gh"]
        );
        assert_eq!(denied, policy);
        #[cfg(target_os = "macos")]
        {
            use crate::disable_ocap_session_tests::EnvVar;
            let _venv = EnvVar::unset("NEWT_VENV");
            let _virtual_env = EnvVar::unset("VIRTUAL_ENV");
            let _paths = EnvVar::set("NEWT_EXEC_PATHS", bin.to_str().unwrap());
            let _path = EnvVar::set("PATH", "/usr/bin:/bin");
            let key = temp.path().join("identity.pem");
            for frozen in [false, true] {
                let mut session =
                    SessionCapability::establish_launch(policy.clone(), Some(&key), frozen, &state)
                        .unwrap();
                assert_eq!(session.caveats(), &policy);
                assert!(session.take_exec_notice().unwrap().contains("gh"));
                assert!(session.take_exec_notice().is_none());
                let signed = newt_identity::enforced_caveats(session.op.as_ref().unwrap()).unwrap();
                assert_eq!(signed.exec, policy.exec);
            }
        }
    }
}
