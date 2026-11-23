//! The confined git steps of the staging broker: the copy of the approved
//! commit into staging (phase 3) and the fetch broker's two copies. They are
//! the broker's only code that runs git under `ConstrainedExecutor` or holds
//! agent-bridle's `HeldReadRoot`, so they sit in one child module that a
//! single `cfg` can gate (#3160).

use std::path::Path;

use agent_mesh_protocol::caveats::Scope;
use newt_policy::caveats::Caveats;

use super::{HeldRoots, Refusal, StagingRepo, TrustedTools};

impl HeldRoots {
    /// The held handles, duplicated, for a fence that anchors on descriptors
    /// (Linux Landlock): the SAME file or directory objects [`Self::verify_identities`]
    /// compared, so nothing after that check can re-point the fence by path.
    /// `HeldReadRoot::bind` re-checks the pairing itself (`fstat` vs `stat`),
    /// so a root swapped between `verify_identities` and here is still caught
    /// even if that earlier call were ever skipped.
    ///
    /// # Errors
    /// When a handle cannot be duplicated, or its identity no longer matches
    /// `root` (defence in depth with [`Self::verify_identities`]).
    #[cfg(target_os = "linux")]
    fn held_read_roots(&self) -> Result<Vec<agent_bridle::HeldReadRoot>, Refusal> {
        self.canonical
            .iter()
            .zip(&self.handles)
            .map(|(root, handle)| {
                let fd = handle.as_fd().try_clone_to_owned().map_err(|e| {
                    format!(
                        "refused: cannot duplicate the handle for read root '{}' ({e})",
                        root.display()
                    )
                })?;
                agent_bridle::HeldReadRoot::bind(root.to_string_lossy().into_owned(), fd).map_err(
                    |e| {
                        format!(
                            "refused: held read root '{}' failed its identity check ({e})",
                            root.display()
                        )
                        .into()
                    },
                )
            })
            .collect()
    }
}

/// Run the checked git under the existing push-copy policy: `fs_read`/`fs_write`
/// as given, with the platform network limitation documented below. It is
/// minted under the `AgentInfluenced` KERNEL floor, and the sandbox kind the
/// executor ACTUALLY applied is checked afterwards: an advisory
/// (`SandboxKind::None`) run refuses even if it succeeded.
///
/// The exec axis stays unrestricted, as for every other `AgentInfluenced`
/// spawn (`workspace_confined_caveats`): Landlock delivers exec only at the
/// interceptor level, so a Kernel floor refuses any restricted exec scope.
/// The program is the checked git; what its children (`git-upload-pack`,
/// `rev-list`, `/bin/sh`) can reach is bounded by the kernel-enforced fs and
/// net axes, which are the fence F4 depends on.
///
/// `held` are read roots (also spelled in `fs_read`) whose fence rules anchor
/// on the caller's descriptors rather than a re-opened path.
///
/// # Errors
/// When the executor refuses, or the applied sandbox was not a kernel fence.
pub fn run_confined_git(
    tools: &TrustedTools,
    args: &[&str],
    cwd: &Path,
    session: &Caveats,
    fs_read: Scope<String>,
    fs_write: Scope<String>,
    held: Vec<agent_bridle::HeldReadRoot>,
) -> Result<crate::confined_exec::ConfinedOutput, String> {
    execute_confined_git(confined_git_request(
        tools, args, cwd, session, fs_read, fs_write, held,
    ))
}

