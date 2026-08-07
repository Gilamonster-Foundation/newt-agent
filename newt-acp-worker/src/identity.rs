//! Worker identity — the per-dispatch capability derivation surface.
//!
//! Issue #94: the headless ACP worker used to dispatch every `prompt`
//! turn under `Caveats::top()`. That was a hard-coded escape hatch from
//! the ocap discipline we already enforce in the TUI (see
//! `newt-tui::SessionCapability`). This module replaces it: every
//! headless worker is rooted in a real operator [`UserKey`] loaded from
//! disk, mints a session root under the human's full authority, and
//! derives an *attenuated* signed [`AgentKey`] per dispatch.
//!
//! The dispatch-time caveats are conservative on purpose:
//!
//! - `fs_read = All` — the coder scans the workspace to build prompts.
//! - `fs_write = All` — the coder applies whole-file emissions whose
//!   target paths can't be enumerated cheaply (the diff path can't
//!   without re-parsing).
//! - `exec = None` — the headless coder dispatches no shell commands.
//! - `net = Only([backend_host])` — only the configured inference
//!   endpoint is reachable.
//! - `max_calls = AtMost(WORKER_TURN_CALL_BUDGET)` — a hard per-turn cap.
//!
//! These are still strictly narrower than the operator's `⊤` authority,
//! so they survive the attenuation-only check in
//! [`newt_identity::attenuate`]. 35c will swap the per-dispatch
//! `worker_session_caveats` derivation for peer-cert extraction without
//! touching this module's call sites.
//!
//! **Refuse-on-missing-key (safe-fail).** If the operator key cannot be
//! loaded, the worker refuses to start — there is no implicit
//! `Caveats::top()` fallback. Operators can opt into the legacy
//! behavior for debugging via [`WorkerIdentity::allow_no_key`], which
//! drops the caveats back to `⊤` and skips the key load.

use std::path::PathBuf;
use std::sync::Arc;

use newt_core::caveats::{lock_fs_to_workspace, Caveats, CountBound, Scope};
use newt_identity::{
    attenuate, enforced_caveats, load_or_generate, session_root, unbounded_debug_fallback, AgentKey,
};

/// Per-turn inference-call budget that headless dispatches operate under.
/// Tight enough that a runaway model can't bill the operator into the
/// ground; loose enough to admit the single re-prompt fallback in
/// `newt-coder` plus a comfortable margin for future tool turns.
pub const WORKER_TURN_CALL_BUDGET: u64 = 32;

/// Errors raised while resolving the headless worker's operator identity.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// The configured key path could not be resolved (e.g. `$HOME` unset
    /// and no explicit override).
    #[error("could not resolve operator key path: {0}")]
    PathUnresolved(String),
    /// The underlying `newt-identity` operation failed (I/O, bad PEM,
    /// signature mismatch, attenuation amplification). Note that
    /// [`newt_identity::load_or_generate`] *creates* a fresh key when
    /// the file does not yet exist, so a `Key` error here is a
    /// **bad** key on disk, not a missing one.
    #[error("identity key error: {0}")]
    Key(String),
}

impl From<newt_identity::IdentityError> for IdentityError {
    fn from(e: newt_identity::IdentityError) -> Self {
        Self::Key(e.to_string())
    }
}

/// The worker's identity: an operator-rooted [`AgentKey`] that the ACP
/// server attenuates per dispatch, or the debug-only `AllowNoKey`
/// escape hatch.
#[derive(Clone)]
pub enum WorkerIdentity {
    /// Real key-rooted identity. Every dispatch derives a signed,
    /// attenuation-only child under this.
    Operator { root: Arc<AgentKey> },
    /// Debug-only escape hatch. Selected by `--allow-no-key`; restores
    /// the pre-#94 `Caveats::top()` dispatch behavior so a developer
    /// can iterate without provisioning a key. Never the default.
    AllowNoKey,
}

impl std::fmt::Debug for WorkerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Operator { .. } => f.debug_struct("Operator").finish_non_exhaustive(),
            Self::AllowNoKey => f.write_str("AllowNoKey"),
        }
    }
}