fn confined_git_request(
    tools: &TrustedTools,
    args: &[&str],
    cwd: &Path,
    session: &Caveats,
    fs_read: Scope<String>,
    fs_write: Scope<String>,
    held: Vec<agent_bridle::HeldReadRoot>,
) -> crate::confined_exec::ExecRequest {
    // The historical push-copy policy, preserved here. Fetch copies additionally
    // require NetGrant::DenyAll through fetch_copy_request; they must never rely
    // on this local-source assumption.
    // macOS Seatbelt is advisory for both net AND exec in this path: the
    // kernel profile it receives does not prevent the confined copy from
    // spawning arbitrary children or opening network sockets.  Net is safe
    // here because the confined fetch reads only a LOCAL workspace — no
    // network is used — so Scope::All for net causes no real exposure.
    // Exec is a different story: Seatbelt does NOT kernel-enforce a restricted
    // exec scope for the copy (it is advisory, unlike Landlock on Linux).
    // TODO(#2661): bind the exec axis to the trusted git binary + exec-path
    // via a Landlock EXECUTE rule or an equivalent macOS mechanism; until
    // that issue lands, the confined copy on macOS has unrestricted exec.
    // Linux keeps net: none() (enforced by Landlock + seccomp via net-guard).
    // macOS net=Scope::All follow-up: TODO(#2662) — enforce net:none on macOS.
    // See docs/security/ocap-deviations.md §mach-xpc-ambient-deputy.
    #[cfg(target_os = "macos")]
    let net_scope = Scope::All;
    #[cfg(not(target_os = "macos"))]
    let net_scope = Scope::none();
    let caveats = Caveats {
        fs_read,
        fs_write,
        exec: Scope::All,
        net: net_scope,
        ..session.clone()
    };
    crate::confined_exec::ExecRequest::new(
        crate::confined_exec::ExecOrigin::AgentInfluenced,
        tools.git.to_string_lossy().into_owned(),
        args.iter().copied(),
        cwd.to_path_buf(),
        caveats,
    )
    .envs(tools.env.iter().cloned())
    .held_read_roots(held)
}

fn execute_confined_git(
    req: crate::confined_exec::ExecRequest,
) -> Result<crate::confined_exec::ConfinedOutput, String> {
    let out = crate::confined_exec::ConstrainedExecutor::run(&req)
        .map_err(|e| format!("refused: confined git could not run — {e}"))?;
    require_kernel_fence(out.sandbox_kind)?;
    Ok(out)
}

/// The sandbox the executor REPORTS it applied must be the platform's kernel
/// fence (Landlock on Linux, Seatbelt on macOS).  An advisory run or the wrong
/// kind refuses regardless of exit status.
fn require_kernel_fence(kind: crate::sandbox_seam::SandboxKind) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let expected = crate::sandbox_seam::SandboxKind::Landlock;
    #[cfg(target_os = "macos")]
    let expected = crate::sandbox_seam::SandboxKind::Seatbelt;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let expected = crate::sandbox_seam::SandboxKind::None; // always refuses below

    if kind == expected && kind != crate::sandbox_seam::SandboxKind::None {
        Ok(())
    } else {
        Err(format!(
            "refused: the confined git step was not kernel-enforced \
             (got {kind:?}, expected {expected:?})"
        ))
    }
}

/// F4: copy the approved commit's reachable objects from the workspace
/// REPOSITORY (`source_repo`, the common dir — never an objects directory)
/// into staging with a confined `git fetch`: read = the held read roots
/// plus staging, write = staging only.
///
/// The held roots' identities are verified against what their paths name
/// NOW ([`HeldRoots::verify_identities`]); on Linux the fence is then built
/// FROM the held descriptors ([`agent_bridle::HeldReadRoot`]), so a root
/// swapped at its pathname after that check is outside the fence and the
/// copy refuses. On macOS the Seatbelt profile admits paths only, so that
/// window stays open there (residual #8, `docs/security/ocap-deviations.md`).
///
/// # Errors
/// When a held root no longer matches its path, or the confined fetch
/// cannot run, is not kernel-enforced, or fails.
pub fn confined_fetch(
    tools: &TrustedTools,
    staging: &StagingRepo,
    source_repo: &Path,
    oid: &str,
    session: &Caveats,
    held: &HeldRoots,
) -> Result<(), Refusal> {
    confined_fetch_with(tools, staging, source_repo, oid, session, held, || {})
}