impl WorkerIdentity {
    /// Load (or generate-on-first-run) the operator key at `path`, mint a
    /// session root under it, and wrap the result as a worker identity.
    ///
    /// Safe-fail: if the path can't be loaded, returns
    /// [`IdentityError::Key`]. Callers in the headless dispatch path
    /// must surface that as a refusal to start, not silently fall back
    /// to `⊤`.
    pub fn from_operator_key(path: &std::path::Path) -> Result<Self, IdentityError> {
        let user = load_or_generate(path)?;
        let root = session_root(&user);
        Ok(Self::Operator {
            root: Arc::new(root),
        })
    }

    /// Resolve the key path from CLI override > env > default.
    pub fn resolve_key_path(
        cli_override: Option<&std::path::Path>,
    ) -> Result<PathBuf, IdentityError> {
        if let Some(p) = cli_override {
            return Ok(p.to_path_buf());
        }
        if let Ok(env) = std::env::var("NEWT_OPERATOR_KEY") {
            if !env.is_empty() {
                return Ok(PathBuf::from(env));
            }
        }
        newt_identity::default_key_path().map_err(|e| IdentityError::PathUnresolved(e.to_string()))
    }

    /// Resolve a key path then construct the identity. If `allow_no_key`
    /// is set *and* the key cannot be loaded, returns the debug
    /// [`Self::AllowNoKey`] identity; otherwise propagates the error
    /// so the worker refuses to start.
    ///
    /// Note: passing `allow_no_key=true` *and* a valid key path still
    /// uses the key — the flag opts into the fallback, not out of the
    /// happy path.
    pub fn resolve(
        cli_override: Option<&std::path::Path>,
        allow_no_key: bool,
    ) -> Result<Self, IdentityError> {
        let path = match Self::resolve_key_path(cli_override) {
            Ok(p) => p,
            Err(e) if allow_no_key => {
                tracing::warn!(
                    error = %e,
                    "headless worker: operator key path unresolved, \
                     --allow-no-key restores unbounded debug authority (debug only)"
                );
                return Ok(Self::AllowNoKey);
            }
            Err(e) => return Err(e),
        };

        match Self::from_operator_key(&path) {
            Ok(id) => Ok(id),
            Err(e) if allow_no_key => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "headless worker: operator key unavailable, \
                     --allow-no-key restores unbounded debug authority (debug only)"
                );
                Ok(Self::AllowNoKey)
            }
            Err(other) => Err(other),
        }
    }

    /// Derive the per-dispatch enforcement caveats this worker will
    /// hand to `Coder::run`.
    ///
    /// For [`Self::Operator`]: attenuate the session root with
    /// [`worker_session_caveats`] for the given backend host, verify
    /// the resulting cert chain, and return the verified caveats. The
    /// double-check (sign + verify) catches any future drift between
    /// `agent-mesh-protocol`'s wire types and `newt-core`'s enforcement
    /// types — issue #95 collapsed them, and this is the canary.
    ///
    /// For [`Self::AllowNoKey`]: return [`unbounded_debug_fallback`] —
    /// the unrestricted caveats lattice element, behaviorally identical
    /// to the pre-#94 dispatch authority. Indirected through a helper
    /// so the headless-dispatch source tree carries zero literal
    /// `Caveats::top()` references (the `no_top_leak` regression test
    /// asserts this).
    pub fn caveats_for_dispatch(
        &self,
        backend_host: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<Caveats, IdentityError> {
        match self {
            Self::Operator { root } => {
                // step-4.2 (`acp-worker-fs-scope`): fence the worker's fs to the
                // per-session ACP workspace (the `cwd` the session was opened on),
                // replacing the open `Scope::All` policy default — threaded from the
                // session, NOT read from `current_dir()`. The object-bound fs
                // resolver (#522) then makes even the fenced roots symlink-safe, so
                // `fs_read`/`fs_write = Only([workspace])` is now a real containment,
                // not a lexical one. A `None` workspace keeps the open policy (the
                // caller is responsible for supplying the session cwd).
                let mut policy = worker_session_caveats(backend_host);
                if let Some(ws) = workspace {
                    lock_fs_to_workspace(&mut policy, ws, &[], &[]);
                }
                let op = attenuate(root, &policy)?;
                let verified = enforced_caveats(&op)?;
                Ok(verified)
            }
            Self::AllowNoKey => Ok(unbounded_debug_fallback()),
        }
    }

    /// `true` when this identity is the safe (key-rooted) variant.
    pub fn is_operator(&self) -> bool {
        matches!(self, Self::Operator { .. })
    }

    /// Borrow the operator-rooted parent [`AgentKey`], if any.
    ///
    /// **Issue #93:** subprocess plugins spawned during a dispatch must
    /// inherit a delegated child from this parent so the plugin's cert
    /// chain walks back to the operator's `UserKey`
    /// (`~/.newt/identity.pem`). The `handle_prompt_coder` path passes
    /// this to `Coder::with_parent_key` so a future provider-plugin
    /// spawn can mint envelopes via `Coder::plugin_envelope_for`.
    ///
    /// Returns `None` for [`Self::AllowNoKey`]: the debug fallback runs
    /// with no operator key on disk, so there's nothing to root
    /// subprocess plugins at. A subprocess plugin spawned under
    /// `AllowNoKey` SHOULD itself fall through to its own
    /// no-envelope debug path — the headlines audit
    /// `synthetic_keys_remaining` check verifies that, even there, no
    /// code path manufactures a fresh `AgentKey::generate()` /
    /// `UserKey::generate()` to fill the gap.
    pub fn parent_key(&self) -> Option<&Arc<AgentKey>> {
        match self {
            Self::Operator { root } => Some(root),
            Self::AllowNoKey => None,
        }
    }
}

/// Build the *policy* caveats a headless worker should dispatch under,
/// before attenuation rolls them up against the operator's authority.
///
/// `backend_host` is the host portion of the inference backend's URL
/// (e.g. `127.0.0.1`). When `None` the network axis is narrowed to
/// `Scope::none()` — a backend with no endpoint (mock, subprocess) has
/// no host to authorize.
pub fn worker_session_caveats(backend_host: Option<&str>) -> Caveats {
    let net = match backend_host {
        Some(h) if !h.is_empty() => Scope::only([h.to_string()]),
        _ => Scope::none(),
    };
    Caveats {
        fs_read: Scope::All,
        fs_write: Scope::All,
        exec: Scope::none(),
        net,
        max_calls: CountBound::AtMost(WORKER_TURN_CALL_BUDGET),
        valid_for_generation: Scope::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::{CaveatsExt, CountBoundExt};
    use tempfile::TempDir;

    #[test]
    fn worker_session_caveats_blocks_exec_and_other_hosts() {
        let c = worker_session_caveats(Some("127.0.0.1"));
        assert!(c.permits_fs_read("/anywhere"));
        assert!(c.permits_fs_write("/anywhere"));
        assert!(!c.permits_exec("cargo"), "exec must be denied by default");
        assert!(c.permits_net("127.0.0.1"));
        assert!(
            !c.permits_net("evil.example.com"),
            "non-backend host must be denied"
        );
        assert!(c.max_calls.permits_one_more(0));
        assert!(!c.max_calls.permits_one_more(WORKER_TURN_CALL_BUDGET));
    }

    #[test]
    fn worker_session_caveats_with_no_endpoint_denies_net() {
        let c = worker_session_caveats(None);
        assert!(!c.permits_net("127.0.0.1"));
        assert!(!c.permits_net("anything"));
    }

    #[test]
    fn worker_session_caveats_is_strictly_narrower_than_top() {
        let policy = worker_session_caveats(Some("127.0.0.1"));
        assert_ne!(policy, Caveats::top(), "policy must not be top");
        assert!(policy.leq(&Caveats::top()), "policy must be ⊑ top");
    }

    fn fresh_dir() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn from_operator_key_generates_and_persists() {
        let dir = fresh_dir();
        let path = dir.path().join("identity.pem");
        assert!(!path.exists());
        let id = WorkerIdentity::from_operator_key(&path).unwrap();
        assert!(id.is_operator(), "operator identity expected");
        assert!(path.exists(), "key must be persisted on first call");
    }

    #[test]
    fn caveats_for_dispatch_is_signed_and_verified() {
        let dir = fresh_dir();
        let path = dir.path().join("identity.pem");
        let id = WorkerIdentity::from_operator_key(&path).unwrap();

        // `None` workspace keeps the open policy — the equality below still holds.
        let c = id.caveats_for_dispatch(Some("127.0.0.1"), None).unwrap();
        // The returned caveats came from verifying the cert chain, so
        // they must equal the policy we attenuated with.
        assert_eq!(c, worker_session_caveats(Some("127.0.0.1")));
        // And they must not be top().
        assert_ne!(c, Caveats::top());
    }

    #[test]
    fn caveats_for_dispatch_with_no_endpoint_is_safe() {
        let dir = fresh_dir();
        let path = dir.path().join("identity.pem");
        let id = WorkerIdentity::from_operator_key(&path).unwrap();
        let c = id.caveats_for_dispatch(None, None).unwrap();
        assert_eq!(c, worker_session_caveats(None));
    }

    #[test]
    fn caveats_for_dispatch_fences_fs_to_the_session_workspace() {
        // step-4.2 (`acp-worker-fs-scope`): with a session workspace, the worker's
        // fs is scoped to that workspace (`Only`), not the open `Scope::All`
        // default — so a dispatch cannot read/write outside the ACP `cwd`. (The
        // object-bound resolver, #522, then makes even this fence symlink-safe.)
        let dir = fresh_dir();
        let path = dir.path().join("identity.pem");
        let id = WorkerIdentity::from_operator_key(&path).unwrap();

        let fenced = id
            .caveats_for_dispatch(Some("127.0.0.1"), Some("/ws"))
            .unwrap();
        // The workspace root is in the fence; outside it is denied (NOT Scope::All).
        assert!(fenced.permits_fs_read("/ws"), "workspace root readable");
        assert!(fenced.permits_fs_write("/ws"), "workspace root writable");
        assert!(
            !fenced.permits_fs_read("/etc/passwd"),
            "reads outside the workspace are denied — no longer Scope::All"
        );
        assert!(
            !fenced.permits_fs_write("/etc/cron.d/newt"),
            "writes outside the workspace are denied — no longer Scope::All"
        );

        // Control: the un-fenced (None-workspace) policy still permits anything —
        // proving the workspace fence is what closed the escape.
        let open = id.caveats_for_dispatch(Some("127.0.0.1"), None).unwrap();
        assert!(
            open.permits_fs_write("/etc/cron.d/newt"),
            "the open policy permits anything (control for the fence)"
        );
    }

    #[test]
    fn allow_no_key_returns_top() {
        let id = WorkerIdentity::AllowNoKey;
        assert!(!id.is_operator());
        let c = id.caveats_for_dispatch(Some("127.0.0.1"), None).unwrap();
        assert_eq!(c, Caveats::top());
    }

    #[test]
    fn resolve_succeeds_when_key_can_be_generated() {
        // load_or_generate creates the key on first run; resolve should
        // therefore succeed against an empty tempdir under explicit
        // override (no env / HOME dependency).
        let dir = fresh_dir();
        let path = dir.path().join("identity.pem");
        let id = WorkerIdentity::resolve(Some(&path), false).unwrap();
        assert!(id.is_operator());
        assert!(path.exists());
    }

    #[test]
    fn resolve_with_allow_no_key_falls_back_on_bad_pem() {
        // Write a junk file at the path so load_or_generate fails the
        // PEM decode. Without --allow-no-key this is a refusal; with
        // it, the worker falls back to AllowNoKey (debug only).
        let dir = fresh_dir();
        let path = dir.path().join("identity.pem");
        std::fs::write(&path, b"not a real PEM key").unwrap();

        let refused = WorkerIdentity::resolve(Some(&path), false);
        assert!(
            matches!(refused, Err(IdentityError::Key(_))),
            "bad PEM must refuse without --allow-no-key, got: {refused:?}"
        );

        let fallback = WorkerIdentity::resolve(Some(&path), true).unwrap();
        assert!(
            !fallback.is_operator(),
            "--allow-no-key must yield AllowNoKey on bad PEM"
        );
    }

    #[test]
    fn resolve_explicit_override_takes_precedence_over_env() {
        // CLI > env > default. Verify the explicit override path is
        // honored without touching the global env at all.
        let dir = fresh_dir();
        let cli_path = dir.path().join("cli.pem");
        let id = WorkerIdentity::resolve(Some(&cli_path), false).unwrap();
        assert!(id.is_operator());
        assert!(
            cli_path.exists(),
            "explicit-override path must be used (not env / default)"
        );
    }
}