/// [`confined_fetch`] with `after_verify` run between the identity check and
/// the fence build — the test seam for that window. Production passes a no-op.
pub(crate) fn confined_fetch_with(
    tools: &TrustedTools,
    staging: &StagingRepo,
    source_repo: &Path,
    oid: &str,
    session: &Caveats,
    held: &HeldRoots,
    after_verify: impl FnOnce(),
) -> Result<(), Refusal> {
    let staging_dir = staging.path().to_string_lossy().into_owned();
    let source = source_repo.to_string_lossy();
    let request = confined_beside_staging(
        tools,
        staging,
        &[
            "-C",
            &staging_dir,
            "fetch",
            "--no-tags",
            "--no-write-fetch-head",
            &source,
            oid,
        ],
        Scope::only([staging_dir.clone()]),
        session,
        held,
        after_verify,
    )?;
    let out = execute_confined_git(request)?;
    if out.success {
        Ok(())
    } else {
        Err("refused: the confined copy of the approved commit failed"
            .to_string()
            .into())
    }
}

/// One confined git that may read the held roots plus staging and write only
/// `fs_write`: the fence both directions of the broker share. A push copies
/// workspace objects INTO staging ([`confined_fetch`]); a fetch seeds staging
/// the same way and then copies the fetched refs back OUT ([`confined_git`]).
fn confined_beside_staging(
    tools: &TrustedTools,
    staging: &StagingRepo,
    args: &[&str],
    fs_write: Scope<String>,
    session: &Caveats,
    held: &HeldRoots,
    after_verify: impl FnOnce(),
) -> Result<crate::confined_exec::ExecRequest, Refusal> {
    let staging_dir = staging.path().to_string_lossy().into_owned();
    held.verify_identities()?;
    #[cfg(target_os = "linux")]
    let held_fds = held.held_read_roots()?;
    #[cfg(not(target_os = "linux"))]
    let held_fds = Vec::new();
    after_verify();
    let fs_read = match held.scope() {
        Scope::All => Scope::All,
        Scope::Only(_) => Scope::only(
            held.canonical()
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .chain([staging_dir]),
        ),
    };
    Ok(confined_git_request(
        tools,
        args,
        staging.path(),
        session,
        fs_read,
        fs_write,
        held_fds,
    ))
}

/// A confined git for the fetch broker: reads the held roots plus staging,
/// writes only `fs_write`, and requires the executor's deny-all network floor.
/// macOS admits audited, unprivileged Seatbelt launches with a kernel network
/// witness; absent that witness, the executor refuses before the copy spawns.
/// Repository transport configuration cannot relax the floor.
///
/// # Errors
/// As [`confined_fetch`].
pub fn confined_git(
    tools: &TrustedTools,
    staging: &StagingRepo,
    args: &[&str],
    fs_write: Scope<String>,
    session: &Caveats,
    held: &HeldRoots,
) -> Result<crate::confined_exec::ConfinedOutput, Refusal> {
    let request = confined_beside_staging(tools, staging, args, fs_write, session, held, || {})?;
    Ok(execute_confined_git(fetch_copy_request(request))?)
}

/// The fetch copy's network floor, independent of the push-copy policy.
pub(crate) fn fetch_copy_request(
    request: crate::confined_exec::ExecRequest,
) -> crate::confined_exec::ExecRequest {
    request
        .net_grant(crate::confined_exec::NetGrant::DenyAll)
        // Reuse the shell's repository-config hardening (hooks, fsmonitor,
        // SSH, ext transport, etc.). File-only transport also defeats a
        // repository's protocol.*.allow overrides after URL rewriting.
        .envs(crate::git_hardening::sandbox_git_env(
            ("", ""),
            &Default::default(),
        ))
        .env("GIT_ALLOW_PROTOCOL", "file")
}

#[cfg(all(test, unix))]
#[path = "confined/tests.rs"]
mod tests;
