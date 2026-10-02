//! OS-level sandbox plumbing.
//!
//! The L3 boundary is the only layer that can confine a *permitted external
//! program's own syscalls* once it has spawned — what neither the static
//! decomposition (L1) nor the in-process interceptor (L2) can see. It is
//! OS-specific, so each operating system gets its own backend behind one
//! [`Sandbox`] trait, selected in code by [`best_available_sandbox`] (one
//! `cfg(target_os, feature)` arm per backend, with a runtime capability probe),
//! never overclaiming: a build either compiles a real backend for its host or
//! falls back to the advisory [`NoopSandbox`] reporting [`SandboxKind::None`]
//! (DESIGN §6, ADR 0001 L3, **ADR 0006** per-OS backends, **ADR 0009** the
//! cross-platform strategy).
//!
//! - **Linux** — [`LandlockSandbox`] (`linux-landlock`): a real Landlock ruleset
//!   confining the `fs_write` axis, and `fs_read` when restricted. `restrict_self`
//!   confines the calling thread (inherited across `fork`/`execve`). Direct
//!   execute rules narrow `execve` but do not close the loader trampoline, so
//!   `exec` remains honestly `Interceptor`; ABI-v4 kernels can kernel-deny all
//!   TCP egress for an empty `net` scope.
//! - **macOS** — [`SeatbeltSandbox`] (`macos-seatbelt`): an SBPL profile derived
//!   from the effective [`Caveats`], applied by wrapping the spawned program in
//!   `sandbox-exec(1)` (no FFI — core forbids `unsafe`). Confines both filesystem
//!   axes, restricted `exec`, and empty or loopback-only `net` scopes. General
//!   remote-host allowlists use the separately fenced proxy path and remain
//!   conservatively reported at their userspace strength.
//! - **Windows** — [`SandboxKind::AppContainer`] (`windows-appcontainer`): a
//!   process-creation wrapper applies filesystem DACLs, deny-all or loopback-only
//!   network policy, and the kernel child-process block for `exec: Only([])`.
//!   Non-empty exec allowlists cannot be expressed without WDAC and stay
//!   `Interceptor`.
//!
//! A backend confines either by restricting the calling thread in [`Sandbox::apply`]
//! (Landlock) **or** by wrapping the spawned command via
//! [`Sandbox::command_prefix`] (Seatbelt/AppContainer); a spawn site honors both,
//! so the mechanism is uniform at the call site.

use crate::{Caveats, SandboxPolicy, ToolError, ToolResult};
use std::sync::Arc;

/// A read root the caller already HOLDS as a directory descriptor, for a
/// backend that can anchor its rule on the descriptor itself (Linux Landlock:
/// `PathBeneath` takes any `AsFd`). The fence then covers the object the
/// caller verified — not whatever `provenance` names by the time the rule is
/// added — so a swap at the pathname between the caller's check and the
/// ruleset cannot re-point it. `provenance` is how the grant spells the same
/// root: it is matched to drop that root's path-opened rule, never opened.
/// Reads are ambient under `fs_read: All`, so held roots add nothing there.
#[derive(Debug, Clone)]
pub struct HeldReadRoot {
    provenance: String,
    #[cfg(unix)]
    fd: Arc<std::os::fd::OwnedFd>,
}

impl HeldReadRoot {
    /// Bind `fd` — a directory descriptor the caller keeps, or a dup of one
    /// (same object) — to `provenance`, the root the grant spells it as. This
    /// is the ONLY constructor: fields are private and there is no public
    /// struct literal, and this one REFUSES unless `fd`'s `(dev, ino)` equals
    /// what `provenance` names right now (`fstat(fd)` vs `stat(path)` — the
    /// path is never reopened to do this). A caller cannot install a
    /// (label, descriptor) pair admission never saw as matching: neither an
    /// honestly-paired but un-admitted root (that is `apply_with_held_roots`'s
    /// job — #2674 P1) nor a falsely labelled one (a mismatched pair is
    /// refused right here, before any `HeldReadRoot` for it can exist).
    ///
    /// # Errors
    /// [`ToolError::Denied`] when `fd` and `provenance` do not name the same
    /// object, or either cannot be inspected.
    #[cfg(unix)]
    pub fn bind(
        provenance: impl Into<String>,
        fd: std::os::fd::OwnedFd,
    ) -> crate::ToolResult<Self> {
        use std::os::unix::fs::MetadataExt;
        let provenance = provenance.into();
        // `File::from`/`OwnedFd::from` are plain ownership retyping (no
        // syscall) — `file` is the SAME object `fd` was, just typed so
        // `.metadata()` can `fstat` it.
        let file = std::fs::File::from(fd);
        let held = file.metadata().map_err(crate::ToolError::from)?;
        let named = std::fs::metadata(&provenance).map_err(crate::ToolError::from)?;
        if (held.dev(), held.ino()) != (named.dev(), named.ino()) {
            return Err(crate::ToolError::denied(format!(
                "held descriptor does not name '{provenance}'"
            )));
        }
        Ok(Self {
            provenance,
            fd: Arc::new(std::os::fd::OwnedFd::from(file)),
        })
    }

    /// The pathname the grant spells this root as (provenance only).
    #[must_use]
    pub fn provenance(&self) -> &str {
        &self.provenance
    }

    /// The held directory descriptor the rule anchors on.
    #[cfg(unix)]
    #[must_use]
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.fd.as_fd()
    }
}

/// Which OS-level sandbox actually backs an authorization.
///
/// Recorded in every [`crate::ToolContext`] and surfaced in every result
/// envelope so callers can tell whether the leash is kernel-enforced or merely
/// advisory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKind {
    /// A real Landlock ruleset is active (Linux). Kernel-enforced.
    Landlock,
    /// A real Seatbelt (`sandbox-exec` SBPL) profile is active (macOS).
    /// Kernel-enforced against the spawned program's interior.
    Seatbelt,
    /// A real AppContainer token is active (Windows). Kernel-enforced.
    AppContainer,
    /// A Linux **minimal-rootfs mount-namespace jail** is active (ADR 0013 D3/D4,
    /// agent-bridle#109/#108). The process runs in a `pivot_root` jail that
    /// physically contains only the granted program files, so `exec` is
    /// kernel-confined by **identity** — no un-granted binary *exists* to run or to
    /// `ld.so`-trampoline into (ADR 0011 D7's precondition is now physically true,
    /// not asserted) — and the filesystem axes are kernel-confined by the
    /// read-only/read-write bind-mounts. Network is not namespaced at this tier, so
    /// `net` stays advisory (never overclaimed). Reserved for the minimal-rootfs
    /// mode: a Landlock-only boundary run stays [`SandboxKind::Landlock`] (its exec
    /// axis is held — ADR 0011).
    MinimalRootfs,
    /// A Linux **Tier-2 micro-VM** is active (ADR 0013 D3, ADR 0009 D2,
    /// agent-bridle#111): the same minimal rootfs booted as a qemu guest under a
    /// separate kernel. Identity is closed as in [`SandboxKind::MinimalRootfs`]
    /// (only the granted program exists in the guest) and the filesystem is confined
    /// by the guest boundary; with no guest network device, egress is impossible —
    /// so `exec`, the fs axes, **and** `net` are all kernel-confined, and a
    /// guest-kernel compromise is still contained. The strongest tier.
    MicroVm,
    /// No OS-level sandbox — the leash is in-process/advisory only. This is the
    /// honest default on a host with no compiled-and-capable backend.
    #[default]
    None,
}

/// An OS-level confinement that can be applied from a set of [`Caveats`].
///
/// Implementations translate the lattice's `fs_read`/`fs_write`/`exec`/`net`
/// axes into the kernel rules their native backend can honestly express.
pub trait Sandbox: Send + Sync {
    /// The kind of confinement this sandbox provides.
    fn kind(&self) -> SandboxKind;

    /// The executable-identity proof domain actually installed by this backend.
    fn exec_boundary(&self) -> crate::ExecBoundary {
        crate::ExecBoundary::ProcessTree
    }

    /// Apply the confinement for the given effective caveats. Called by a tool
    /// *before* it does any privileged work, on the thread/process that will do
    /// it. A `Noop` implementation succeeds without restricting anything.
    ///
    /// This is the confinement mechanism for *thread-confining* backends
    /// (Landlock's `restrict_self`). *Wrapper-based* backends (macOS Seatbelt)
    /// confine via [`Sandbox::command_prefix`] instead and make this a no-op.
    fn apply(&self, effective: &Caveats) -> ToolResult<()>;

    /// [`Sandbox::apply`] with read roots the caller holds as directory
    /// descriptors ([`HeldReadRoot`]). A backend that can anchor a rule on a
    /// descriptor (Landlock) adds `PathBeneath` on each one and never re-opens
    /// that root by path. Every other backend REFUSES a non-empty `held`
    /// rather than fall back to the pathname (fail-closed); with none held
    /// this is [`Sandbox::apply`].
    fn apply_with_held_roots(&self, effective: &Caveats, held: &[HeldReadRoot]) -> ToolResult<()> {
        if held.is_empty() {
            return self.apply(effective);
        }
        Err(ToolError::denied(format!(
            "the {:?} sandbox cannot anchor a read root on a held descriptor",
            self.kind()
        )))
    }

    /// The argv prefix that wraps a child so a *wrapper-based* L3 backend
    /// confines it (macOS `sandbox-exec`). The returned vector, prepended to a
    /// `(program, args…)`, is the argv that must actually be spawned.
    ///
    /// Backends that confine the spawning thread in [`Sandbox::apply`]
    /// (Landlock) or that do not confine ([`NoopSandbox`]) return an **empty**
    /// prefix. A spawn site applies *both* `apply()` and this prefix, so either
    /// mechanism is honored without the caller knowing which backend is active.
    ///
    /// **Fail-closed:** a backend that is selected but cannot build its wrapper
    /// (e.g. the wrapper binary is missing) returns `Err` — never an empty
    /// (silently unconfined) prefix. The default is the empty prefix.
    fn command_prefix(&self, effective: &Caveats) -> ToolResult<Vec<String>> {
        let _ = effective;
        Ok(Vec::new())
    }

    /// A **conservative upper bound** on the authority this backend/mechanism
    /// stack can actually deliver to a *hostile* child, per axis — NOT a
    /// projection of the rules we intend to install (I15 / INV-BOUND, the grain
    /// corollary). A known mechanism bypass — `io_uring` egress under `net:none`,
    /// an ambient Mach network deputy, an executable process image outside the
    /// exec grant, a symlinked grant root, a DACL that necessarily confers read
    /// on a write grant — MUST be reflected here as [`ResolvedScope::Unknown`] (or
    /// a `Superset`/`Unbounded` scope) on the affected axis, so mesh admission
    /// (`resolved ⊑ delegated ∪ closure`) fails closed. This is the operand the
    /// spawn-path scope check consumes; it is never `ResolvedAuthority::from_delegated`
    /// (which merely lifts the caveats verbatim and re-asserts the fidelity the
    /// audit disputed).
    ///
    /// **Fail-closed default:** every axis is `Unknown` (honest ignorance ⇒
    /// refuse). A backend that has not yet implemented a faithful bound therefore
    /// refuses any restricted grant rather than silently admitting it — the
    /// conservative rule applied to the trait itself. Each concrete backend
    /// overrides this with the authority it can actually be shown to enforce.
    ///
    /// `stdio` is the spawn's declared [`crate::StdioPosture`] — whether this
    /// specific spawn's stdin/stdout/stderr are each a pipe/`/dev/null`
    /// (`Audited`) or something this builder cannot vouch for (`Unaudited`,
    /// the default). Only [`SeatbeltSandbox`] consults it today (the Mach
    /// deputy audit's own precondition, agent-bridle#416 round 3: an L3 scope
    /// bound is the admission gate under a non-Kernel floor, so it must share
    /// the same stdio precondition `seatbelt_net_kernel_witness` already
    /// requires at L4 — an unaudited restricted launch stays `Unknown`, never
    /// a named bound); every other backend ignores it.
    fn resolved_authority(
        &self,
        effective: &Caveats,
        stdio: crate::StdioPosture,
    ) -> crate::ResolvedAuthority {
        let _ = (effective, stdio);
        crate::ResolvedAuthority {
            fs_read: crate::ResolvedScope::Unknown,
            fs_write: crate::ResolvedScope::Unknown,
            exec: crate::ResolvedScope::Unknown,
            net: crate::ResolvedScope::Unknown,
        }
    }

    /// The explicit, minimal, **harness-disjoint** runtime closure this backend
    /// legitimately adds beyond the delegated grant — the `closure` operand of
    /// mesh admission (`resolved ⊑ delegated ∪ closure`). It may declare system
    /// runtime substrate (the loader, library/system-data read base, the resolved
    /// image of a granted program, device sinks) but MUST be disjoint from
    /// harness-private authority (secrets, keys, control sockets, the authority/
    /// provenance store) — see [`crate::admitted::closure_is_harness_disjoint`],
    /// the one canonical guard `AdmittedFence::admit` applies. The default
    /// declares nothing (`empty_closure`); each backend overrides with the
    /// substrate it actually adds, so a *legitimate* grant admits as `Subset`
    /// while an undeclared widening still refuses.
    fn runtime_closure(&self, effective: &Caveats) -> crate::ResolvedAuthority {
        let _ = effective;
        crate::empty_closure()
    }
}

/// The no-backend sandbox: applies nothing and reports [`SandboxKind::None`].
///
/// This is the honest fallback when no compiled native backend is capable or
/// when the effective caveats engage no axis that the available backend governs.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSandbox;

impl Sandbox for NoopSandbox {
    fn kind(&self) -> SandboxKind {
        SandboxKind::None
    }

    fn apply(&self, _effective: &Caveats) -> ToolResult<()> {
        // Intentionally a no-op: the advisory default. Real kernel enforcement
        // lives in `LandlockSandbox` (Linux + `linux-landlock`).
        Ok(())
    }

    fn resolved_authority(
        &self,
        _effective: &Caveats,
        _stdio: crate::StdioPosture,
    ) -> crate::ResolvedAuthority {
        // Noop confines nothing, so it can deliver EVERYTHING on every axis:
        // the conservative upper bound is `Unbounded`. Any restricted (`Only(_)`)
        // grant is then a `Superset` of what was delegated ⇒ admission refuses —
        // a Noop backend can never satisfy a CONFINED contract.
        crate::ResolvedAuthority {
            fs_read: crate::ResolvedScope::Unbounded,
            fs_write: crate::ResolvedScope::Unbounded,
            exec: crate::ResolvedScope::Unbounded,
            net: crate::ResolvedScope::Unbounded,
        }
    }
}

#[cfg(test)]
mod resolved_authority_foundation_tests {
    //! PR-0 foundation: the conservative-upper-bound contract at the trait level
    //! (I15 / INV-BOUND). Per-backend faithful bounds land in follow-up slices;
    //! here we pin that the *defaults* fail closed, so no backend can silently
    //! admit a restricted grant it has not been shown to enforce.
    use super::{NoopSandbox, Sandbox, SandboxKind};
    use crate::{
        admit, empty_closure, AdmissionDecision, Caveats, ResolvedScope, Scope, ToolResult,
    };

    fn exec_only(program: &str) -> Caveats {
        Caveats {
            exec: Scope::only([program.to_string()]),
            ..Caveats::top()
        }
    }

    #[test]
    fn unimplemented_backend_default_is_unknown_and_fails_closed() {
        // A Sandbox that does NOT override resolved_authority inherits the
        // all-Unknown default, so admission refuses any restricted grant.
        struct Bare;
        impl Sandbox for Bare {
            fn kind(&self) -> SandboxKind {
                SandboxKind::None
            }
            fn apply(&self, _e: &Caveats) -> ToolResult<()> {
                Ok(())
            }
        }
        let effective = exec_only("git");
        let resolved = Bare.resolved_authority(&effective, crate::StdioPosture::Unaudited);
        assert_eq!(resolved.exec, ResolvedScope::Unknown);
        assert!(matches!(
            admit(&resolved, &effective, &empty_closure()),
            AdmissionDecision::Reject(_)
        ));
    }

    #[test]
    fn noop_backend_is_unbounded_and_refuses_restricted_grants() {
        let effective = exec_only("git");
        let resolved = NoopSandbox.resolved_authority(&effective, crate::StdioPosture::Unaudited);
        assert_eq!(resolved.exec, ResolvedScope::Unbounded);
        assert!(matches!(
            admit(&resolved, &effective, &empty_closure()),
            AdmissionDecision::Reject(_)
        ));
    }

    #[test]
    fn an_unrestricted_grant_admits_even_against_an_unbounded_backend() {
        // top() is All on every axis; nothing is restricted, so an Unbounded
        // resolved authority is Subset/Equal of the (unbounded) delegated bound
        // ⇒ admit. The conservative rule only bites RESTRICTED axes.
        let effective = Caveats::top();
        let resolved = NoopSandbox.resolved_authority(&effective, crate::StdioPosture::Unaudited);
        assert!(matches!(
            admit(&resolved, &effective, &empty_closure()),
            AdmissionDecision::Admit
        ));
    }
}

/// `true` if either filesystem axis is actually restricted (`Only(_)`) — the
/// condition under which the fs-confining backends (Landlock, Seatbelt) have
/// something to enforce. When **no** fs axis is restricted, an fs-only backend
/// governs nothing, so honest reporting downgrades the [`SandboxKind`] to
/// [`SandboxKind::None`] rather than overclaiming a boundary that confines
/// nothing (I9 / ADR 0006 D3). Used by every spawn site that reports a kind.
#[must_use]
pub(crate) fn restricts_fs(caveats: &Caveats) -> bool {
    matches!(caveats.fs_write, crate::Scope::Only(_))
        || matches!(caveats.fs_read, crate::Scope::Only(_))
}

/// `true` if the `exec` axis is actually restricted (`Only(_)`). Seatbelt acts on
/// every such scope via `process-exec*`, including the spawned program's
/// interior execs (ADR 0014), so `exec: Only(_)` engages it by itself.
/// AppContainer separately handles the deny-all subset via
/// [`exec_fully_denied`]. Landlock narrows direct `execve` when another governed
/// axis engages it, but its loader-trampoline residual keeps the reported exec
/// strength at `Interceptor`, so exec restriction alone does not engage it.
#[must_use]
pub(crate) fn restricts_exec(caveats: &Caveats) -> bool {
    matches!(caveats.exec, crate::Scope::Only(_))
}

/// `true` if the `net` axis is restricted to the **empty** set — i.e. *all*
/// network egress is denied. Seatbelt and AppContainer enforce this scope, and a
/// Landlock ABI-v4 kernel can deny all TCP egress. A general non-empty hostname
/// allowlist is not directly expressible by those native rules and follows the
/// separately documented proxy/advisory path.
#[must_use]
pub(crate) fn net_fully_denied(caveats: &Caveats) -> bool {
    matches!(&caveats.net, crate::Scope::Only(s) if s.is_empty())
}

/// `true` when the Seatbelt Mach-lookup deputy audit (agent-bridle#405) is
/// complete on this build — i.e. [`seatbelt_impl::MACH_DEPUTY_AUDIT`] is
/// `Complete`. A narrow, read-only crossing of the `seatbelt_impl` cfg
/// boundary so `report.rs`'s cross-platform `enforcement_report` can gate a
/// `net → Kernel` claim on the same audit state that already gates the L3
/// `resolved_authority` projection ([`seatbelt_impl::seatbelt_net_projection`]),
/// without making the enum or the constant itself public. On a non-macOS
/// build (or without the `macos-seatbelt` feature) the audit cannot exist, so
/// this is unconditionally `false` — never a claim this platform cannot back.
#[must_use]
pub(crate) fn seatbelt_mach_deputy_audit_complete() -> bool {
    #[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
    {
        seatbelt_impl::MACH_DEPUTY_AUDIT == seatbelt_impl::MachDeputyAudit::Complete
    }
    #[cfg(not(all(target_os = "macos", feature = "macos-seatbelt")))]
    {
        false
    }
}

/// `true` when this process is NOT running as root (effective UID ≠ 0). ADR
/// 0015 amendment E6's probes all ran unprivileged (agent-bridle#416 round-2
/// review, item 2): a root-owned bridle process can reach kernel controls
/// (privileged IOKit classes, sysctl write paths, privileged Mach services)
/// the probes never exercised, so the Kernel net witness must not extend to
/// it. Mirrors [`seatbelt_mach_deputy_audit_complete`]'s cfg-crossing shape:
/// unconditionally `true` off a Seatbelt-capable build, since nothing there
/// depends on it (`seatbelt_net_kernel_witness`'s `audit_complete` term is
/// already `false` there).
#[must_use]
pub(crate) fn seatbelt_caller_is_unprivileged() -> bool {
    #[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
    {
        !seatbelt_impl::caller_is_root()
    }
    #[cfg(not(all(target_os = "macos", feature = "macos-seatbelt")))]
    {
        true
    }
}

/// `true` when a Seatbelt `net` scope is a **Kernel** egress-deny witness: the
/// deny-all shape (`net_fully_denied`, which already implies zero `mach:`/
/// `unix:` grants — they are entries in the same non-empty scope set), a
/// complete Mach-lookup deputy audit (agent-bridle#405/ADR 0015 E6), the
/// spawn's stdio in the shape that audit covered, and an unprivileged
/// spawning process (agent-bridle#416 round-2 review, items 1/2 — the ADR's
/// probes assumed pipe/null-only stdio and ran unprivileged; neither was
/// enforced by construction). Pure — every precondition is a parameter rather
/// than read from ambient state, so `report.rs`'s test suite can pin this
/// predicate directly, independent of whichever state the production
/// constant ships or the actual privilege of whatever process runs the suite
/// (`Complete` as of agent-bridle#405/ADR 0015 amendment E6, 2026-10-01; see
/// [`seatbelt_impl::MACH_DEPUTY_AUDIT`]'s doc comment for the evidence).
#[must_use]
pub(crate) fn seatbelt_net_kernel_witness(
    effective: &Caveats,
    audit_complete: bool,
    stdio_audited: bool,
    caller_unprivileged: bool,
) -> bool {
    audit_complete && stdio_audited && caller_unprivileged && net_fully_denied(effective)
}

/// An explicit `unix:<path>` token names a path-anchored Unix-domain socket
/// endpoint, distinct from a DNS host. It participates in the ordinary net
/// scope like any other grant, but filesystem grants never imply permission
/// to connect one — only Seatbelt projects this endpoint authority
/// ([`seatbelt_impl`]'s `unix_socket_paths`/profile emission).
#[must_use]
pub(crate) fn has_unix_socket_grants(caveats: &Caveats) -> bool {
    matches!(&caveats.net, crate::Scope::Only(s) if s.iter().any(|h| h.starts_with("unix:")))
}

/// The `net`-scope token prefix that names one Mach service a network-denied
/// macOS child may look up: `mach:<global-name>` (agent-bridle#405).
///
/// Under a network-denied Seatbelt profile the Mach-lookup floor is **zero**:
/// every named service is kernel-denied unless the operator grants it by name
/// with one of these tokens. Nothing is ambient. A grant is a scope entry like
/// any other — it rides in the ordinary `net` axis, is narrowed by `meet`, and
/// is projected as a named class (`seatbelt-mach-service:<name>`) so admission
/// compares it honestly and the report shows it. Only Seatbelt can enforce it;
/// every other backend refuses the token outright (never silently ambient).
pub const MACH_GRANT_PREFIX: &str = "mach:";

/// `true` if the `net` axis carries at least one `mach:<service>` grant.
#[must_use]
pub(crate) fn has_mach_service_grants(caveats: &Caveats) -> bool {
    matches!(&caveats.net, crate::Scope::Only(s) if s.iter().any(|h| h.starts_with(MACH_GRANT_PREFIX)))
}

/// `true` iff `token` is a **structural** net-scope entry — a `unix:` endpoint
/// or a `mach:` service grant — rather than an IP/DNS host name. Both are
/// Seatbelt-only, and neither is an egress host the proxy could admit.
#[must_use]
pub(crate) fn is_structural_net_token(token: &str) -> bool {
    token.starts_with("unix:") || token.starts_with(MACH_GRANT_PREFIX)
}

/// The validated, sorted, de-duplicated Mach service names granted by
/// `mach:<global-name>` tokens in `caveats.net`. `Ok(empty)` when the axis is
/// unrestricted or carries no such token.
///
/// A global name is a launchd/XPC service label such as
/// `com.apple.system.opendirectoryd.libinfo`: non-empty, ASCII letters, digits,
/// `.`, `_` and `-` only. Anything else — an empty name, whitespace, a quote,
/// a wildcard — fails closed, so a crafted token can never reach the SBPL
/// profile (which quotes the literal, but the validation keeps the grant
/// vocabulary exact rather than relying on quoting alone).
///
/// # Errors
/// [`ToolError::Denied`] when any `mach:` token is malformed.
pub fn mach_service_grants(caveats: &Caveats) -> ToolResult<Vec<String>> {
    let crate::Scope::Only(names) = &caveats.net else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = names
        .iter()
        .filter_map(|token| token.strip_prefix(MACH_GRANT_PREFIX))
        .map(|name| {
            let valid = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
            if valid {
                Ok(name.to_owned())
            } else {
                Err(crate::ToolError::denied(format!(
                    "mach: service grant must name a launchd global-name \
                     (ASCII letters, digits, '.', '_', '-'); got {name:?}"
                )))
            }
        })
        .collect::<ToolResult<_>>()?;
    out.sort();
    out.dedup();
    Ok(out)
}

/// The named class under which one granted Mach service appears in the
/// resolved-authority lattice (`ResolvedScope::classes`) — following the
/// `appcontainer-loopback-exemption` precedent: a grant that is real authority
/// the mechanism permits, revealed by name so it can never collapse to `∅`.
#[must_use]
pub fn seatbelt_mach_service_class(service: &str) -> String {
    format!("seatbelt-mach-service:{service}")
}

/// The Mach services a network-denied child *commonly needs* to run ordinary
/// tooling — the **candidate grant set** an operator (or a host prompt) may open
/// by name with a `mach:<service>` token. Until agent-bridle#405 this list was
/// re-allowed ambiently under `net:none`; it is now **withheld by default** and
/// listed here only so a host can name what it withheld.
///
/// None of these services has native evidence that it cannot act as a network
/// deputy for the child, so none is in the default floor (which is empty). The
/// native breakage measurement recorded in ADR 0015 (2026-09-28 amendment)
/// found: `opendirectoryd.libinfo` is needed for uid→name resolution (`id -un`,
/// `git commit` without a configured identity); `SecurityServer` for
/// keychain access (`security`); git, cargo, sh, python (incl. TLS default
/// certs), curl and xcrun ran without any of them.
pub const MACH_SERVICE_CANDIDATES: &[&str] = &[
    "com.apple.system.opendirectoryd.libinfo",
    "com.apple.system.opendirectoryd.membership",
    "com.apple.system.DirectoryService.libinfo_v1",
    "com.apple.system.notification_center",
    "com.apple.CoreServices.coreservicesd",
    "com.apple.coreservices.launchservicesd",
    "com.apple.dyld.closured",
    "com.apple.logd",
    "com.apple.logd.events",
    "com.apple.diagnosticd",
    "com.apple.SecurityServer",
    "com.apple.trustd.agent",
];

/// The structured Mach-service posture of one network-denied Seatbelt run —
/// the "service X denied" result a host needs to offer an operator grant
/// (agent-bridle#405). The kernel denies a Mach lookup **silently** (no
/// per-lookup signal reaches the child or the harness), so this is derived
/// from the policy actually installed, not observed after the fact: `granted`
/// is exactly the set re-allowed in the profile, `withheld` is every known
/// candidate the profile denies. Informational (a [`crate::Disclosure`]
/// field): it never raises or lowers the enforcement claim.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MachServiceDisclosure {
    /// Services re-allowed by name (`mach:<service>` grants), sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub granted: Vec<String>,
    /// Known candidate services ([`MACH_SERVICE_CANDIDATES`]) the profile
    /// denies because no grant named them, sorted. A child that fails on one
    /// of these is a candidate for an operator grant; the host owns the prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withheld: Vec<String>,
}

/// The [`MachServiceDisclosure`] for `caveats` under `kind`, or `None` when no
/// Mach floor is installed: only a **Seatbelt** run whose direct network is
/// fully denied (an empty scope, or `unix:`/`mach:` entries only) carries the
/// zero floor. A loopback or remote-host shape leaves Mach lookup ambient (so
/// nothing is withheld), and no other backend installs a Mach floor at all.
///
/// # Errors
/// [`ToolError::Denied`] when a `mach:` token is malformed.
pub fn mach_service_disclosure(
    caveats: &Caveats,
    kind: SandboxKind,
) -> ToolResult<Option<MachServiceDisclosure>> {
    if kind != SandboxKind::Seatbelt || !net_direct_denied(caveats) {
        return Ok(None);
    }
    let granted = mach_service_grants(caveats)?;
    let withheld = MACH_SERVICE_CANDIDATES
        .iter()
        .map(|s| (*s).to_string())
        .filter(|s| !granted.contains(s))
        .collect();
    Ok(Some(MachServiceDisclosure { granted, withheld }))
}

/// `true` iff the `net` axis denies **every direct socket** of the child: it
/// is `Only(set)` and every entry is structural (`unix:` endpoint or `mach:`
/// service grant) — including the empty set. This is the shape under which
/// Seatbelt emits `(deny network*)` plus the zero Mach floor. It generalizes
/// [`net_fully_denied`] (empty set only), which the Landlock/AppContainer
/// backends keep because they cannot express either structural entry.
#[must_use]
pub(crate) fn net_direct_denied(caveats: &Caveats) -> bool {
    matches!(&caveats.net, crate::Scope::Only(s) if s.iter().all(|h| is_structural_net_token(h)))
}

/// `true` when the `exec` axis is a deny-all empty allow-list (`Scope::Only([])`).
///
/// An empty allow-list means *no program may be spawned* — any `exec` call is
/// refused. On Windows AppContainer this maps to the
/// `PROCESS_CREATION_CHILD_PROCESS_RESTRICTED` kernel mitigation, so the
/// sandboxed process cannot create child processes at the kernel level.
#[must_use]
pub(crate) fn exec_fully_denied(caveats: &Caveats) -> bool {
    matches!(&caveats.exec, crate::Scope::Only(s) if s.is_empty())
}

/// Whether a restricted `exec` grant pulls an **implementation-variant**
/// executable that the Caveat does not literally name — so the Seatbelt
/// `process-exec*` profile must permit a program beyond the requested identities
/// and is therefore NOT an exact Kernel witness of the exec authority.
///
/// Today the sole such variant is Apple's `/bin/sh`, a launcher that re-execs
/// `/bin/bash` at startup (a kernel-checked `process-exec`); a granted `sh`
/// (bare or `/bin/sh`) forces `/bin/bash` into the allow-list too (see the
/// macOS `resolve_exec_targets`). Pure and cross-platform (a function of the
/// Caveat data), so the report classification and the profile builder stay
/// coupled to the same rule regardless of which platform compiles the profile.
#[must_use]
pub(crate) fn exec_grant_pulls_launcher_variant(caveats: &Caveats) -> bool {
    matches!(&caveats.exec, crate::Scope::Only(s)
        if s.iter().any(|p| p == "sh" || p == "/bin/sh"))
}

/// `true` if this kernel supports Landlock TCP network rules (ABI V4, kernel ≥ 6.7).
/// Always `false` on non-Linux or builds without `linux-landlock`.
#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
pub(crate) fn landlock_net_capable() -> bool {
    landlock_impl::landlock_net_is_supported()
}
#[cfg(not(all(target_os = "linux", feature = "linux-landlock")))]
pub(crate) fn landlock_net_capable() -> bool {
    false
}

/// The host tokens that name the machine's own **loopback interface**. SBPL's
/// `(remote ip "localhost:*")` filter matches exactly these destinations
/// (`127.0.0.1` and `::1`) — empirically the *only* remote a non-empty SBPL net
/// rule can name (an arbitrary IP is rejected: "host must be * or localhost").
pub(crate) const LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1"];

/// `true` if the `net` axis is restricted to a **non-empty** allow-list whose
/// every host is a [loopback identifier](LOOPBACK_HOSTS) — the one non-deny-all
/// net policy SBPL *can* kernel-enforce (`(deny network*)` + `(allow network*
/// (remote ip "localhost:*"))`), confining egress to the loopback interface so the
/// process's **own off-box socket egress is kernel-denied** (ADR 0015; the
/// system-resolver DNS residual is shared with the empty-net case). A general remote
/// host cannot be named in SBPL (only `*`/`localhost` + ports), so a mixed or
/// non-loopback allow-list is **not** loopback-only and stays advisory — never
/// silently dropped. Mutually exclusive with [`net_fully_denied`] (empty set).
///
/// The kernel rule confines egress to the loopback *interface* — `localhost` =
/// `127.0.0.1` **and** `::1`, the finest grain SBPL can name. For a **spawned
/// child** (governed only by the kernel rule, not the in-process leash) that
/// interface *is* the boundary, so a grant naming a single loopback address
/// (e.g. `127.0.0.1`) still permits the other (`::1`) — a widening strictly
/// *within* loopback, never off-box. Admission (`ToolContext::check_net`,
/// exact-match) narrows to the granted host for the engine's *own* operations.
/// Unlike the fs `(subpath root)` case — where the kernel subtree and the granted
/// root denote the same set — the loopback interface can exceed a single-address
/// grant; see ADR 0015 D2.
#[must_use]
pub(crate) fn net_loopback_only(caveats: &Caveats) -> bool {
    matches!(&caveats.net, crate::Scope::Only(s)
        if s.iter().any(|h| LOOPBACK_HOSTS.contains(&h.as_str()))
            && s.iter().all(|h| is_structural_net_token(h) || LOOPBACK_HOSTS.contains(&h.as_str())))
}

/// `true` iff a loopback-only net scope denotes the **entire** kernel-enforced
/// loopback interface, so the Seatbelt/AppContainer loopback fence — which always
/// allows the whole `localhost` interface (`127.0.0.1` **and** `::1`) — is an
/// **exact** witness of the requested authority rather than a widening.
///
/// This is the OCAP scope-fidelity gate (Kernel *strength* ≠ least *authority*):
/// a single-address grant such as `net = Only({"127.0.0.1"})` asks for v4
/// loopback ONLY, but the kernel fence also permits `::1` — strictly MORE
/// authority than the Caveat. Such a scope is NOT an exact Kernel witness and is
/// reported below Kernel (so a `CONFINED` floor refuses; the coarser fence is
/// documented BOUNDED, never pretended exact). The full interface is denoted by
/// `localhost` (the interface token) or by naming BOTH `127.0.0.1` and `::1`;
/// the egress-proxy fence ([`loopback_fenced_caveats`]) grants the full
/// [`LOOPBACK_HOSTS`] set and so remains exact.
#[must_use]
pub(crate) fn net_loopback_full_interface(caveats: &Caveats) -> bool {
    // A `unix:` endpoint riding alongside the loopback interface is real
    // additional authority AppContainer cannot express or enforce at all (it
    // rejects such a grant outright, see `appcontainer_impl::command_prefix`)
    // — never let its presence read as "still exactly the loopback interface".
    if has_unix_socket_grants(caveats) || has_mach_service_grants(caveats) {
        return false;
    }
    matches!(&caveats.net, crate::Scope::Only(s) if
        net_loopback_only(caveats)
            && (s.iter().any(|h| h == "localhost")
                || (s.iter().any(|h| h == "127.0.0.1") && s.iter().any(|h| h == "::1"))))
}

/// The granted host set of a **general remote-host** `net` allow-list — the case
/// SBPL cannot express and [`net_loopback_only`] therefore leaves advisory
/// (ADR 0015 D3). `Some(hosts)` iff `net` is `Only(set)`, non-empty, with **at
/// least one non-loopback host**; `None` for `All`, the empty set (deny-all), and
/// a loopback-only allow-list — those three keep their existing owners
/// ([`net_fully_denied`] / [`net_loopback_only`]).
///
/// This is the trigger for the macOS **egress-proxy** mechanism (#124, ADR 0016):
/// a caller confines a spawned child's egress to the loopback interface
/// ([`loopback_fenced_caveats`], reusing the ADR 0015 kernel fence) and runs a
/// loopback forward proxy that enforces this host set. Pure; no IO. The returned
/// set is the **full** grant (loopback members included — the proxy admits them
/// too), matching `ToolContext::check_net`'s exact-name membership.
/// `unix:` endpoints are not DNS hosts and never appear in the returned set —
/// they stay exclusively in the child's kernel Seatbelt profile, never handed
/// to the loopback-proxy host list.
#[must_use]
pub fn net_egress_proxy_hosts(caveats: &Caveats) -> Option<Vec<String>> {
    match &caveats.net {
        crate::Scope::Only(s)
            if s.iter()
                .any(|h| !is_structural_net_token(h) && !LOOPBACK_HOSTS.contains(&h.as_str())) =>
        {
            Some(
                s.iter()
                    .filter(|h| !is_structural_net_token(h))
                    .cloned()
                    .collect(),
            )
        }
        _ => None,
    }
}

/// The confinement caveats for a spawned child paired with a loopback **egress
/// proxy** (#124, ADR 0016): identical to `caveats` except the `net` axis is
/// replaced by the loopback set, so its [`seatbelt_profile`] emits the ADR 0015
/// kernel fence — `(deny network*)` + `(allow network* (remote ip
/// "localhost:*"))` — while the `fs`/`exec` rules are preserved verbatim. The
/// child can then reach *nothing* off-box directly; its only path off the
/// loopback interface is the proxy it is pointed at via `*_PROXY` env. Pure; no
/// IO. Only meaningful for a grant where [`net_egress_proxy_hosts`] is `Some`.
/// Explicit `unix:` endpoints in `caveats` survive the fence: they are
/// separate exact outbound exceptions under Seatbelt, never additional IP
/// authority, so they are carried into the fenced net set alongside loopback.
#[must_use]
pub fn loopback_fenced_caveats(caveats: &Caveats) -> Caveats {
    let mut endpoints: std::collections::BTreeSet<String> =
        LOOPBACK_HOSTS.iter().map(|h| (*h).to_string()).collect();
    if let crate::Scope::Only(names) = &caveats.net {
        endpoints.extend(names.iter().filter(|h| h.starts_with("unix:")).cloned());
    }
    Caveats {
        net: crate::Scope::Only(endpoints),
        ..caveats.clone()
    }
}

/// The egress-proxy plan for `caveats` (#124/#257, ADR 0016), or `None` to fall
/// through to the ordinary confinement paths. `Some((allow_hosts, fenced))`
/// **iff** the grant is a general remote-host `net` allow-list
/// ([`net_egress_proxy_hosts`]) *and* the available backend can kernel-fence the
/// child's egress **to the loopback interface** ([`loopback_net_enforceable`]) —
/// the precondition for the proxy to be real confinement instead of a
/// walk-around-able advisory. A proxy a rogue child can dial around is not
/// confinement, so backends that cannot address-fence stay inert (the ADR 0015
/// honest posture); their `net` remains honestly advisory.
///
/// The ONE decision both consumers route through — the shell engine's
/// proxied-pipeline path and `ConfinedCommand::spawn_tokio` (#257) — so the
/// check and the spawn routing cannot disagree.
#[must_use]
pub fn egress_proxy_plan(
    caveats: &Caveats,
    policy: &Arc<SandboxPolicy>,
) -> Option<(Vec<String>, Caveats)> {
    egress_proxy_plan_for(best_available_sandbox(policy).kind(), caveats)
}

/// The egress-proxy plan given an **already-resolved** available backend
/// `kind` — the pure, host-independent core of [`egress_proxy_plan`], split out
/// so the enforceability decision can be unit-tested against each backend
/// deterministically (the fail-open at #257/#275 hid behind a host-only path).
pub(crate) fn egress_proxy_plan_for(
    available: SandboxKind,
    caveats: &Caveats,
) -> Option<(Vec<String>, Caveats)> {
    if has_unix_socket_grants(caveats) && available != SandboxKind::Seatbelt {
        // No other backend projects exact Unix endpoint authority; the proxy
        // would silently drop the Unix grant instead of confining it.
        return None;
    }
    if has_mach_service_grants(caveats) {
        // A `mach:` grant has meaning only under a Mach floor, and the proxy's
        // loopback fence installs none (Mach stays ambient there). The fenced
        // caveats would erase the grant rather than confine it, so a
        // mach-bearing remote-host scope has no defined proxy semantics on ANY
        // backend: refuse the plan outright instead of dropping the token
        // (review of #406). Define and prove those semantics before enabling.
        return None;
    }
    let allow_hosts = net_egress_proxy_hosts(caveats)?;
    // Engage the proxy ONLY where the child's egress can be kernel-fenced to
    // loopback. Checking merely that the sandbox confines *something* (as the
    // pre-fix gate did via `effective_sandbox_kind != None`) is a fail-open:
    // Landlock engages on the *fs* axis (`restricts_fs`) while its `net` fence is
    // port-based and cannot confine a loopback-only host set (`apply` sets
    // `confine_net = net_fully_denied` only) — so under a general remote-host
    // grant with restricted fs on Linux, the proxy would start, the child would
    // be handed `*_PROXY`, yet the child could ignore it and dial any host
    // directly (exfil unblocked AND unrecorded). That is the exact "proxy a rogue
    // child can walk around" this must never engage. See [`loopback_net_enforceable`].
    if !loopback_net_enforceable(available) {
        return None; // net-loopback fence unenforceable → advisory, no proxy
    }
    Some((allow_hosts, loopback_fenced_caveats(caveats)))
}

/// Whether `available` can kernel-fence a spawned child's egress to the
/// **loopback interface** — the precondition for the egress-proxy pattern
/// (ADR 0016) to be real confinement rather than an advisory a child can dial
/// around. True only for the address-fenceable backends:
/// - [`SandboxKind::Seatbelt`] — SBPL `(allow network* (remote ip "localhost:*"))`.
/// - [`SandboxKind::AppContainer`] — `NetworkIsolation` loopback exemption (#133).
///
/// False for the rest, each honestly advisory on `net` for a loopback-only set:
/// - [`SandboxKind::Landlock`] — TCP rules are **port-based**, not address-based
///   (ADR 0014/0015); it can deny *all* egress (`net: none`) but cannot admit
///   only loopback. **The Linux enabler is the network-namespace egress fence**
///   (netns + veth-to-parent proxy) tracked separately — until it lands, a
///   remote-host `net` grant on Linux is advisory, not proxy-fenced.
/// - [`SandboxKind::MinimalRootfs`] — net is not namespaced at this tier.
/// - [`SandboxKind::MicroVm`] — no guest network device: egress is impossible, so
///   the loopback proxy has no path anyway (net is confined by absence, not proxy).
/// - [`SandboxKind::None`] — no backend.
#[must_use]
const fn loopback_net_enforceable(available: SandboxKind) -> bool {
    matches!(available, SandboxKind::Seatbelt | SandboxKind::AppContainer)
}

/// The [`SandboxKind`] honestly in force for `caveats` given the strongest
/// `available` backend: the backend's own kind when it will actually confine
/// *something*, else [`SandboxKind::None`]. The single honesty rule shared by the
/// subprocess primitive ([`crate::ConfinedCommand`]) and the shell engine, so
/// neither overclaims.
///
/// Capabilities differ per backend, so the engaging condition does too: Landlock
/// governs the filesystem axes; Seatbelt governs those, kernel-denies all egress
/// when `net` is empty ([`net_fully_denied`]) or confines it to the loopback
/// interface for a loopback-only allow-list ([`net_loopback_only`], ADR 0015),
/// **and** confines the `exec` axis via `process-exec*` ([`restricts_exec`]) — a
/// confinement Landlock cannot supply (ADR 0014). Landlock's exec axis stays held
/// (agent-bridle#31/#57), so a Landlock host does not engage on `exec` alone.
/// AppContainer (Windows, #51 / #123 / #133) engages when: `net` is fully denied
/// (deny-all capability model), `net` is loopback-only (egress-proxy fence, ADR 0016),
/// `exec` is fully denied (`PROCESS_CREATION_CHILD_PROCESS_RESTRICTED`, ADR 0013 D7),
/// or `fs` is restricted (per-path DACL grants, ADR 0009).
#[must_use]
pub fn effective_sandbox_kind(available: SandboxKind, caveats: &Caveats) -> SandboxKind {
    match available {
        SandboxKind::Landlock
            if restricts_fs(caveats) || (net_fully_denied(caveats) && landlock_net_capable()) =>
        {
            SandboxKind::Landlock
        }
        SandboxKind::Seatbelt
            if restricts_fs(caveats)
                || net_fully_denied(caveats)
                || net_loopback_only(caveats)
                || has_unix_socket_grants(caveats)
                || has_mach_service_grants(caveats)
                || restricts_exec(caveats) =>
        {
            SandboxKind::Seatbelt
        }
        SandboxKind::AppContainer
            if net_fully_denied(caveats)
                || net_loopback_only(caveats)
                || exec_fully_denied(caveats)
                || restricts_fs(caveats) =>
        {
            SandboxKind::AppContainer
        }
        _ => SandboxKind::None,
    }
}

/// Return the strongest [`Sandbox`] available in this build on this host.
///
/// One `cfg(target_os, feature)` arm per backend (ADR 0006 D2): Landlock probes
/// kernel support at runtime; Seatbelt probes for `sandbox-exec`; AppContainer
/// uses its process-launch wrapper on Windows. Otherwise the advisory
/// [`NoopSandbox`] is selected, so callers get a real native boundary where one
/// is available and an honest [`SandboxKind::None`] where it is not. Enabling a
/// backend feature off its target OS compiles and selects no target-specific
/// implementation.
pub fn best_available_sandbox(policy: &Arc<SandboxPolicy>) -> Box<dyn Sandbox> {
    #[cfg(all(target_os = "windows", feature = "windows-appcontainer"))]
    {
        Box::new(appcontainer_impl::AppContainerSandbox::new(
            policy.appcontainer_launcher_path.clone(),
        ))
    }

    #[cfg(not(all(target_os = "windows", feature = "windows-appcontainer")))]
    {
        #[cfg(all(target_os = "linux", feature = "linux-landlock"))]
        {
            if landlock_impl::landlock_is_supported() {
                return Box::new(landlock_impl::LandlockSandbox::with_policy(policy.clone()));
            }
        }
        #[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
        {
            if seatbelt_impl::seatbelt_is_supported() {
                return Box::new(seatbelt_impl::SeatbeltSandbox::with_policy(policy.clone()));
            }
        }
        let _ = policy; // NoopSandbox is unconfigurable (advisory).
        Box::new(NoopSandbox)
    }
}

/// Explicitly select the reviewed native root operation. Other backends refuse;
/// in particular Seatbelt's legacy fs projection is not a ruleset-grain proof.
pub(crate) fn named_root_sandbox(policy: &Arc<SandboxPolicy>) -> ToolResult<Box<dyn Sandbox>> {
    #[cfg(all(target_os = "linux", feature = "linux-landlock"))]
    if landlock_impl::landlock_is_supported() {
        return Ok(Box::new(landlock_impl::LandlockSandbox::for_named_root(
            policy.clone(),
        )));
    }
    let _ = policy;
    Err(crate::ToolError::denied("named-root execution requires the supported Landlock backend; other backend projections are not established"))
}

#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
pub use landlock_impl::{landlock_is_supported, landlock_net_is_supported, LandlockSandbox};

#[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
pub use seatbelt_impl::{seatbelt_is_supported, SeatbeltSandbox};

// Prefix construction is portable; unit tests exercise its admission decisions
// on every host. Native Windows enforcement remains in the Windows proof lane.
#[cfg(any(test, all(target_os = "windows", feature = "windows-appcontainer")))]
pub(crate) mod appcontainer_impl {
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        exec_fully_denied, net_fully_denied, net_loopback_only, restricts_fs, Sandbox, SandboxKind,
    };
    use crate::{Caveats, Scope, ToolError, ToolResult};

    /// Monotonic counter for unique container names (PID + counter → no clock).
    static SPAWN_N: AtomicU64 = AtomicU64::new(0);

    /// A Windows AppContainer process sandbox.
    ///
    /// AppContainer is attached when creating a new process via
    /// `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`; it cannot be installed on
    /// the current thread and inherited across a later spawn the way Landlock
    /// can. The spawn path must therefore use the `agent-bridle-aclaunch`
    /// wrapper binary returned by [`command_prefix`] rather than the thread
    /// `apply` path (ADR 0006 / agent-bridle#51).
    ///
    /// Calling [`Sandbox::apply`] directly fails closed: it is never correct to
    /// call `apply` expecting AppContainer confinement on the current thread.
    #[derive(Debug, Default, Clone)]
    pub struct AppContainerSandbox {
        launcher_path: Option<String>,
    }

    impl AppContainerSandbox {
        /// Construct the sandbox. Confinement is per-process; the optional path
        /// pins the trusted launcher when it is not shipped beside the binary.
        pub fn new(launcher_path: Option<String>) -> Self {
            Self { launcher_path }
        }
    }

    /// Return the trusted path of `agent-bridle-aclaunch.exe`: an explicitly
    /// configured absolute path, or the helper shipped next to the current
    /// executable. Ambient `PATH` is not a provenance source for AppContainer.
    fn find_launcher(configured: Option<&str>) -> ToolResult<String> {
        const LAUNCHER: &str = "agent-bridle-aclaunch.exe";
        let canonical_launcher_path = |path: &Path, source: &str| -> ToolResult<String> {
            if !path.is_absolute() {
                return Err(ToolError::denied(format!(
                    "windows-appcontainer: {source} launcher path {path:?} is not absolute; \
                     cannot confine"
                )));
            }
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !file_name.eq_ignore_ascii_case(LAUNCHER) {
                return Err(ToolError::denied(format!(
                    "windows-appcontainer: {source} launcher path {path:?} is not named \
                     {LAUNCHER}; cannot confine"
                )));
            }
            if !path.is_file() {
                return Err(ToolError::denied(format!(
                    "windows-appcontainer: {source} launcher {path:?} is not an existing file; \
                     cannot confine"
                )));
            }
            std::fs::canonicalize(path)
                .map(|p| p.to_string_lossy().into_owned())
                .map_err(|error| {
                    ToolError::denied(format!(
                        "windows-appcontainer: could not canonicalize {source} launcher \
                         {path:?}: {error}; cannot confine"
                    ))
                })
        };

        if let Some(raw) = configured {
            if raw.is_empty() {
                return Err(ToolError::denied(
                    "windows-appcontainer: configured launcher path is empty; cannot confine",
                ));
            }
            return canonical_launcher_path(Path::new(raw), "configured");
        }

        // Same directory as the current exe — the normal install layout.
        if let Ok(mut p) = std::env::current_exe() {
            p.set_file_name(LAUNCHER);
            if p.is_file() {
                return canonical_launcher_path(&p, "shipped sibling");
            }
        }
        Err(ToolError::denied(
            "windows-appcontainer: agent-bridle-aclaunch.exe not found next to the \
             current executable and no configured absolute launcher path was supplied; \
             PATH is not searched; cannot confine",
        ))
    }

    impl Sandbox for AppContainerSandbox {
        fn kind(&self) -> SandboxKind {
            SandboxKind::AppContainer
        }

        /// Faithful ruleset-grain projection of the AppContainer + DACL fence
        /// (#317 INV-BOUND / E2). Derived from the SAME grants `command_prefix`
        /// emits and the `agent-bridle-aclaunch` DACL actually installs — NEVER
        /// `from_delegated`, which would merely re-assert the requested caveats the
        /// #317 audit disputed. DECLARED ≠ RESOLVED ≠ APPLIED: this is the RESOLVED
        /// bound, and it must never claim narrower authority than the ACL applies.
        ///
        /// **fs — E2 (write ⇒ read):** `agent-bridle-aclaunch` grants every
        /// `--fs-write` path `FILE_GENERIC_READ_WRITE` (`main.rs`: `READ | WRITE`,
        /// "a superset of read") with subtree inherit and no DENY ACE — there is no
        /// write-only ACE — so a write-granted path is kernel-**readable**. The
        /// faithful resolved READ authority is therefore `fs_read ∪ fs_write`, never
        /// the requested read scope alone. When `fs_write ⊄ fs_read` this union is a
        /// `Superset` of the delegated read bound, so `admit` refuses fail-closed —
        /// the leak becomes a refusal, not a silent widening. (Native-proven on real
        /// Windows: a write-only-granted dir is readable by the AppContainer child;
        /// an ungranted neighbour is `Access is denied`.)
        ///
        /// **exec:** AppContainer bounds exec ONLY via the deny-all child-process
        /// block (`--no-child-process`, engaged iff exec is fully denied) → `∅`. A
        /// NON-empty allowlist is not kernel-bounded — the child may `CreateProcess`
        /// any image; enforcing the allowlist is the harness leash's Interceptor
        /// job, not the container's — so it is `Unknown` ⇒ `admit` refuses a
        /// restricted-exec-as-Kernel contract. `All` → `Unbounded` (honest: no exec
        /// bound). Never let a non-empty allowlist masquerade as kernel-enforced.
        ///
        /// **net:** deny-by-default (no `INTERNET_CLIENT` capability) → `∅`;
        /// `--net-allow` (full client capability) → `Unbounded`; the loopback
        /// exemption is all-or-nothing (it grants the WHOLE loopback interface —
        /// `127.0.0.0/8` + `::1`, every port — not a requested subset), so union a
        /// `loopback-exemption` class to REVEAL that widening; a specific remote-host
        /// allowlist AppContainer cannot faithfully bound → `Unknown` ⇒ refuse.
        fn resolved_authority(
            &self,
            effective: &Caveats,
            _stdio: crate::StdioPosture,
        ) -> crate::ResolvedAuthority {
            use crate::ResolvedScope as Rs;
            // fs: mirror the aclaunch DACL — a write ACE (FILE_GENERIC_READ_WRITE)
            // confers read, so the resolved read scope unions the write scope.
            let fs_read =
                Rs::from_scope(&effective.fs_read).union(&Rs::from_scope(&effective.fs_write));
            let fs_write = Rs::from_scope(&effective.fs_write);
            // exec: bounded ONLY by the deny-all child-process block; any non-empty
            // allowlist is Unknown (Interceptor, not a kernel bound).
            let exec = if exec_fully_denied(effective) {
                Rs::from_scope(&effective.exec) // ∅ — no child process may be created
            } else {
                match &effective.exec {
                    Scope::All => Rs::Unbounded,
                    Scope::Only(_) => Rs::Unknown,
                }
            };
            // net: deny-by-default ⇒ ∅; loopback exemption widens to the whole
            // interface (reveal via a class); remote-host allowlist ⇒ Unknown.
            let net = if net_fully_denied(effective) {
                Rs::from_scope(&effective.net) // ∅ — no INTERNET_CLIENT capability
            } else if net_loopback_only(effective) {
                Rs::from_scope(&effective.net).union(&Rs::class("appcontainer-loopback-exemption"))
            } else {
                match &effective.net {
                    Scope::All => Rs::Unbounded,
                    Scope::Only(_) => Rs::Unknown,
                }
            };
            crate::ResolvedAuthority {
                fs_read,
                fs_write,
                exec,
                net,
            }
        }

        /// No-op: AppContainer confinement is applied at process creation via the
        /// `command_prefix` launcher wrapper (`agent-bridle-aclaunch`), not via
        /// this thread. `apply` is reached only when `command_prefix` returned an
        /// empty prefix (nothing to confine), so a no-op is correct here.
        fn apply(&self, _effective: &Caveats) -> ToolResult<()> {
            Ok(())
        }

        /// Build the `["agent-bridle-aclaunch.exe", ...]` prefix that wraps the
        /// child inside a fresh AppContainer profile.
        ///
        /// Returns an empty prefix when nothing on a governed axis is restricted
        /// (so the spawn runs unwrapped — the backend engages only when it
        /// actually confines something). Fails closed if the launcher binary is
        /// not found.
        fn command_prefix(&self, effective: &Caveats) -> ToolResult<Vec<String>> {
            if super::has_unix_socket_grants(effective) {
                return Err(ToolError::denied(
                    "windows-appcontainer: exact Unix endpoint grants require Seatbelt",
                ));
            }
            if super::has_mach_service_grants(effective) {
                return Err(ToolError::denied(
                    "windows-appcontainer: mach: service grants require Seatbelt",
                ));
            }
            // The launcher engages when:
            //  - net is fully denied (deny-by-default network policy)
            //  - net is loopback-only (egress proxy path, #133)
            //  - exec is fully denied (kernel child-process-creation block)
            //  - fs is restricted (ACL grants let the container reach its workspace)
            if !net_fully_denied(effective)
                && !net_loopback_only(effective)
                && !exec_fully_denied(effective)
                && !restricts_fs(effective)
            {
                return Ok(Vec::new());
            }

            // Fail-closed: without the launcher we cannot enforce.
            let launcher = find_launcher(self.launcher_path.as_deref())?;

            // Unique container name: PID + monotonic counter (no wall clock).
            let n = SPAWN_N.fetch_add(1, Ordering::Relaxed);
            let container_name = format!("ab{}{}", std::process::id(), n);

            let mut prefix = vec![launcher, "--name".to_string(), container_name];

            // Grant network capabilities only when net is fully unrestricted
            // (Scope::All). Any non-All net scope denies egress by default via
            // the AppContainer's deny-by-default network policy.
            if matches!(effective.net, Scope::All) {
                prefix.push("--net-allow".to_string());
            }

            // Loopback-only fence (#133, ADR 0016): AppContainers block loopback
            // by default. For the egress-proxy pattern the child must reach the
            // parent's loopback proxy, so grant the loopback exemption via the
            // NetworkIsolationSetAppContainerConfig API.
            if net_loopback_only(effective) {
                prefix.push("--loopback-exemption".to_string());
            }

            // Kernel-block child process creation when exec is fully denied.
            // The `--no-child-process` flag sets PROCESS_CREATION_CHILD_PROCESS_RESTRICTED
            // on the spawned process — the kernel refuses any CreateProcess call
            // it makes, closing the exec axis by OS enforcement (#123).
            if exec_fully_denied(effective) {
                prefix.push("--no-child-process".to_string());
            }

            // FS ACL narrowing (#51): grant the AppContainer SID access to the
            // allowed paths so the container can read/write its workspace.
            // AppContainers are denied user directories by default; without this
            // grant the child cannot access its working directory.
            if let Scope::Only(paths) = &effective.fs_write {
                for p in paths {
                    prefix.push("--fs-write".to_string());
                    prefix.push(p.clone());
                }
            }
            // Read-only paths that are not already covered by fs_write.
            let write_set: std::collections::HashSet<&str> =
                if let Scope::Only(paths) = &effective.fs_write {
                    paths.iter().map(String::as_str).collect()
                } else {
                    std::collections::HashSet::new()
                };
            if let Scope::Only(paths) = &effective.fs_read {
                for p in paths {
                    if !write_set.contains(p.as_str()) {
                        prefix.push("--fs-read".to_string());
                        prefix.push(p.clone());
                    }
                }
            }

            Ok(prefix)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn missing_launcher_is_a_denial() {
            let err = find_launcher(Some("")).expect_err("empty launcher must deny");
            assert!(
                matches!(err, ToolError::Denied { ref reason } if reason.contains("configured launcher path is empty")),
                "missing AppContainer launcher must be a denial, got {err:?}"
            );
        }

        #[test]
        fn configured_launcher_must_be_absolute() {
            let err = find_launcher(Some("agent-bridle-aclaunch.exe"))
                .expect_err("relative configured launcher must deny");
            assert!(
                matches!(err, ToolError::Denied { ref reason } if reason.contains("is not absolute")),
                "relative AppContainer launcher must be a denial, got {err:?}"
            );
        }

        fn assert_appcontainer_rejects_unix(names: &[&str]) {
            let caveats = Caveats {
                net: Scope::only(names.iter().map(|name| (*name).to_owned())),
                ..Caveats::top()
            };
            let result = AppContainerSandbox::new(None).command_prefix(&caveats);
            assert!(
                matches!(&result, Err(ToolError::Denied { reason }) if reason.contains("Unix")),
                "unsupported Unix authority must refuse before launcher lookup: {result:?}"
            );
        }

        #[test]
        fn appcontainer_command_prefix_rejects_unix_only() {
            assert_appcontainer_rejects_unix(&["unix:/private/tmp/service.sock"]);
        }

        #[test]
        fn appcontainer_command_prefix_rejects_unix_with_loopback() {
            assert_appcontainer_rejects_unix(&["unix:/private/tmp/service.sock", "localhost"]);
        }
    }
}

/// E2 adversarial regression: the AppContainer faithful projection + mesh
/// admission fail CLOSED on unrepresentable narrowing (`fs_write ⇒ read`, exec/net
/// honesty). Proves DECLARED ≠ RESOLVED — the resolved authority reveals the DACL
/// widening and admission refuses it, rather than re-asserting the delegated grant.
#[cfg(all(test, target_os = "windows", feature = "windows-appcontainer"))]
mod appcontainer_resolved_authority_tests {
    use super::appcontainer_impl::AppContainerSandbox;
    use super::Sandbox;
    use crate::{
        admit, empty_closure, AdmissionDecision, Caveats, ConfinedAxis, ResolvedScope, Scope,
        ScopeRelation,
    };

    /// exec + net fully denied so those axes admit (`∅ ⊆ ∅`); the fs axes are the
    /// variable under test.
    fn fs_probe(read: &[&str], write: &[&str]) -> Caveats {
        Caveats {
            fs_read: Scope::only(read.iter().map(|s| (*s).to_string())),
            fs_write: Scope::only(write.iter().map(|s| (*s).to_string())),
            exec: Scope::only(std::iter::empty::<String>()),
            net: Scope::only(std::iter::empty::<String>()),
            ..Caveats::top()
        }
    }

    fn decide(caveats: &Caveats) -> AdmissionDecision {
        let resolved = AppContainerSandbox::new(None)
            .resolved_authority(caveats, crate::StdioPosture::Unaudited);
        admit(&resolved, caveats, &empty_closure())
    }

    /// THE E2 fail-closed case: a write-granted path NOT in the read scope is
    /// kernel-readable (the aclaunch DACL grants `FILE_GENERIC_READ_WRITE`), so the
    /// resolved read authority is a Superset of the requested read → REFUSE.
    #[test]
    fn write_only_path_widens_read_and_refuses() {
        let c = fs_probe(&["C:/repo"], &["C:/dropbox"]); // fs_write ⊄ fs_read
        match decide(&c) {
            AdmissionDecision::Reject(r) => {
                assert_eq!(r.axis, ConfinedAxis::FsRead, "the read axis is the widened one");
                assert_eq!(
                    r.relation,
                    ScopeRelation::Superset,
                    "read is widened by the write grant, not incomparable/unknown"
                );
            }
            AdmissionDecision::Admit => panic!(
                "fs_write ⊄ fs_read must refuse: the write ACE confers read the grant did not authorize"
            ),
        }
        // The projection itself must reveal the widening (never == the delegated read).
        let resolved =
            AppContainerSandbox::new(None).resolved_authority(&c, crate::StdioPosture::Unaudited);
        assert_ne!(
            resolved.fs_read,
            ResolvedScope::from_scope(&c.fs_read),
            "resolved read must fold in the write scope, not re-assert the requested read"
        );
    }

    /// Positive control: `fs_write ⊆ fs_read` → resolved read == requested read → ADMIT.
    #[test]
    fn write_subset_of_read_admits() {
        let c = fs_probe(&["C:/repo", "C:/work"], &["C:/work"]); // fs_write ⊆ fs_read
        assert_eq!(
            decide(&c),
            AdmissionDecision::Admit,
            "a write scope inside the read scope adds no new read authority"
        );
    }

    /// exec: a NON-empty allowlist is not kernel-bounded by AppContainer → `Unknown`
    /// → REFUSE (never let a restricted-exec config read as kernel-enforced).
    #[test]
    fn nonempty_exec_allowlist_refuses_as_unknown() {
        let c = Caveats {
            exec: Scope::only(["cmd".to_string()]),
            ..Caveats::top() // other axes unrestricted ⇒ admit; exec is the refuser
        };
        match decide(&c) {
            AdmissionDecision::Reject(r) => {
                assert_eq!(r.axis, ConfinedAxis::Exec);
                assert_eq!(r.relation, ScopeRelation::Unknown);
            }
            AdmissionDecision::Admit => {
                panic!("a restricted exec allowlist must not admit as AppContainer-enforced")
            }
        }
    }

    /// exec fully denied (`--no-child-process`) IS kernel-bounded → ADMIT.
    #[test]
    fn exec_deny_all_admits() {
        let c = Caveats {
            exec: Scope::only(std::iter::empty::<String>()),
            ..Caveats::top()
        };
        assert_eq!(decide(&c), AdmissionDecision::Admit);
    }

    /// net: a remote-host allowlist AppContainer cannot bound → `Unknown` → REFUSE.
    #[test]
    fn remote_host_net_allowlist_refuses_as_unknown() {
        let c = Caveats {
            net: Scope::only(["api.example.com:443".to_string()]),
            ..Caveats::top()
        };
        match decide(&c) {
            AdmissionDecision::Reject(r) => {
                assert_eq!(r.axis, ConfinedAxis::Net);
                assert_eq!(r.relation, ScopeRelation::Unknown);
            }
            AdmissionDecision::Admit => {
                panic!("a remote-host net allowlist must not admit as AppContainer-bounded")
            }
        }
    }

    /// net fully denied (deny-by-default, no `INTERNET_CLIENT` capability) → ADMIT.
    #[test]
    fn net_deny_all_admits() {
        let c = Caveats {
            net: Scope::only(std::iter::empty::<String>()),
            ..Caveats::top()
        };
        assert_eq!(decide(&c), AdmissionDecision::Admit);
    }
}

#[cfg(all(target_os = "linux", feature = "linux-landlock"))]
pub(crate) mod landlock_impl {
    use super::{Sandbox, SandboxKind};
    use crate::{Caveats, ChildNetworkPolicy, SandboxPolicy, Scope, ToolError, ToolResult};
    use landlock::{
        path_beneath_rules, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath,
        Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, ABI,
    };
    use std::sync::Arc;

    /// Map a configured ABI floor to the landlock `ABI` enum. `apply` runs
    /// `BestEffort`, so a floor above the running kernel still degrades
    /// gracefully; unknown/too-high values clamp to the highest ABI this crate
    /// (landlock 0.4.5) models — V7 — so raising a floor to reach a newer axis
    /// (e.g. `IoctlDev` at V5) is honored, not silently dropped to V4. The
    /// default floors (fs 3 / net 4) reproduce the previous `ABI::V3` / `ABI::V4`
    /// constants.
    ///
    /// The *lower* bound is deliberately NOT enforced here: it is axis-specific
    /// and applied at the call site via [`fs_abi_floor`] / [`net_abi_floor`],
    /// because fs and net have different safe minimums below which the honesty
    /// report would overclaim.
    fn abi_from_u32(v: u32) -> ABI {
        match v {
            0 | 1 => ABI::V1,
            2 => ABI::V2,
            3 => ABI::V3,
            4 => ABI::V4,
            5 => ABI::V5,
            6 => ABI::V6,
            _ => ABI::V7,
        }
    }

    /// The fs-axis ABI floor actually installed — never below V3 (the default).
    ///
    /// Security-critical clamp: a configured `landlock_abi_floor` below 3 would
    /// drop `Refer` (V2) / `Truncate` (V3) from the governed write set, letting a
    /// confined child `truncate`/`rename` files OUTSIDE its `fs_write` scope while
    /// [`crate::enforcement_report`] still reports `fs_write = Kernel` — a silent
    /// weakening *and* an overclaim. Lowering a floor has no legitimate use
    /// (`BestEffort` already degrades on genuinely older kernels), so we clamp up
    /// to the claimed baseline rather than honor a weakening. Raising above the
    /// default stays allowed (explicit opt-in hardening).
    fn fs_abi_floor(policy: &SandboxPolicy) -> ABI {
        abi_from_u32(policy.landlock_abi_floor.max(3))
    }

    /// The net-axis ABI floor actually installed — never below V4 (the default).
    ///
    /// TCP net rights first exist at V4, so a configured `landlock_net_abi_floor`
    /// below 4 makes `AccessNet::from_all` EMPTY; under `BestEffort`,
    /// `handle_access` of an empty set governs nothing, silently dropping a
    /// requested deny-all-egress even on a capable (≥ 6.7) kernel while the report
    /// claims `net = Kernel`. Clamp up to V4 for the same reason as
    /// [`fs_abi_floor`].
    fn net_abi_floor(policy: &SandboxPolicy) -> ABI {
        abi_from_u32(policy.landlock_net_abi_floor.max(4))
    }

    /// `true` if this kernel can enforce a Landlock ruleset.
    ///
    /// Non-destructive: it creates (but never `restrict_self`s) a throwaway
    /// ruleset under `HardRequirement`, so an unsupported kernel surfaces as
    /// `Err` rather than being silently swallowed by best-effort.
    pub fn landlock_is_supported() -> bool {
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V1))
            .and_then(|r| r.create())
            .is_ok()
    }

    /// `true` if this kernel supports Landlock TCP network rules (ABI V4,
    /// kernel ≥ 6.7). Probed non-destructively — creates but never
    /// `restrict_self`s a throwaway ruleset. This is the *capability* threshold
    /// (TCP rules first appear at V4), distinct from the configurable request
    /// floor in [`abi_from_u32`].
    pub fn landlock_net_is_supported() -> bool {
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessNet::from_all(ABI::V4))
            .and_then(|r| r.create())
            .is_ok()
    }

    // The Landlock read/exec allow-lists now live in `SandboxPolicy`
    // (config.rs) and are read from `self.policy` in `apply`. Their security
    // rationale is unchanged (ADR 0011 D3/D7):
    //
    // - `base_read_paths`: the loader/library trees + system DATA a permitted,
    //   dynamically-linked program needs to start — but NOT the executable dirs
    //   (`/usr/bin`, `/bin`, `/sbin`). Keeping bin dirs out of the read set
    //   shrinks the loader-trampoline corpus: `/usr/bin/curl` is unreadable and
    //   so cannot be `mmap`-exec'd via `ld.so`. This shrinks, but does not close,
    //   the trampoline (`/usr/lib` still hides interpreters), so `exec` stays
    //   `interceptor`, never `kernel`. `/etc` is never granted wholesale.
    // - `bin_read_paths`: executable dirs, read-allowed ONLY when `exec` is
    //   ambient (`All`); when `exec` is confined the granted binaries are added
    //   by resolved path instead, narrowing the corpus to exactly them.
    // - `loader_paths`: the dynamic linker(s) only — specific FILES, never
    //   directories (a `path_beneath` dir grant would expose every ELF beneath
    //   `/usr/lib` via the merged-usr symlink, defeating the exec axis).
    //
    // The `PathList` shrink-guard (config) means an operator can *widen* these
    // (disclosed) but can only *remove* an entry with an explicit `replace=true`.

    /// A real, kernel-enforced Landlock sandbox (Linux).
    ///
    /// **The `fs_write`, `fs_read`, and `exec` axes.** Writes are always governed
    /// (from `fs_write`); reads are governed only when `fs_read` is *restricted*
    /// (`Only(_)`), in which case the granted read roots plus the configured
    /// `base_read_paths` are read-allowed and everything else is denied — so a
    /// permitted external program cannot read user data outside `fs_read` (closing
    /// `grep -f /etc/shadow`-style reads) yet can still load its libraries.
    ///
    /// `Execute` is governed only when `exec` is restricted: the *resolved*
    /// granted program files plus the configured `loader_paths` (the dynamic linker only — never
    /// library directories, which `path_beneath` would make recursively executable
    /// and expose `/usr/lib`'s interpreters) are execute-allowed and all else
    /// denied. This kernel-denies a **direct** `execve` of a different, un-granted
    /// tool (`find -exec curl`, a written/symlinked payload, a shebang to an
    /// un-granted interpreter) — the ADR 0011 boundary increment.
    ///
    /// It does **not** close the loader/interpreter *trampoline*: with reads
    /// allow-listed, `ld.so` can `mmap`-exec any readable ELF, and a granted
    /// interpreter runs arbitrary in-process code — neither is an `execve` the
    /// `Execute` rule sees (ADR 0011 D2; Landlock has no `mmap` hook). So this is
    /// the filesystem **boundary** + direct-execve denial, **not** program
    /// identity — the per-axis report therefore keeps `exec → interceptor`, never
    /// `kernel` (ADR 0011 D7); a strong principal still fails closed on a
    /// restricted `exec` (ADR 0012 D4, already wired). The trampoline-tight close
    /// (narrowed read base + W^X + seccomp `execve`/namespace deny, or a
    /// micro-VM rootfs) is the Tier-2 follow-up (#57 / ADR 0009). When an axis is
    /// `All` it stays ambient. On ABI-v4 kernels an empty `net` scope additionally
    /// installs a deny-all TCP ruleset; hostname allowlists remain inexpressible.
    ///
    /// `restrict_self` is per-thread and irreversible, and is inherited across
    /// `fork`/`execve`. Callers must therefore call [`Sandbox::apply`] on the
    /// very thread that will spawn the confined work, immediately before the
    /// spawn.
    #[derive(Debug, Default, Clone)]
    pub struct LandlockSandbox {
        /// The read/exec allow-lists + ABI floors this backend enforces (I5-B).
        policy: Arc<SandboxPolicy>,
        exec_boundary: crate::ExecBoundary,
    }

    impl LandlockSandbox {
        /// Construct with the built-in defaults (today's allow-lists).
        pub fn new() -> Self {
            Self::default()
        }

        /// Construct configured with an operator-supplied [`SandboxPolicy`].
        pub fn with_policy(policy: Arc<SandboxPolicy>) -> Self {
            Self {
                policy,
                exec_boundary: crate::ExecBoundary::ProcessTree,
            }
        }

        pub(super) fn for_named_root(policy: Arc<SandboxPolicy>) -> Self {
            Self {
                policy,
                exec_boundary: crate::ExecBoundary::NamedRoot,
            }
        }

        fn handles_execute(&self, effective: &Caveats) -> bool {
            self.exec_boundary == crate::ExecBoundary::ProcessTree
                && matches!(effective.exec, Scope::Only(_))
        }

        // ── Shared root computation (ONE routine for both the applied ruleset and
        // the resolved-authority projection — Q2 anti-drift). The precise claim:
        // the ROOT-SET DERIVATION cannot independently drift, because both the
        // fence and the projection call this same code on the same caveats. It is
        // NOT a claim that the projection equals the kernel's actual authority:
        // native access masks, `BestEffort` compat behaviour, OS path/symlink
        // interpretation, aliases and deputies still require the later
        // CompiledFence + AppliedFenceEvidence / native-hostile-test layer to
        // establish empirical fidelity. ─────────────────────────────────────────

        /// The write roots the ruleset anchors on: the granted write scope plus
        /// the always-write-openable device sinks (#1220), existing paths only.
        fn write_roots(&self, effective: &Caveats) -> Vec<String> {
            let mut roots = scope_roots(&effective.fs_write);
            roots.extend(self.policy.device_sink_paths.resolve());
            roots.retain(|p| std::path::Path::new(p).exists());
            roots
        }

        /// The read roots the ruleset anchors on when `fs_read` is restricted: the
        /// granted read scope plus the base-read list plus, per exec-confinement,
        /// either the resolved granted programs or the bin dirs, existing only.
        fn read_roots(&self, effective: &Caveats, confine_exec: bool) -> Vec<String> {
            let mut roots = scope_roots(&effective.fs_read);
            roots.extend(self.policy.base_read_paths.resolve());
            if confine_exec {
                roots.extend(resolve_exec_paths(&effective.exec));
            } else {
                roots.extend(self.policy.bin_read_paths.resolve());
            }
            roots.retain(|p| std::path::Path::new(p).exists());
            roots
        }

        /// The execute roots the ruleset anchors on when `exec` is restricted: the
        /// resolved granted program files plus the dynamic linker(s), existing only.
        fn exec_roots(&self, effective: &Caveats) -> Vec<String> {
            let mut roots = resolve_exec_paths(&effective.exec);
            roots.extend(self.policy.loader_paths.resolve());
            roots.retain(|p| std::path::Path::new(p).exists());
            roots
        }
    }

    /// Whether every *grant-derived* root is an already-canonical, non-symlink
    /// absolute path, so the Landlock rule (whose `PathFd` opens `O_PATH` and
    /// FOLLOWS a final-component symlink — landlock-0.4.x) anchors on exactly the
    /// named path and not a wider symlink target. A symlinked or non-canonical
    /// grant root is NOT object-stable: for 0.8 the resolved authority on that axis
    /// is `Unknown` ⇒ admission refuses (the E1 fail-closed posture; the same-object
    /// FD bind that would let us honestly bound an aliased root is the PR-5 follow-up).
    /// Policy-declared closures (base-read/loader/bin/device) are NOT checked here —
    /// they are trusted, explicitly-declared runtime closure, not model-named roots.
    fn grant_roots_are_object_stable(grant_roots: &[String]) -> bool {
        grant_roots.iter().all(|p| match std::fs::canonicalize(p) {
            Ok(canon) => canon.to_str() == Some(p.as_str()),
            Err(_) => false,
        })
    }

    impl Sandbox for LandlockSandbox {
        fn kind(&self) -> SandboxKind {
            SandboxKind::Landlock
        }

        fn exec_boundary(&self) -> crate::ExecBoundary {
            self.exec_boundary
        }

        fn apply(&self, effective: &Caveats) -> ToolResult<()> {
            self.apply_with_held_roots(effective, &[])
        }

        /// Each `held` root is anchored on its descriptor and its path form is
        /// dropped from the read rules; `apply` passes none.
        fn apply_with_held_roots(
            &self,
            effective: &Caveats,
            held: &[super::HeldReadRoot],
        ) -> ToolResult<()> {
            let write = AccessFs::from_write(fs_abi_floor(&self.policy));
            // Pure read rights — `from_read` also bundles `Execute`, which we
            // govern separately (only when `exec` is restricted), never via the
            // read axis.
            let read = AccessFs::ReadFile | AccessFs::ReadDir;

            // Govern writes always; govern reads / execute only when their axis is
            // actually restricted (`Only`). `All` means no confinement was asked
            // for, so that axis stays ambient and needs no base allow-list.
            let confine_read = matches!(effective.fs_read, Scope::Only(_));
            let confine_exec = matches!(effective.exec, Scope::Only(_));
            // `net: Scope::Only([])` (empty) = deny ALL TCP bind + connect.
            // Non-empty host allow-lists are not expressible in Landlock (port-
            // based, not hostname-based) and stay advisory — only the empty-set
            // case maps cleanly to a deny-all TCP rule.
            let confine_net = super::net_fully_denied(effective);
            let mut handled = write;
            if confine_read {
                handled |= read;
            }
            if self.handles_execute(effective) {
                handled |= AccessFs::Execute;
            }

            // #1220: the device sinks are always write-openable — a confined
            // git opening `/dev/null` O_RDWR must not be what the jail breaks.
            // (O_RDWR also needs the read right: ambient when `fs_read` is
            // `All`; granted via `base_read_paths` — which lists the same
            // devices — when confined.) Built via the shared routine so the
            // resolved-authority projection anchors on the identical set.
            let write_roots = self.write_roots(effective);
            // Build the ruleset: fs axes first (V3 floor), then optionally the
            // net axis (V4+). BestEffort means handle_access silently skips
            // access types the kernel doesn't know — so on pre-6.7 kernels the
            // TCP handle is a no-op and only fs rules apply.
            let ruleset = Ruleset::default()
                .set_compatibility(CompatLevel::BestEffort)
                .handle_access(handled)
                .map_err(landlock_denied)?;
            // When net is fully denied: declare AccessNet without adding any
            // NetPort rules → deny-by-default for all TCP bind + connect.
            let ruleset = if confine_net {
                ruleset
                    .handle_access(AccessNet::from_all(net_abi_floor(&self.policy)))
                    .map_err(landlock_denied)?
            } else {
                ruleset
            };
            let ruleset = ruleset
                .create()
                .map_err(landlock_denied)?
                .add_rules(path_beneath_rules(&write_roots, write))
                .map_err(landlock_denied)?;

            let ruleset = if confine_read {
                // Granted read roots + the loader/library/data base list, so a
                // permitted binary loads while out-of-scope reads stay denied.
                // Granted read roots + the base list + (per exec-confinement) the
                // resolved granted programs or the bin dirs — via the shared
                // routine so the resolved-authority projection anchors on the
                // identical set (ADR 0011 D3: confined-exec keeps bin dirs OUT of
                // the trampoline corpus).
                let mut read_roots = self.read_roots(effective, confine_exec);
                // A held root is anchored on its descriptor below; its path
                // form is dropped so nothing re-opens that root by name after
                // the caller's check. (The resolved-authority projection still
                // lists the path: it is the same object's provenance.)
                read_roots.retain(|p| !held.iter().any(|h| p.as_str() == h.provenance()));
                let ruleset = ruleset
                    .add_rules(path_beneath_rules(&read_roots, read))
                    .map_err(landlock_denied)?;
                held.iter()
                    .try_fold(ruleset, |rs, h| rs.add_rule(PathBeneath::new(h.fd(), read)))
                    .map_err(landlock_denied)?
            } else {
                ruleset
            };

            let ruleset = if self.handles_execute(effective) {
                // Execute-allow ONLY the resolved granted program files plus the
                // dynamic linker(s) — never library directories (recursive +
                // expose `/usr/lib`'s interpreters). A permitted binary still runs
                // (its own execve + the loader + .so reads), but cannot DIRECTLY
                // execve a different, un-granted program.
                let exec_roots = self.exec_roots(effective);
                ruleset
                    .add_rules(path_beneath_rules(&exec_roots, AccessFs::Execute))
                    .map_err(landlock_denied)?
            } else {
                ruleset
            };

            let status = ruleset.restrict_self().map_err(landlock_denied)?;

            // Fail closed: if the kernel did not actually enforce the ruleset,
            // do not let the caller believe it is confined.
            if status.ruleset == RulesetStatus::NotEnforced {
                return Err(ToolError::denied(
                    "landlock ruleset was not enforced by this kernel",
                ));
            }

            // ChildNetworkPolicy::DenyDirect — the seccomp socket()-family egress
            // deny, on THIS confining thread (same thread as `restrict_self`,
            // inherited across the imminent `fork`/`execve`). Only when net is
            // already fully denied (a granted net scope leaves it inert), and
            // fail-closed: a failed install refuses the spawn rather than let the
            // caller believe UDP/DNS/raw egress is denied when it is not.
            if self.policy.child_network == ChildNetworkPolicy::DenyDirect && confine_net {
                install_seccomp_egress_deny()?;
            }
            Ok(())
        }

        fn resolved_authority(
            &self,
            effective: &Caveats,
            _stdio: crate::StdioPosture,
        ) -> crate::ResolvedAuthority {
            use crate::ResolvedScope;
            use std::collections::BTreeSet;

            let confine_read = matches!(effective.fs_read, Scope::Only(_));
            let confine_write = matches!(effective.fs_write, Scope::Only(_));
            let confine_exec = matches!(effective.exec, Scope::Only(_));

            let bounded = |roots: Vec<String>| ResolvedScope::Bounded {
                concrete: roots.into_iter().collect::<BTreeSet<String>>(),
                classes: BTreeSet::new(),
            };

            // fs_read: `All` is ambient (Unbounded). Restricted ⇒ the read roots
            // the ruleset ACTUALLY anchors on (shared routine) — UNLESS a
            // grant-derived root is symlinked/non-canonical, in which case the
            // kernel rule can anchor on a wider target (E1) and we cannot honestly
            // bound it ⇒ Unknown (refuse; the same-object bind is PR-5).
            let fs_read = if !confine_read {
                ResolvedScope::Unbounded
            } else if !grant_roots_are_object_stable(&scope_roots(&effective.fs_read)) {
                ResolvedScope::Unknown
            } else {
                bounded(self.read_roots(effective, confine_exec))
            };

            // fs_write is always governed; the grant portion must be object-stable
            // (same E1 concern) — writable symlink roots are the more dangerous case.
            let fs_write = if confine_write
                && !grant_roots_are_object_stable(&scope_roots(&effective.fs_write))
            {
                ResolvedScope::Unknown
            } else if !confine_write {
                ResolvedScope::Unbounded
            } else {
                bounded(self.write_roots(effective))
            };

            // exec: `resolve_exec_paths` canonicalizes (no grant-symlink issue).
            // The bound is process-image identity — the resolved programs + the
            // loader (the direct-execve corpus). The ld.so mmap-exec trampoline is
            // out of scope for the exec axis by definition (arbitrary-code, not a
            // process image; a separate future concern), so it is NOT a widening here.
            let exec = if self.handles_execute(effective) {
                bounded(self.exec_roots(effective))
            } else {
                ResolvedScope::Unbounded
            };

            // net: `All` is ambient (Unbounded). A RESTRICTED net axis is honestly
            // BOUNDED only where the child cannot egress at all — `net: none`
            // (fully denied) under `DenyDirect`, where the seccomp socket()+
            // io_uring deny (PR-1) closes the io_uring bypass (E3) on top of
            // Landlock's TCP deny. There the resolved authority is exactly the
            // empty host set (`from_scope(net:none)` = the bound the grant names,
            // so admission is Equal). Every other restricted net — a hostname
            // allow-list Landlock cannot express, or `net: none` under the default
            // `LandlockOnly` where io_uring stays open — cannot be bounded ⇒
            // Unknown ⇒ refuse.
            let net = if matches!(effective.net, Scope::All) {
                ResolvedScope::Unbounded
            } else if super::net_fully_denied(effective)
                && self.policy.child_network == ChildNetworkPolicy::DenyDirect
            {
                ResolvedScope::from_scope(&effective.net)
            } else {
                ResolvedScope::Unknown
            };

            crate::ResolvedAuthority {
                fs_read,
                fs_write,
                exec,
                net,
            }
        }

        fn runtime_closure(&self, effective: &Caveats) -> crate::ResolvedAuthority {
            use crate::ResolvedScope;
            use std::collections::BTreeSet;

            let confine_exec = matches!(effective.exec, Scope::Only(_));
            let existing = |mut v: Vec<String>| -> BTreeSet<String> {
                v.retain(|p| std::path::Path::new(p).exists());
                v.into_iter().collect()
            };
            let bounded = |s: BTreeSet<String>| ResolvedScope::Bounded {
                concrete: s,
                classes: BTreeSet::new(),
            };

            // OBJECT-IDENTITY harness-disjointness (review #3): a benign-looking
            // closure pathname can itself SYMLINK/alias into a harness-private
            // store, so we check each root's RESOLVED (canonical) object identity —
            // not only its lexical form — against the harness-private markers. Any
            // root whose canonical identity reaches harness-private authority
            // compromises the whole axis ⇒ `Unknown` ⇒ admission fails closed
            // (`closure_is_harness_disjoint` rejects `Unknown`, L3/L7). Benign
            // system aliases (`/lib`→`/usr/lib`, the loader) pass: their canonical
            // identity is not harness-private. (The blanket "any non-canonical
            // closure root refuses" posture is deferred to the same-object-FD
            // binding, PR-5; here we refuse only closure roots that actually
            // resolve INTO harness-private state.)
            let harness_safe_bounded = |s: BTreeSet<String>| -> ResolvedScope {
                if self.exec_boundary == crate::ExecBoundary::NamedRoot {
                    // Retain the REAL substrate entries for the NamedRoot
                    // additions-only inventory check. It checks canonical
                    // recorded protected-root overlap against explicit delegation;
                    // poisoning an already read-granted image here would hide
                    // the evidence that no extra authority was added.
                    return bounded(s);
                }
                let reaches_private = s.iter().any(|entry| {
                    crate::admitted::entry_reaches_harness_private(entry)
                        || std::fs::canonicalize(entry)
                            .ok()
                            .and_then(|canon| {
                                canon
                                    .to_str()
                                    .map(crate::admitted::entry_reaches_harness_private)
                            })
                            .unwrap_or(false)
                });
                if reaches_private {
                    ResolvedScope::Unknown
                } else {
                    bounded(s)
                }
            };

            // fs_read additions the ruleset makes BEYOND the granted read scope:
            // the base-read list (loader/lib/system-data) + (confined-exec ? the
            // resolved granted program images : the bin dirs). System runtime
            // substrate + the granted program's own image — harness-disjoint.
            let mut read_add = self.policy.base_read_paths.resolve();
            if confine_exec {
                read_add.extend(resolve_exec_paths(&effective.exec));
            } else {
                read_add.extend(self.policy.bin_read_paths.resolve());
            }

            // exec additions: the resolved granted program image (reconciling the
            // grant TOKEN with its canonical path) + the dynamic linker(s).
            let exec_add = if self.handles_execute(effective) {
                let mut e = resolve_exec_paths(&effective.exec);
                e.extend(self.policy.loader_paths.resolve());
                existing(e)
            } else {
                BTreeSet::new()
            };

            crate::ResolvedAuthority {
                fs_read: harness_safe_bounded(existing(read_add)),
                fs_write: harness_safe_bounded(existing(self.policy.device_sink_paths.resolve())),
                exec: harness_safe_bounded(exec_add),
                net: ResolvedScope::empty(),
            }
        }
    }

    /// Install the seccomp `socket()`-family egress deny on the CURRENT thread —
    /// the [`ChildNetworkPolicy::DenyDirect`] leg (`crate::ChildNetworkPolicy`).
    ///
    /// Denies `socket()` for the off-box address families (`AF_INET` /
    /// `AF_INET6` / `AF_PACKET`) with `EACCES`; `AF_UNIX` and every other syscall
    /// stay allowed. This closes the UDP/DNS/raw/packet egress leg that Landlock's
    /// TCP-only net rule cannot filter — a child under `net: none` can otherwise
    /// still create those sockets.
    ///
    /// It ALSO denies the `io_uring` family (`io_uring_setup`/`enter`/`register`)
    /// with `EACCES` (E3, the io_uring egress floor / PR-1): `IORING_OP_SOCKET` +
    /// `IORING_OP_CONNECT`/`SEND` create and use a socket **without** the
    /// `socket()` syscall, so a socket()-only filter is bypassable. seccomp
    /// cannot inspect an io_uring SQE opcode, so the honest close is to deny the
    /// io_uring setup/enter primitive entirely while net is confined — a child
    /// that asked for `net: none` does not get an un-mediated async-I/O channel.
    /// (A child needing io_uring for file I/O under `net: none` falls back to the
    /// ordinary syscalls; net confidentiality wins the trade.)
    ///
    /// `apply_filter` sets `PR_SET_NO_NEW_PRIVS`, so it needs no privilege, is
    /// irreversible, and is inherited by every `fork`/`execve` descendant.
    /// `apply_filter` is a safe fn, so core keeps `unsafe_code = forbid`. Must run
    /// on the confining thread, after `restrict_self`, immediately before the spawn.
    fn install_seccomp_egress_deny() -> ToolResult<()> {
        use seccompiler::{
            apply_filter, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp,
            SeccompCondition, SeccompFilter, SeccompRule, TargetArch,
        };
        use std::collections::BTreeMap;

        let denied =
            |e: String| ToolError::denied(format!("seccomp egress deny not installed: {e}"));

        // One rule per off-box family, matched on socket()'s `domain` arg (arg 0).
        let families: [u64; 3] = [
            libc::AF_INET as u64,
            libc::AF_INET6 as u64,
            libc::AF_PACKET as u64,
        ];
        let rules: Vec<SeccompRule> = families
            .into_iter()
            .map(|fam| {
                let cond = SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, fam)
                    .map_err(|e| denied(e.to_string()))?;
                SeccompRule::new(vec![cond]).map_err(|e| denied(e.to_string()))
            })
            .collect::<ToolResult<_>>()?;

        let mut per_syscall: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        per_syscall.insert(libc::SYS_socket, rules);
        // Empty rule vec ⇒ the syscall is denied UNCONDITIONALLY (the filter's
        // match action, EACCES). Closes the io_uring egress bypass of `net: none`.
        per_syscall.insert(libc::SYS_io_uring_setup, Vec::new());
        per_syscall.insert(libc::SYS_io_uring_enter, Vec::new());
        per_syscall.insert(libc::SYS_io_uring_register, Vec::new());

        let filter = SeccompFilter::new(
            per_syscall,
            // Default for every other syscall — and for `socket()` with a
            // non-matched family (e.g. AF_UNIX): allow.
            SeccompAction::Allow,
            // A matched off-box `socket()`: fail with EACCES (a clean, catchable
            // "permission denied" the child sees as an unreachable network).
            SeccompAction::Errno(libc::EACCES as u32),
            TargetArch::try_from(std::env::consts::ARCH).map_err(|e| denied(e.to_string()))?,
        )
        .map_err(|e| denied(e.to_string()))?;

        let prog: BpfProgram = BpfProgram::try_from(filter).map_err(|e| denied(e.to_string()))?;
        apply_filter(&prog).map_err(|e| denied(e.to_string()))
    }

    /// Resolve the granted `exec` scope to absolute, existing program **files**
    /// for the `Execute` allow-list: a path-bearing entry is taken as-is (if it
    /// exists); a bare name is resolved against the exec search dirs. Canonicalized
    /// so the rule anchors the real inode. `All` => empty (exec stays ambient).
    ///
    /// **Git's own exec-path is folded in (#2630).** A grant that resolves to a
    /// trusted `git` binary lets git run `git worktree add` / `git commit` / …
    /// only because those porcelain commands themselves `execve` git's
    /// *internal* helpers (`git-branch`, `git-update-ref`, …) from `git
    /// --exec-path` — binaries the caller never named. A grant of `["git"]`
    /// alone therefore kernel-denies the helper exec and git fails with
    /// "cannot exec 'branch'". [`git_exec_path_binaries`] resolves that
    /// directory WITHOUT EVER EXECUTING the granted binary (round 2, #2630) —
    /// see its doc comment for why running `<git> --exec-path` in the
    /// resolving process was refused.
    fn resolve_exec_paths(scope: &Scope<String>) -> Vec<String> {
        let set = match scope {
            Scope::All => return Vec::new(),
            Scope::Only(set) => set,
        };
        let dirs = exec_search_dirs();
        let mut out = Vec::new();
        for entry in set {
            let candidate = if entry.contains('/') {
                let p = std::path::PathBuf::from(entry);
                p.exists().then_some(p)
            } else {
                dirs.iter()
                    .map(|d| std::path::Path::new(d).join(entry))
                    .find(|c| c.is_file())
            };
            if let Some(p) = candidate {
                if let Ok(canon) = p.canonicalize() {
                    out.extend(git_exec_path_binaries(&canon));
                    out.push(canon.to_string_lossy().into_owned());
                }
            }
        }
        out
    }

    /// Fixed, root-owned system directories a granted `git` binary must
    /// resolve into before its own exec-path helpers are folded in. A `git`
    /// living anywhere else — a repo-local `./tools/git`, a user's
    /// `~/bin/git`, anything found only via `$PATH` shadowing — gets no extra
    /// helpers, and (see [`git_exec_path_binaries`]) is never even opened,
    /// let alone executed, to make that determination.
    const GIT_TRUSTED_DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

    /// Where a git installed under one of [`GIT_TRUSTED_DIRS`] keeps its own
    /// helper binaries, relative to that directory's parent prefix
    /// (`/usr/bin/git` → prefix `/usr`). Covers the two conventional
    /// packaging layouts; a git installed anywhere else yields no candidate
    /// (fail closed — no helper, `git worktree add` fails exactly as before
    /// this fix, never wrongly permissive).
    const GIT_EXEC_PATH_CANDIDATES: &[&str] = &["lib/git-core", "libexec/git-core"];

    /// If `canon_git_bin` is a `git` binary that lives directly inside one of
    /// [`GIT_TRUSTED_DIRS`], return the single absolute, canonical path of
    /// `<exec-path>/git` — git re-executing *itself* under a different name
    /// (`fatal: cannot exec 'branch'` is `<exec-path>/git branch`) — PROVIDED
    /// that file is the same image as `canon_git_bin`. Empty otherwise.
    ///
    /// #2630 round 3 (bridle PR #407, finding P1-1): round 2 admitted every
    /// direct child of the exec-path directory — `git-remote-https`,
    /// `git-shell`, `git-daemon`, scripts — because they sit in a root-owned
    /// package directory. A root-owned directory is not operator authority
    /// for each file inside it; the operator granted execution of ONE binary
    /// (`git`), and the only reason it needs help at all is that it re-execs
    /// itself under an internal alias. So admit exactly that alias and
    /// nothing else. A worktree op that later needs an independent helper
    /// (`git-remote-https`, …) is a separate, explicit authority decision,
    /// not an automatic one.
    ///
    /// **Never executes `canon_git_bin` or anything else (#2630 round 2).**
    /// The only filesystem operations are `canonicalize`, `symlink_metadata`
    /// (via [`ancestry_is_root_owned_and_unwritable`]) and reading the two
    /// files' bytes to compare them ([`same_image`]) — no `exec`.
    ///
    /// Narrowing, on purpose:
    /// - `canon_git_bin` AND its full ancestor chain up to `/` must each be
    ///   root-owned and not group/other-writable
    ///   ([`ancestry_is_root_owned_and_unwritable`]) — not just its immediate
    ///   parent directory (P1-2): a writable *grandparent* could otherwise
    ///   let an attacker replace a trusted leaf without touching the leaf
    ///   itself.
    /// - `canon_git_bin`'s parent must canonicalize to EXACTLY one of
    ///   [`GIT_TRUSTED_DIRS`].
    /// - The candidate `<exec-path>/git` is independently subjected to the
    ///   same full-ancestry check, so a symlink or an intermediate writable
    ///   directory anywhere in ITS chain also fails closed.
    /// - The candidate must be [`same_image`] as `canon_git_bin` (same
    ///   inode, or identical content by `content_addressable::RawContentId`)
    ///   — proving it really is the same git, not merely another root-owned
    ///   file that happens to be named `git`.
    fn git_exec_path_binaries(canon_git_bin: &std::path::Path) -> Vec<String> {
        if canon_git_bin.file_name().and_then(|n| n.to_str()) != Some("git") {
            return Vec::new();
        }
        if !ancestry_is_root_owned_and_unwritable(canon_git_bin) {
            return Vec::new();
        }
        let Some(bin_dir) = canon_git_bin.parent() else {
            return Vec::new();
        };
        let is_trusted_dir = GIT_TRUSTED_DIRS.iter().any(|trusted| {
            std::fs::canonicalize(trusted).is_ok_and(|canon_trusted| canon_trusted == bin_dir)
        });
        if !is_trusted_dir {
            return Vec::new();
        }
        let Some(prefix) = bin_dir.parent() else {
            return Vec::new();
        };

        for candidate in GIT_EXEC_PATH_CANDIDATES {
            let Ok(exec_path_git) = prefix.join(candidate).join("git").canonicalize() else {
                continue;
            };
            if exec_path_git == *canon_git_bin {
                continue; // same path as the granted binary — nothing extra to add.
            }
            if !ancestry_is_root_owned_and_unwritable(&exec_path_git) {
                continue;
            }
            if same_image(canon_git_bin, &exec_path_git) {
                return vec![exec_path_git.to_string_lossy().into_owned()];
            }
        }
        Vec::new()
    }

    /// The subset of a filesystem object's identity the ancestry walk
    /// needs: owning uid and permission mode. Broken out as plain data so
    /// the walk itself ([`ancestry_passes`]) can be driven by either live
    /// `symlink_metadata` (production, via
    /// [`ancestry_is_root_owned_and_unwritable`]) or a synthetic lookup
    /// (tests) — bridle PR #407 review, round 3 finding 1: without this
    /// seam, the round-3 adversarial "writable grandparent" tests could
    /// pass with the RECURSIVE ancestor walk deleted outright, because they
    /// never constructed a real root-owned leaf and so never got past the
    /// leaf's own ownership check to exercise a grandparent at all.
    #[cfg(unix)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct OwnerMode {
        uid: u32,
        mode: u32,
    }

    /// The actual ancestry walk: `path` and every ancestor up to and
    /// including `/` must each pass `lookup` as root-owned (`uid == 0`) and
    /// not group/other-writable (`mode & 0o022 == 0`). `lookup` returning
    /// `None` (object doesn't exist / can't be inspected) fails closed.
    /// Both production ([`ancestry_is_root_owned_and_unwritable`], backed
    /// by live `symlink_metadata`) and the adversarial unit tests (backed
    /// by a synthetic `HashMap`) call this SAME function, so a test that
    /// passes is a claim about this exact traversal, not a parallel
    /// reimplementation of it that could drift from what production runs.
    #[cfg(unix)]
    fn ancestry_passes(
        path: &std::path::Path,
        lookup: &dyn Fn(&std::path::Path) -> Option<OwnerMode>,
    ) -> bool {
        let Some(meta) = lookup(path) else {
            return false;
        };
        if meta.uid != 0 || (meta.mode & 0o022) != 0 {
            return false;
        }
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => ancestry_passes(parent, lookup),
            _ => true,
        }
    }

    /// `true` iff `path` itself AND every ancestor directory up to and
    /// including `/` is owned by root (uid 0) and carries no group- or
    /// other-write bit. Unlike a single-directory check, this rejects a
    /// trusted-looking leaf sitting under a writable grandparent (P1-2):
    /// an attacker who can write `/usr` but not `/usr/bin` could otherwise
    /// replace `/usr/bin` itself with a symlink to attacker-controlled
    /// content, or (more directly) install a new sibling that later
    /// canonicalizes into the trusted set. `path` must already be
    /// canonicalized by the caller — this checks the object at that path,
    /// not what a symlink there might point to (`symlink_metadata`, not
    /// `metadata`, via [`ancestry_passes`]'s `lookup`), so a still-symlinked
    /// path fails closed.
    #[cfg(unix)]
    fn ancestry_is_root_owned_and_unwritable(path: &std::path::Path) -> bool {
        ancestry_passes(path, &|p| {
            use std::os::unix::fs::MetadataExt;
            std::fs::symlink_metadata(p).ok().map(|m| OwnerMode {
                uid: m.uid(),
                mode: m.mode(),
            })
        })
    }

    #[cfg(not(unix))]
    fn ancestry_is_root_owned_and_unwritable(_path: &std::path::Path) -> bool {
        false
    }

    /// `true` iff `a` and `b` are the same file (same device+inode), or —
    /// when they are distinct filesystem objects (e.g. a hardlink is not in
    /// use) — carry identical content, proven by comparing
    /// `content_addressable::RawContentId` over each file's bytes rather
    /// than a hand-rolled hash (repo doctrine: content identity goes through
    /// `content-addressable`, never bespoke). Never executes either file.
    #[cfg(unix)]
    fn same_image(a: &std::path::Path, b: &std::path::Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            if ma.dev() == mb.dev() && ma.ino() == mb.ino() {
                return true;
            }
        }
        let (Ok(ba), Ok(bb)) = (std::fs::read(a), std::fs::read(b)) else {
            return false;
        };
        content_addressable::RawContentId::from_content(&ba)
            == content_addressable::RawContentId::from_content(&bb)
    }

    #[cfg(not(unix))]
    fn same_image(_a: &std::path::Path, _b: &std::path::Path) -> bool {
        false
    }

    /// The directories a bare program name is resolved against: `$PATH` if set,
    /// else a conventional fallback. Used only to anchor the `Execute` allow-list
    /// (the spawn itself still resolves the program normally).
    fn exec_search_dirs() -> Vec<String> {
        if let Ok(path) = std::env::var("PATH") {
            let dirs: Vec<String> = path
                .split(':')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            if !dirs.is_empty() {
                return dirs;
            }
        }
        [
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/local/sbin",
            "/usr/sbin",
            "/sbin",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
    }

    /// The existing path roots a [`Scope`] grants: `All` => the whole tree
    /// (`/`); `Only(set)` => exactly those paths that exist (a non-existent path
    /// cannot anchor a Landlock rule and is skipped — safe, since its parent is
    /// ungranted, so access beneath it stays denied).
    fn scope_roots(scope: &Scope<String>) -> Vec<String> {
        match scope {
            Scope::All => vec!["/".to_string()],
            Scope::Only(set) => set
                .iter()
                .filter(|p| std::path::Path::new(p).exists())
                .cloned()
                .collect(),
        }
    }

    fn landlock_denied(e: impl std::fmt::Display) -> ToolError {
        ToolError::denied(format!("landlock: {e}"))
    }

    /// #2630 round 2 — `resolve_exec_paths`/`git_exec_path_binaries` never
    /// executes a grant-selected binary while computing the exec-path
    /// allow-list, and only trusts a directory whose live permissions it has
    /// checked.
    #[cfg(test)]
    mod git_exec_path_tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn unique_dir(tag: &str) -> std::path::PathBuf {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(0);
            let mut d = std::env::temp_dir();
            d.push(format!(
                "ab-2630-{}-{}-{}",
                tag,
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        fn write_executable_marker_script(path: &std::path::Path, marker: &std::path::Path) {
            std::fs::write(
                path,
                format!(
                    "#!/bin/sh\ntouch \"{}\"\necho /nonexistent/hostile-exec-path\n",
                    marker.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// The real system `git` (if present) resolves into its own
        /// `--exec-path`-equivalent directory purely from the fixed
        /// trusted-dir/candidate-suffix convention, with every admitted path
        /// actually living inside that one resolved directory.
        #[test]
        fn a_real_trusted_git_grant_admits_its_own_exec_path_helpers() {
            let Some(git) = ["/usr/bin/git", "/bin/git"]
                .into_iter()
                .find(|p| std::path::Path::new(p).exists())
            else {
                eprintln!("skipping: no git in a GIT_TRUSTED_DIRS location on this host");
                return;
            };
            let resolved = resolve_exec_paths(&Scope::only([git.to_string()]));
            assert!(
                resolved.iter().any(|p| p == git),
                "the git binary itself must still be admitted: {resolved:?}"
            );
            assert!(
                resolved.len() > 1,
                "git's own exec-path helpers must be folded in, not just the binary: {resolved:?}"
            );
        }

        /// A `git`-named binary OUTSIDE [`GIT_TRUSTED_DIRS`] — a repo-local
        /// `./tools/git`-style plant — gets NO extra helpers, and (the load-
        /// bearing assertion) is never executed while resolution decides
        /// that: a marker-writing stand-in script proves it was never run.
        #[test]
        fn a_git_outside_trusted_dirs_gets_no_helpers_and_is_never_executed() {
            let dir = unique_dir("outside-trusted");
            let marker = dir.join("was-executed");
            let fake_git = dir.join("git");
            write_executable_marker_script(&fake_git, &marker);

            let resolved =
                resolve_exec_paths(&Scope::only([fake_git.to_string_lossy().into_owned()]));

            assert!(
                !marker.exists(),
                "a git outside the trusted dirs must NEVER be executed during resolution"
            );
            let canon_fake_git = fake_git.canonicalize().unwrap();
            assert_eq!(
                resolved,
                vec![canon_fake_git.to_string_lossy().into_owned()],
                "an untrusted git must admit only itself, no helpers: {resolved:?}"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }

        // An ambient, attacker-controlled `GIT_EXEC_PATH` is irrelevant by
        // construction: neither `resolve_exec_paths` nor
        // `git_exec_path_binaries` calls `std::env::var`/`var_os` anywhere —
        // there is nothing in either function body that reads it. Round 2
        // kept a behavioral test here that mutated process-global
        // `GIT_EXEC_PATH`, which raced every other test in this binary with
        // no synchronization (bridle PR #407 review, finding 4); dropped
        // per the round-3 brief rather than moved to a child process, since
        // the static fact above is already the whole proof. `PATH` itself
        // is still read, for bare-name entries in the caller's own grant
        // set — pre-existing, unrelated to `GIT_EXEC_PATH`, and exercised by
        // `a_non_git_binary_elsewhere_is_not_admitted` below.

        /// A binary sitting in a sibling directory to git's real exec-path
        /// directory — never one of the fixed candidate suffixes — is not
        /// admitted just because a `git` grant is present.
        #[test]
        fn a_non_git_binary_elsewhere_is_not_admitted() {
            let Some(git) = ["/usr/bin/git", "/bin/git"]
                .into_iter()
                .find(|p| std::path::Path::new(p).exists())
            else {
                eprintln!("skipping: no git in a GIT_TRUSTED_DIRS location on this host");
                return;
            };
            let resolved = resolve_exec_paths(&Scope::only([git.to_string()]));
            assert!(
                !resolved
                    .iter()
                    .any(|p| p.contains("/elsewhere/") || p.contains("not-git-at-all")),
                "only the fixed exec-path candidate directories may be admitted: {resolved:?}"
            );
        }

        /// A directory that LOOKS like a git exec-path (matches a candidate
        /// suffix under a trusted git's prefix) but is writable by non-root
        /// must not be trusted — an attacker who can write there could plant
        /// a helper the kernel would then be told to allow.
        #[test]
        fn a_group_writable_candidate_directory_is_not_trusted() {
            let dir = unique_dir("writable-exec-dir");
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
            assert!(
                !ancestry_is_root_owned_and_unwritable(&dir),
                "a world-writable directory must never be trusted, regardless of owner"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// #2630 round 3 (P1-1): even the real trusted system git admits
        /// ONLY `<exec-path>/git` — never a sibling helper such as
        /// `git-shell`, `git-daemon`, a `git-remote-*`, or a packaging
        /// script — despite all of them sitting in the same root-owned
        /// exec-path directory. A root-owned directory authorizes the ONE
        /// file the operator granted (git re-executing itself), not every
        /// file that happens to live beside it.
        #[test]
        fn a_trusted_git_grant_admits_only_its_own_exec_path_alias_not_sibling_helpers() {
            let Some(git) = ["/usr/bin/git", "/bin/git"]
                .into_iter()
                .find(|p| std::path::Path::new(p).exists())
            else {
                eprintln!("skipping: no git in a GIT_TRUSTED_DIRS location on this host");
                return;
            };
            let resolved = resolve_exec_paths(&Scope::only([git.to_string()]));
            assert!(
                resolved.iter().all(|p| std::path::Path::new(p)
                    .file_name()
                    .and_then(|n| n.to_str())
                    == Some("git")),
                "every admitted path must be a `git`-named alias of the granted binary, \
                 never an independent helper: {resolved:?}"
            );
            assert!(
                resolved.len() <= 2,
                "at most the granted binary plus its one same-image exec-path alias: {resolved:?}"
            );
        }

        /// P1-2: a `git`-named file sitting in an OTHERWISE trusted exec-path
        /// directory, but reached only via a symlink whose target escapes
        /// that directory, must not be admitted — canonicalization alone
        /// (which resolves the symlink) is not the same as authenticating
        /// the ancestry of what it resolves to.
        #[test]
        fn an_exec_path_git_reached_via_an_escaping_symlink_is_not_trusted() {
            let dir = unique_dir("escaping-symlink");
            let outside = dir.join("outside.txt");
            std::fs::write(&outside, b"not really git\n").unwrap();
            let link = dir.join("git");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            // The escape is in what the ancestry check must catch even once
            // canonicalized: `outside` itself is not root-owned (it lives in
            // a plain tmp dir owned by the test's own uid), so the resolved
            // target fails the ancestry check regardless of the symlink.
            let canon = link.canonicalize().unwrap();
            assert!(
                !ancestry_is_root_owned_and_unwritable(&canon),
                "a symlink resolving outside a root-owned, unwritable ancestry must not be trusted: {canon:?}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// P1-2: `ancestry_is_root_owned_and_unwritable` walks the FULL
        /// ancestor chain, not just the immediate parent — a file can sit in
        /// a root-owned, unwritable leaf directory while a GRANDPARENT is
        /// attacker-writable, which round 2's single-directory check missed.
        #[test]
        fn a_writable_grandparent_defeats_trust_even_with_a_locked_down_leaf() {
            let base = unique_dir("writable-grandparent");
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o777)).unwrap();
            let leaf = base.join("locked-leaf");
            std::fs::create_dir(&leaf).unwrap();
            // The leaf itself may be arbitrarily locked down; it is still
            // reachable through a writable ancestor, so the whole chain must
            // fail closed. (uid checks are skipped when not running as an
            // owner able to chown; the writable-ancestor bit alone already
            // fails this on any uid.)
            let file = leaf.join("git");
            std::fs::write(&file, b"stand-in\n").unwrap();
            assert!(
                !ancestry_is_root_owned_and_unwritable(&file),
                "a writable grandparent must defeat trust even when the leaf directory is locked down"
            );
            let _ = std::fs::remove_dir_all(&base);
        }

        /// [`same_image`] must reject two DIFFERENT `git`-named files even
        /// when both would independently pass the ancestry check — content
        /// equality is what proves "the same git re-executing itself", not
        /// merely "also named git, also root-owned".
        #[test]
        fn same_image_rejects_distinct_content() {
            let dir = unique_dir("distinct-content");
            let a = dir.join("git-a");
            let b = dir.join("git-b");
            std::fs::write(&a, b"binary one\n").unwrap();
            std::fs::write(&b, b"binary two, totally different\n").unwrap();
            assert!(
                !same_image(&a, &b),
                "distinct content must never be treated as the same image"
            );
            std::fs::write(&b, b"binary one\n").unwrap();
            assert!(
                same_image(&a, &b),
                "identical content (different inode) must be recognized as the same image"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// #2630 round 4 (bridle PR #407 review, round 3 finding 1): drives
    /// [`ancestry_passes`] — the SAME traversal `ancestry_is_root_owned_and_
    /// unwritable` runs in production — through a synthetic ownership map
    /// instead of the real filesystem. Round 3's adversarial ancestry tests
    /// built their fixtures from plain, user-owned tmp files/dirs, so the
    /// leaf itself already failed the uid check before the walk ever
    /// reached a parent; deleting the recursive call entirely left those
    /// tests green. These tests hold every level but ONE constant between
    /// the positive and negative cases, so only the traversal itself can
    /// account for the difference.
    #[cfg(test)]
    mod ancestry_walk_tests {
        use super::*;
        use std::collections::HashMap;
        use std::path::PathBuf;

        fn root_owned_unwritable() -> OwnerMode {
            OwnerMode {
                uid: 0,
                mode: 0o755,
            }
        }

        /// `/`, `/a`, `/a/b`, `/a/b/git` — every level root-owned and
        /// unwritable. The baseline every negative test below perturbs at
        /// exactly one level.
        fn all_protected_chain() -> HashMap<PathBuf, OwnerMode> {
            [
                PathBuf::from("/"),
                PathBuf::from("/a"),
                PathBuf::from("/a/b"),
                PathBuf::from("/a/b/git"),
            ]
            .into_iter()
            .map(|p| (p, root_owned_unwritable()))
            .collect()
        }

        fn lookup_in(
            map: &HashMap<PathBuf, OwnerMode>,
        ) -> impl Fn(&std::path::Path) -> Option<OwnerMode> + '_ {
            move |p: &std::path::Path| map.get(p).copied()
        }

        const LEAF: &str = "/a/b/git";

        /// Positive control: an entirely root-owned, unwritable synthetic
        /// chain from the leaf to `/` passes.
        #[test]
        fn an_all_protected_synthetic_chain_passes() {
            let map = all_protected_chain();
            assert!(ancestry_passes(
                std::path::Path::new(LEAF),
                &lookup_in(&map)
            ));
        }

        /// The leaf and its immediate parent (`/a/b`) are UNCHANGED from the
        /// positive case; only the GRANDPARENT's owner differs. This can
        /// only fail if the walk actually recurses past the immediate
        /// parent — deleting the recursive call (round 3's gap) would leave
        /// this green.
        #[test]
        fn a_grandparent_owned_by_a_non_root_uid_fails_the_walk() {
            let mut map = all_protected_chain();
            map.insert(
                PathBuf::from("/a"),
                OwnerMode {
                    uid: 1000,
                    ..root_owned_unwritable()
                },
            );
            assert!(!ancestry_passes(
                std::path::Path::new(LEAF),
                &lookup_in(&map)
            ));
        }

        /// Same shape, but the grandparent differs ONLY in its write mode
        /// (still uid 0) — isolates the mode half of the check from the uid
        /// half, and again only fails if the walk reaches it.
        #[test]
        fn a_group_writable_grandparent_fails_the_walk() {
            let mut map = all_protected_chain();
            map.insert(
                PathBuf::from("/a"),
                OwnerMode {
                    mode: 0o775,
                    ..root_owned_unwritable()
                },
            );
            assert!(!ancestry_passes(
                std::path::Path::new(LEAF),
                &lookup_in(&map)
            ));
        }

        /// A path with no entry in the lookup at all (object doesn't exist)
        /// fails closed rather than defaulting to trusted.
        #[test]
        fn an_unresolvable_path_fails_closed() {
            let map = all_protected_chain();
            assert!(!ancestry_passes(
                std::path::Path::new("/a/b/does-not-exist"),
                &lookup_in(&map)
            ));
        }
    }

    #[cfg(test)]
    mod resolved_authority_tests {
        //! Adversarial tests for the Landlock conservative-bound projection: the
        //! confirmed escapes (E1 symlink root, E3 net:none io_uring) must resolve
        //! to `Unknown`/`Superset` and refuse through mesh admission — the honest
        //! upper bound, computed from the SAME routines `apply` uses.
        use super::*;
        use crate::{admit, empty_closure, AdmissionDecision, ResolvedScope};
        use std::os::unix::fs::symlink;

        fn fs_read_only(path: &str) -> Caveats {
            Caveats {
                fs_read: Scope::only([path.to_string()]),
                ..Caveats::top()
            }
        }

        /// E1, grounded: `grant_roots_are_object_stable` rejects a symlinked root
        /// (canonical target ≠ literal) and accepts a real canonical directory.
        #[test]
        fn object_stability_flags_symlinked_grant_roots() {
            let base = std::env::temp_dir().join(format!("ab-e1a-{}", std::process::id()));
            let real = base.join("real");
            let link = base.join("link");
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&real).unwrap();
            symlink("/", &link).unwrap();
            let real_canon = std::fs::canonicalize(&real)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            assert!(grant_roots_are_object_stable(&[real_canon]));
            assert!(!grant_roots_are_object_stable(&[link
                .to_str()
                .unwrap()
                .to_string()]));
            let _ = std::fs::remove_dir_all(&base);
        }

        /// #3 object-identity: a benign-LOOKING closure root that SYMLINKS into a
        /// harness-private store (`.newt`) poisons the axis → `Unknown` → admission
        /// refuses (`closure_is_harness_disjoint` rejects `Unknown`). A benign
        /// system alias whose canonical identity is NOT harness-private stays
        /// admissible — the default policy's merged-`/usr` loader/lib symlinks
        /// (e.g. `/lib`→`/usr/lib` on this host) must remain disjoint, proving we
        /// refuse on resolved OBJECT IDENTITY, not on the mere presence of a symlink.
        #[test]
        fn a_closure_root_resolving_into_harness_private_poisons_the_axis() {
            use std::sync::Arc;
            let dir = std::env::temp_dir().join(format!("ab-obj-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(".newt/ocap")).unwrap();
            let link = dir.join("innocent-substrate");
            symlink(dir.join(".newt/ocap"), &link).unwrap();
            let policy = crate::SandboxPolicy {
                base_read_paths: crate::PathList::from_defaults(&[link.to_str().unwrap()]),
                ..crate::SandboxPolicy::default()
            };
            let closure = LandlockSandbox::with_policy(Arc::new(policy))
                .runtime_closure(&fs_read_only("/tmp"));
            assert_eq!(
                closure.fs_read,
                ResolvedScope::Unknown,
                "a closure root whose canonical identity reaches .newt must poison the axis"
            );
            assert!(!crate::admitted::closure_is_harness_disjoint(&closure));
            // The default policy (benign merged-/usr symlinks) stays disjoint.
            let benign = LandlockSandbox::new().runtime_closure(&fs_read_only("/tmp"));
            assert!(
                crate::admitted::closure_is_harness_disjoint(&benign),
                "benign system aliases (loader/lib) must remain harness-disjoint"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// E1 end-to-end: a symlinked read grant (`sub -> /`) resolves `fs_read`
        /// to `Unknown`, so mesh admission refuses — the whole-tree-read escape
        /// can never admit.
        #[test]
        fn e1_symlinked_read_grant_resolves_unknown_and_refuses() {
            let base = std::env::temp_dir().join(format!("ab-e1b-{}", std::process::id()));
            let link = base.join("sub");
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            symlink("/", &link).unwrap();
            let delegated = fs_read_only(link.to_str().unwrap());
            let resolved = LandlockSandbox::new()
                .resolved_authority(&delegated, crate::StdioPosture::Unaudited);
            assert_eq!(resolved.fs_read, ResolvedScope::Unknown);
            assert!(matches!(
                admit(&resolved, &delegated, &empty_closure()),
                AdmissionDecision::Reject(_)
            ));
            let _ = std::fs::remove_dir_all(&base);
        }

        /// E3: under the DEFAULT `LandlockOnly` policy, `net:none` cannot be
        /// honestly bounded — io_uring UDP bypasses the `SYS_socket` seccomp deny
        /// and no io_uring floor is installed — so `resolved.net = Unknown` ⇒
        /// refuse. (The enforced case is `net:none` under `DenyDirect`, below.)
        #[test]
        fn e3_net_none_under_landlock_only_resolves_unknown_and_refuses() {
            let delegated = Caveats {
                net: Scope::only(Vec::<String>::new()),
                ..Caveats::top()
            };
            let resolved = LandlockSandbox::new()
                .resolved_authority(&delegated, crate::StdioPosture::Unaudited);
            assert_eq!(resolved.net, ResolvedScope::Unknown);
            assert!(matches!(
                admit(&resolved, &delegated, &empty_closure()),
                AdmissionDecision::Reject(_)
            ));
        }

        /// PR-1: `net:none` under `DenyDirect` IS honestly bounded — the seccomp
        /// socket()+io_uring deny closes the E3 io_uring bypass on top of
        /// Landlock's TCP deny — so `resolved.net` is the empty host set the grant
        /// names and admission ADMITS (enforced no-egress). This is what re-enables
        /// restricted-`net:none` confined operation faithfully.
        #[test]
        fn net_none_under_deny_direct_resolves_bounded_and_admits() {
            use std::sync::Arc;
            let delegated = Caveats {
                net: Scope::only(Vec::<String>::new()),
                ..Caveats::top()
            };
            let policy = crate::SandboxPolicy {
                child_network: crate::ChildNetworkPolicy::DenyDirect,
                ..crate::SandboxPolicy::default()
            };
            let resolved = LandlockSandbox::with_policy(Arc::new(policy))
                .resolved_authority(&delegated, crate::StdioPosture::Unaudited);
            assert_ne!(
                resolved.net,
                ResolvedScope::Unknown,
                "DenyDirect net:none must be BOUNDED (io_uring closed), not Unknown"
            );
            assert!(
                matches!(
                    admit(&resolved, &delegated, &empty_closure()),
                    AdmissionDecision::Admit
                ),
                "enforced net:none (DenyDirect) must admit"
            );
        }

        /// The conservative rule only bites RESTRICTED axes: an unrestricted grant
        /// (`All` everywhere) resolves `Unbounded` and admits.
        #[test]
        fn unrestricted_grant_admits() {
            let delegated = Caveats::top();
            let resolved = LandlockSandbox::new()
                .resolved_authority(&delegated, crate::StdioPosture::Unaudited);
            assert_eq!(resolved.net, ResolvedScope::Unbounded);
            assert_eq!(resolved.fs_read, ResolvedScope::Unbounded);
            assert!(matches!(
                admit(&resolved, &delegated, &empty_closure()),
                AdmissionDecision::Admit
            ));
        }

        /// A legit fs_read grant ADMITS: the base-read/bin substrate the ruleset
        /// adds is DECLARED by the runtime closure, so `resolved ⊑ delegated ∪
        /// closure`. WITHOUT the closure the same substrate is an undeclared
        /// widening and refuses — proving the closure is load-bearing, not cosmetic.
        #[test]
        fn a_legit_fs_read_grant_admits_via_the_declared_closure() {
            let dir = std::env::temp_dir().join(format!("ab-c-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let delegated = fs_read_only(dir.to_str().unwrap());
            let sb = LandlockSandbox::new();
            let resolved = sb.resolved_authority(&delegated, crate::StdioPosture::Unaudited);
            let closure = sb.runtime_closure(&delegated);
            assert!(
                matches!(
                    admit(&resolved, &delegated, &closure),
                    AdmissionDecision::Admit
                ),
                "a legit grant must admit once the substrate is declared; resolved={resolved:?}"
            );
            assert!(
                matches!(
                    admit(&resolved, &delegated, &empty_closure()),
                    AdmissionDecision::Reject(_)
                ),
                "without the closure the base-read additions are an undeclared widening"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// The Landlock runtime closure declares only system substrate — disjoint
        /// from harness-private authority; a closure reaching the OCAP store fails.
        #[test]
        fn runtime_closure_is_harness_disjoint() {
            let delegated = fs_read_only("/tmp");
            let closure = LandlockSandbox::new().runtime_closure(&delegated);
            assert!(crate::admitted::closure_is_harness_disjoint(&closure));
            let mut bad = closure;
            if let ResolvedScope::Bounded { concrete, .. } = &mut bad.fs_read {
                concrete.insert("/home/agent/.newt/ocap/state".to_string());
            }
            assert!(!crate::admitted::closure_is_harness_disjoint(&bad));
        }
    }
}

#[cfg(all(target_os = "macos", feature = "macos-seatbelt"))]
mod seatbelt_impl {
    use super::{Sandbox, SandboxKind};
    use crate::{Caveats, SandboxPolicy, Scope, ToolError, ToolResult};
    use std::path::Path;
    use std::sync::Arc;

    /// Whether the Mach-service deputy audit for the zero floor is complete
    /// (agent-bridle#405 D4). While `Incomplete`, every restricted Seatbelt net
    /// shape projects `Unknown` and admission refuses it — the fail-closed
    /// posture of ADR 0015's E4 ruling. Only a deputy-complete native proof
    /// (every reachable ambient IPC route shown closed, positive controls
    /// included) may flip [`MACH_DEPUTY_AUDIT`] to `Complete`. The constant
    /// controls ONLY the resolved-authority projection (the L3 scope bound);
    /// `report.rs`'s Seatbelt `net` arm reads it too (via
    /// `seatbelt_mach_deputy_audit_complete`/`seatbelt_net_kernel_witness`) so
    /// the L3 projection and the L4 strength report promote together, never
    /// one without the other.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum MachDeputyAudit {
        Incomplete,
        Complete,
    }

    /// The audit state this build ships. **Complete** (agent-bridle#405, ADR
    /// 0015 amendment E6, 2026-10-01; narrowed by the round-2 review,
    /// agent-bridle#416): the zero floor closes every *named* Mach lookup, and
    /// the measured legs of the channel sweep — unix-domain sockets,
    /// `open(1)`/LaunchServices, Darwin notifications, pasteboard,
    /// `iokit-open`, `sysctl-write`, a write-class `file-ioctl`, XPC beyond
    /// `mach-lookup`, and AppleEvents — are each shown closed under an
    /// UNPRIVILEGED probing process (full evidence table: ADR 0015 amendment
    /// E6). `process-info`/`signal` and the shared-memory/file-drop surface
    /// are stated as explicit ACCEPTED LIMITS of a direct-egress claim (they
    /// carry no network authority), not as proof the deputy set is exhaustive;
    /// "no ambient relay found" on the probed host is an observation about
    /// that host, not a closure proof. This governs ONLY the deny-all,
    /// zero-`mach:`-grant shape, and ONLY when the caller is unprivileged and
    /// the spawn's stdio is pipe/null-only — [`super::seatbelt_net_kernel_witness`]'s
    /// other parameters, enforced at `caller_is_root`/the spawn's declared
    /// [`crate::StdioPosture`]. A named grant, a loopback scope, a remote-host
    /// allowlist, a root-owned caller, or unaudited stdio are all unaffected
    /// and stay `Unknown`/`Advisory`.
    pub(super) const MACH_DEPUTY_AUDIT: MachDeputyAudit = MachDeputyAudit::Complete;

    /// The conservative network projection for `effective` under `audit`.
    /// Pure. `All` is ambient (`Unbounded`). Every restricted shape is
    /// `Unknown` while the audit is incomplete. Under a complete audit the
    /// direct-denied shapes (`net:none`, `unix:`-only, `mach:`-only, or mixed)
    /// resolve to exactly what the profile permits: the `unix:` endpoints as
    /// concrete entries and each `mach:` grant as its named class — never `∅`
    /// while a grant is present, so admission compares the grant honestly.
    /// Loopback and remote-host shapes stay `Unknown` under either state:
    /// their Mach lookup is ambient (no floor), so nothing bounds a deputy.
    pub(super) fn seatbelt_net_projection(
        effective: &Caveats,
        audit: MachDeputyAudit,
    ) -> crate::ResolvedScope {
        use crate::ResolvedScope as Rs;
        match &effective.net {
            Scope::All => Rs::Unbounded,
            Scope::Only(_) if audit == MachDeputyAudit::Incomplete => Rs::Unknown,
            Scope::Only(set) if super::net_direct_denied(effective) => {
                let Ok(grants) = super::mach_service_grants(effective) else {
                    return Rs::Unknown;
                };
                let concrete = Rs::concrete(set.iter().filter(|h| h.starts_with("unix:")).cloned());
                grants.iter().fold(concrete, |acc, g| {
                    acc.union(&Rs::class(super::seatbelt_mach_service_class(g)))
                })
            }
            Scope::Only(_) => Rs::Unknown,
        }
    }

    /// The macOS sandbox wrapper. We invoke it by **absolute path** (never via
    /// `PATH`) so the boundary cannot be shadowed by a `sandbox-exec` planted
    /// earlier in a caller's `PATH`. `sandbox-exec(1)` is deprecated-but-present
    /// on stock macOS; using it keeps the boundary FFI-free, which core requires
    /// (`unsafe_code = "forbid"`).
    const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

    // Read-side base allow-list (subpaths): the system/loader paths a
    // dynamically-linked Mach-O binary must read to *start and run* — the dynamic
    // linker and dyld shared cache (under `/System`, incl. the Cryptex volume),
    // system dylibs/frameworks, the binaries themselves, the name-service and
    // locale config (`/private/etc`, the real target of `/etc`), the dyld closure
    // db, and the `/dev` essentials. Added whenever `fs_read` is confined,
    // alongside the literal root entry, so a *permitted* program still loads while
    // user data outside scope stays unreadable. Non-existent entries are dropped
    // during canonicalization, so extra entries are harmless across macOS layouts
    // (verified on Apple Silicon: `grep`/`cat`/`cp` load read-confined). The list
    // now lives in `SandboxPolicy::base_read_paths` (config.rs), whose default is
    // macOS-specific on this platform (I5-B, #144).

    /// `true` if this host can enforce a Seatbelt profile — i.e. the
    /// `sandbox-exec` wrapper is present. The wrapper itself is the boundary, so
    /// its presence is the capability (the analog of `landlock_is_supported`).
    #[must_use]
    pub fn seatbelt_is_supported() -> bool {
        Path::new(SANDBOX_EXEC).exists()
    }

    /// `true` when this process's effective UID is 0 (root). ADR 0015 E6's
    /// probes all ran unprivileged (agent-bridle#416 round-2 review, item 2).
    /// Reads the real ambient UID via `rustix` (a safe wrapper, so core stays
    /// `forbid(unsafe_code)`) — deliberately NOT a pure/parameterized
    /// predicate, unlike [`super::seatbelt_net_kernel_witness`], which takes
    /// the already-read value as a plain bool specifically so its logic stays
    /// testable without depending on the actual privilege of whatever process
    /// runs the suite.
    pub(super) fn caller_is_root() -> bool {
        rustix::process::geteuid().is_root()
    }

    /// A real, kernel-enforced Seatbelt sandbox (macOS).
    ///
    /// **The `fs_write` and `fs_read` axes** — the same *axes* the Linux Landlock
    /// backend governs (not necessarily the same path-level strictness; see
    /// below). Confinement is applied by wrapping the spawned program in
    /// `sandbox-exec -p <profile>`, where the SBPL profile is generated from the
    /// effective [`Caveats`] (see [`seatbelt_profile`]): writes are denied
    /// outside the granted `fs_write` roots, and — when `fs_read` is restricted —
    /// reads are denied outside the granted roots plus the loader/system base
    /// list. When direct network is denied (`net:none`, or `unix:`/`mach:`
    /// entries only) it kernel-denies the child's direct socket operations and
    /// installs the **zero** Mach-lookup floor (agent-bridle#405): every named
    /// Mach service is denied unless the operator granted it by name with a
    /// `mach:<service>` token — nothing ambient. With no grant the floor
    /// closes every named Mach lookup; a grant re-opens the named service,
    /// deputy or not. Neither is a deputy-complete proof (other ambient IPC is
    /// not certified), so restricted network authority remains held at admission
    /// (`MACH_DEPUTY_AUDIT`). A non-empty `net` host allowlist is not
    /// expressible in SBPL (it filters by socket, not hostname) and stays
    /// advisory.
    ///
    /// **The `exec` axis** — when restricted, the profile emits
    /// `(deny process-exec*)` and re-allows exactly the granted programs (resolved
    /// to absolute paths). Because `process-exec*` is a kernel-checked operation
    /// applied to the confined process *and everything it spawns*, this confines
    /// the program's **interior** execs — the L3 gap a path allow-list alone
    /// cannot reach. Unlike Landlock, no seccomp backstop is needed: the loader
    /// trampoline (`dyld TARGET`) is itself a governed `process-exec`, and the
    /// `mmap(PROT_EXEC)` read-as-code path is closed by Apple-Silicon hardware
    /// W^X + code signing — so "the readable set equals the runnable set" (the
    /// fact that forces the Linux seccomp filter) does **not** hold here. The axis
    /// is therefore honestly reported `Kernel` (ADR 0014; agent-bridle#31/#57).
    ///
    /// Read confinement here is **content-level**: file *metadata* (stat,
    /// existence, directory traversal) stays ambient so binaries can load through
    /// symlink ancestors, and the system read base (the configured `base_read_paths`, incl.
    /// `/private/etc`) is broadly readable — looser than Landlock's file-level
    /// `/etc` allow-list, but the protected resource (out-of-scope file
    /// *contents*, the exfil threat) is denied identically. macOS keeps user
    /// secrets in the Keychain and `$HOME`, not `/etc`.
    ///
    /// Unlike Landlock's per-thread `restrict_self`, Seatbelt confinement is
    /// carried by the wrapper process and inherited by the child, so
    /// [`Sandbox::apply`] is a no-op and the boundary lives entirely in
    /// [`Sandbox::command_prefix`].
    #[derive(Debug, Default, Clone)]
    pub struct SeatbeltSandbox {
        /// The read base this backend's SBPL profile allows (I5-B).
        policy: Arc<SandboxPolicy>,
    }

    /// The Mach-lookup leg paired with Seatbelt's direct-network deny for
    /// `net:none`. Production always uses `Closed`; the test-only ambient mode
    /// exists solely to characterize the incremental effect of the Mach floor.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum NetNoneMachFloor {
        Closed,
        #[cfg(test)]
        AmbientCharacterization,
    }

    impl SeatbeltSandbox {
        /// Construct with the built-in defaults (today's read base).
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Construct configured with an operator-supplied [`SandboxPolicy`].
        #[must_use]
        pub fn with_policy(policy: Arc<SandboxPolicy>) -> Self {
            Self { policy }
        }

        fn wrapper_prefix(
            &self,
            effective: &Caveats,
            mach_floor: NetNoneMachFloor,
        ) -> ToolResult<Vec<String>> {
            if !seatbelt_is_supported() {
                return Err(ToolError::denied(
                    "macOS seatbelt: /usr/bin/sandbox-exec is unavailable; cannot confine",
                ));
            }
            Ok(vec![
                SANDBOX_EXEC.to_string(),
                "-p".to_string(),
                seatbelt_profile_with(
                    effective,
                    &self.policy.base_read_paths.resolve(),
                    &self.policy.device_sink_paths.resolve(),
                    &unix_socket_paths(effective)?,
                    &super::mach_service_grants(effective)?,
                    mach_floor,
                ),
            ])
        }

        /// Test-only typed seam for the E4 differential. It accepts exactly
        /// `net:none` with every other caveat unrestricted and emits the same
        /// direct-network deny as production while deliberately leaving Mach
        /// lookup ambient. No profile text is parsed or rewritten.
        #[cfg(test)]
        pub(super) fn net_none_ambient_mach_prefix(
            &self,
            effective: &Caveats,
        ) -> ToolResult<Vec<String>> {
            let expected = Caveats {
                net: Scope::none(),
                ..Caveats::top()
            };
            if effective != &expected {
                return Err(ToolError::denied(
                    "the ambient-Mach characterization requires exactly net:none and otherwise-top caveats",
                ));
            }
            self.wrapper_prefix(effective, NetNoneMachFloor::AmbientCharacterization)
        }
    }

    impl Sandbox for SeatbeltSandbox {
        fn kind(&self) -> SandboxKind {
            SandboxKind::Seatbelt
        }

        /// Deliberately partial Seatbelt projection. The filesystem and exec axes
        /// retain the legacy caveats-grain/verbatim projection so this change does
        /// not claim ruleset-grain fidelity that has not been established. Network
        /// follows [`seatbelt_net_projection`] under the shipped
        /// [`MACH_DEPUTY_AUDIT`]: unrestricted authority is honestly ambient, and
        /// every restricted network scope remains `Unknown` (refused before spawn)
        /// until the deputy audit is complete AND this specific spawn's declared
        /// [`crate::StdioPosture`]/caller privilege back it. The zero Mach floor and
        /// the named `mach:` grants in the generated profile are real kernel rules,
        /// but they are not used to promote restricted network authority to a
        /// bounded claim on their own (agent-bridle#405 D4: fail closed first).
        fn resolved_authority(
            &self,
            effective: &Caveats,
            stdio: crate::StdioPosture,
        ) -> crate::ResolvedAuthority {
            let mut resolved = crate::ResolvedAuthority::from_delegated(effective);
            // A root-owned process, or a spawn whose stdio is not declared
            // audited, narrows straight back to `Incomplete` — the SAME
            // `Unknown`/held-for-admission treatment an actually incomplete audit
            // already gets (agent-bridle#416 round-2 review, items 1/2: the
            // probes behind `MACH_DEPUTY_AUDIT` all ran unprivileged with
            // pipe/null-only stdio, so nothing backs a claim about a root
            // process's privileged kernel-control surface, or about a launch
            // whose stdio could already be a connected endpoint Seatbelt's own
            // socket rules never see). This is the L3 admission gate, not merely
            // a strength label (round-3 review): it must share the SAME
            // preconditions `seatbelt_net_kernel_witness` already requires at
            // L4, or an unprivileged net:none spawn with unaudited stdio resolves
            // a named `Bounded(∅)` bound here regardless of what L4 reports, and
            // a non-Kernel floor (e.g. the shipped `EnforcementFloor::DEFAULT`,
            // which accepts Advisory net) admits it on that bound alone.
            let audit = if caller_is_root() || stdio != crate::StdioPosture::Audited {
                MachDeputyAudit::Incomplete
            } else {
                MACH_DEPUTY_AUDIT
            };
            resolved.net = seatbelt_net_projection(effective, audit);
            resolved
        }

        /// The Seatbelt closure declares, on the net axis, exactly the named
        /// classes its profile re-allows for the operator's `mach:` grants — the
        /// bridge that lets `resolved.net` (which names the grant as a class)
        /// admit as a `Subset` of `delegated ∪ closure` once the projection is
        /// bounded. It adds nothing the caveats did not name: each class is
        /// derived from a grant token in `effective.net`, never from a built-in
        /// list. Nothing on any other axis.
        fn runtime_closure(&self, effective: &Caveats) -> crate::ResolvedAuthority {
            let mut closure = crate::empty_closure();
            if let Ok(grants) = super::mach_service_grants(effective) {
                closure.net = grants.iter().fold(crate::ResolvedScope::empty(), |acc, g| {
                    acc.union(&crate::ResolvedScope::class(
                        super::seatbelt_mach_service_class(g),
                    ))
                });
            }
            closure
        }

        fn apply(&self, _effective: &Caveats) -> ToolResult<()> {
            // Deliberate no-op: Seatbelt confines via the `sandbox-exec` wrapper
            // (see `command_prefix`), not by restricting the calling thread. The
            // boundary is the wrapped spawn.
            Ok(())
        }

        fn command_prefix(&self, effective: &Caveats) -> ToolResult<Vec<String>> {
            let unix_sockets = unix_socket_paths(effective)?;
            let mach_grants = super::mach_service_grants(effective)?;
            if !unix_sockets.is_empty()
                && !super::net_direct_denied(effective)
                && !super::net_loopback_only(effective)
            {
                return Err(ToolError::denied(
                    "Unix socket grants with remote hosts require the managed egress proxy",
                ));
            }
            // A `mach:` grant only has meaning where a Mach floor is installed —
            // the direct-denied shapes. Under a loopback or remote-host shape the
            // profile leaves Mach lookup ambient (no floor, so nothing to re-open);
            // refuse rather than accept a grant the fence would not act on.
            if !mach_grants.is_empty() && !super::net_direct_denied(effective) {
                return Err(ToolError::denied(
                    "mach: service grants apply only to a network-denied scope \
                     (net:none or unix:/mach: entries only)",
                ));
            }
            // Nothing on a governed axis (fs, a direct-network floor, exact Unix
            // endpoints, Mach service grants, or a restricted exec allow-list) =>
            // nothing to confine; run unwrapped (coarse honesty falls to `None`
            // upstream, and the per-axis report omits unrestricted axes).
            if !super::restricts_fs(effective)
                && !super::net_direct_denied(effective)
                && !super::net_loopback_only(effective)
                && unix_sockets.is_empty()
                && mach_grants.is_empty()
                && !super::restricts_exec(effective)
            {
                return Ok(Vec::new());
            }
            // Production always installs the zero Mach floor plus the named
            // grants. The network projection follows `MACH_DEPUTY_AUDIT`.
            self.wrapper_prefix(effective, NetNoneMachFloor::Closed)
        }
    }

    /// Generate the SBPL profile for `effective`. **Pure** (modulo path
    /// canonicalization against the real filesystem); no spawning.
    ///
    /// Model (the macOS analog of Landlock handling only the write/read access
    /// rights and leaving the rest ambient): start from `(allow default)` so
    /// unhandled operations — `exec`, `network`, mach lookups a normal process
    /// needs — stay ambient, then `(deny file-write*)` / `(deny file-read*)` for
    /// a restricted axis and re-allow exactly the granted roots (canonicalized,
    /// so `/tmp` → `/private/tmp` matches). An empty `fs_write` scope emits the
    /// deny with no re-allow — every write denied. SBPL evaluates last-match-wins,
    /// so the trailing allow-roots override the deny.
    // Convenience over the built-in read base — **tests only** (production uses
    // `command_prefix` → `seatbelt_profile_with` with the configured
    // `SandboxPolicy::base_read_paths`, I5-B #144).
    #[cfg(test)]
    #[must_use]
    pub fn seatbelt_profile(effective: &Caveats) -> String {
        let policy = SandboxPolicy::default();
        seatbelt_profile_with(
            effective,
            &policy.base_read_paths.resolve(),
            &policy.device_sink_paths.resolve(),
            &unix_socket_paths(effective).expect("valid Unix socket grants in profile fixture"),
            &super::mach_service_grants(effective).expect("valid mach: grants in profile fixture"),
            NetNoneMachFloor::Closed,
        )
    }

    /// SBPL profile builder, parameterized on the read base (`base_read`), the
    /// always-writable device sinks (`sinks`, #1220), the exact Unix endpoint
    /// paths (`unix_sockets`) already validated by [`unix_socket_paths`], and
    /// the Mach service names (`mach_grants`) already validated by
    /// [`super::mach_service_grants`].
    #[must_use]
    fn seatbelt_profile_with(
        effective: &Caveats,
        base_read: &[String],
        sinks: &[String],
        unix_sockets: &[String],
        mach_grants: &[String],
        mach_floor: NetNoneMachFloor,
    ) -> String {
        let mut p = String::from("(version 1)\n(allow default)\n");

        // fs_write: deny writes, then re-allow the granted roots.
        if let Scope::Only(_) = &effective.fs_write {
            p.push_str("(deny file-write*)\n");
            let roots = confined_roots(&effective.fs_write);
            if !roots.is_empty() {
                p.push_str("(allow file-write*");
                for r in &roots {
                    p.push_str(&format!(" (subpath {})", sbpl_string(r)));
                }
                p.push_str(")\n");
            }
            // #1220: device sinks stay write-openable under confinement —
            // `literal` (not `subpath`): each is a single character device.
            if !sinks.is_empty() {
                p.push_str("(allow file-write*");
                for s in sinks {
                    p.push_str(&format!(" (literal {})", sbpl_string(s)));
                }
                p.push_str(")\n");
            }
        }

        // fs_read: deny reads, then re-allow. `(allow file-read-metadata)`
        // permits path *traversal* and `stat` everywhere — without it, reaching
        // an in-scope file through a symlink ancestor (`/tmp`, `/var`, `/etc` →
        // `/private/…`) is denied at the symlink lookup. Metadata reveals only
        // existence/size, never **content**; the data axis stays confined to the
        // loader/system base, the root directory *entry* (dyld reads `/` itself),
        // and the granted roots — so a permitted program loads and reads in-scope
        // files while out-of-scope file *contents* (the exfil threat) stay denied.
        if let Scope::Only(_) = &effective.fs_read {
            p.push_str("(deny file-read*)\n");
            p.push_str("(allow file-read-metadata)\n");
            p.push_str("(allow file-read* (literal \"/\")");
            for base in base_read {
                if let Some(c) = canonical_path(base) {
                    p.push_str(&format!(" (subpath {})", sbpl_string(&c)));
                }
            }
            for r in confined_roots(&effective.fs_read) {
                p.push_str(&format!(" (subpath {})", sbpl_string(&r)));
            }
            p.push_str(")\n");
        }

        // net: SBPL can name only `*`/`localhost` + ports as a remote (an
        // arbitrary IP is rejected: "host must be * or localhost"; ADR 0015), so a
        // general host allowlist is inexpressible and left ambient (reported
        // advisory, never silently dropped). The two policies it *can* enforce:
        //   • direct-denied scope (empty, or `unix:`/`mach:` entries only) →
        //     `(deny network*)`: the child's direct socket operations are
        //     kernel-denied. Production also installs the ZERO Mach-lookup floor
        //     below (agent-bridle#405): every named Mach service is denied unless
        //     the operator granted it by name. Nothing is ambient. With no grant
        //     this closes every named Mach lookup; it is not a deputy-complete
        //     proof, so restricted network admission remains held
        //     (`MACH_DEPUTY_AUDIT`).
        //   • loopback-only allowlist → deny all, then re-allow the loopback
        //     interface (`localhost` = 127.0.0.1 + ::1). The process's own off-box
        //     socket egress stays kernel-denied; the exact loopback host is narrowed
        //     by admission. Last-match-wins, so the allow overrides.
        if super::net_direct_denied(effective) {
            p.push_str("(deny network*)\n");
            match mach_floor {
                NetNoneMachFloor::Closed => {
                    // Zero floor: default-deny named Mach lookup, then re-allow
                    // exactly the operator's `mach:` grants (sorted, deduped by
                    // `mach_service_grants`). No grant ⇒ the deny stands alone.
                    // The former ambient compatibility list is now
                    // `MACH_SERVICE_CANDIDATES` — documented, never emitted.
                    p.push_str("(deny mach-lookup)\n");
                    if !mach_grants.is_empty() {
                        p.push_str("(allow mach-lookup");
                        for name in mach_grants {
                            p.push_str(&format!(" (global-name {})", sbpl_string(name)));
                        }
                        p.push_str(")\n");
                    }
                }
                #[cfg(test)]
                NetNoneMachFloor::AmbientCharacterization => {}
            }
        } else if super::net_loopback_only(effective) {
            p.push_str("(deny network*)\n");
            p.push_str("(allow network* (remote ip \"localhost:*\"))\n");
            // The server side of the same interface: bind, listen and accept
            // on a loopback address (a test suite's mock server). `remote ip`
            // governs only the peer, so without this `bind` is EPERM. Off-box
            // stays denied: both rules name only `localhost`.
            p.push_str("(allow network-bind network-inbound (local ip \"localhost:*\"))\n");
        }
        // Exact Unix endpoints (#385 upstream, forward-ported): each granted
        // socket path is an exact outbound exception, never an `(allow
        // network*)` widening — `unix_socket_paths` already validated each
        // path is an existing, canonical, symlink-free socket.
        for path in unix_sockets {
            p.push_str(&format!(
                "(allow network-outbound (literal {}))\n",
                sbpl_string(path)
            ));
        }

        // exec: deny *all* further execs, then re-allow exactly the granted
        // programs (resolved to absolute, canonical paths). `process-exec*` is
        // kernel-checked on the confined process AND everything it spawns, so this
        // is the `exec` axis at interior grain — no seccomp backstop needed (the
        // dyld trampoline is itself a governed `process-exec`, and `mmap(PROT_EXEC)`
        // read-as-code is closed by hardware W^X + code signing; ADR 0014). An
        // empty/unresolvable grant emits the deny with no re-allow — every exec
        // (including the wrapped program's own launch) denied: fail-closed, never
        // ambient. SBPL is last-match-wins, so the trailing allow overrides.
        if let Scope::Only(_) = &effective.exec {
            p.push_str("(deny process-exec*)\n");
            let targets = resolve_exec_targets(&effective.exec);
            if !targets.is_empty() {
                p.push_str("(allow process-exec*");
                for t in &targets {
                    p.push_str(&format!(" (literal {})", sbpl_string(t)));
                }
                p.push_str(")\n");
            }
        }

        p
    }

    /// Validate and resolve every `unix:<path>` token in `effective.net` to its
    /// exact canonical path, for an `(allow network-outbound (literal …))`
    /// exception. No lexical aliases, symlinks, missing endpoints, or patterns
    /// — matching is to this exact existing pathname, never its parent or a
    /// socket subtree: a granted `unix:/tmp/svc.sock` must equal its own
    /// canonicalization (rejecting a symlinked or relative name) and must
    /// already exist as a socket (rejecting a missing or ordinary file).
    fn unix_socket_paths(effective: &Caveats) -> ToolResult<Vec<String>> {
        use std::os::unix::fs::FileTypeExt;
        let Scope::Only(names) = &effective.net else {
            return Ok(Vec::new());
        };
        names
            .iter()
            .filter_map(|name| name.strip_prefix("unix:"))
            .map(|name| {
                let path = Path::new(name);
                let valid = path.is_absolute()
                    && !name
                        .chars()
                        .any(|c| c.is_control() || matches!(c, '*' | '?' | '[' | ']'))
                    && std::fs::canonicalize(path)
                        .ok()
                        .and_then(|p| p.to_str().map(str::to_owned))
                        .as_deref()
                        == Some(name)
                    && std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket());
                if valid {
                    Ok(name.to_owned())
                } else {
                    Err(ToolError::denied(
                        "Unix endpoint grant must name an existing canonical absolute socket \
                         without symlinks or patterns",
                    ))
                }
            })
            .collect()
    }

    /// The canonicalized, existing roots a restricted [`Scope`] grants. A path
    /// that cannot be resolved to any existing ancestor is dropped (it cannot
    /// anchor a rule — safe, since its parent is ungranted, so access beneath it
    /// stays denied). `All` yields nothing (callers only pass a restricted axis).
    fn confined_roots(scope: &Scope<String>) -> Vec<String> {
        let Scope::Only(set) = scope else {
            return Vec::new();
        };
        let mut roots: Vec<String> = set.iter().filter_map(|p| canonical_path(p)).collect();
        roots.sort();
        roots.dedup();
        roots
    }

    /// System binary directories searched to resolve a **bare-name** `exec` grant
    /// (e.g. `["git"]`) to absolute path(s) for the `process-exec*` allow-list.
    /// SIP-protected, read-only system locations — a trustworthy pin. Bare names
    /// resolve through this *fixed* list, never the ambient `$PATH` (ADR 0014 /
    /// ADR 0011 D5), so a binary planted earlier on a caller's `$PATH` cannot
    /// widen the kernel allow-list. An absolute-path grant is honored verbatim
    /// (then canonicalized); a basename collision outside these dirs is not.
    const TRUSTED_EXEC_DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

    /// Resolve a restricted `exec` [`Scope`] to the absolute, canonical program
    /// paths that anchor the SBPL `(allow process-exec* (literal …))` rules. The
    /// kernel matches `process-exec` against the *resolved* path of the exec
    /// target, so each grant must become a realpath: an absolute grant is
    /// canonicalized; a bare name is resolved against [`TRUSTED_EXEC_DIRS`] (each
    /// existing hit included, mirroring admission's basename semantics in
    /// [`crate::context`] but pinned to trusted dirs). A relative-path or
    /// unresolvable grant is dropped — it cannot anchor a rule, so the program
    /// stays denied (fail-closed). `All` yields nothing (callers pass a restricted
    /// axis). Results are sorted+deduped so the emitted profile is deterministic.
    fn resolve_exec_targets(scope: &Scope<String>) -> Vec<String> {
        let Scope::Only(set) = scope else {
            return Vec::new();
        };
        let canon_file = |path: &Path, out: &mut Vec<String>| {
            if let Ok(c) = std::fs::canonicalize(path) {
                if c.is_file() {
                    out.push(c.to_string_lossy().into_owned());
                }
            }
        };
        let mut out: Vec<String> = Vec::new();
        for token in set {
            if token.starts_with('/') {
                // Absolute grant: honored verbatim (canonicalized, must exist).
                canon_file(Path::new(token), &mut out);
            } else if !token.contains('/') {
                // Bare name: resolve against the fixed trusted system dirs only.
                for dir in TRUSTED_EXEC_DIRS {
                    canon_file(&Path::new(dir).join(token), &mut out);
                }
            }
            // else: a relative path grant cannot anchor a kernel rule safely — drop.
        }
        // Apple's `/bin/sh` is a small launcher (a distinct binary from
        // `/bin/bash`) that re-execs `/bin/bash` as its interpreter *variant* at
        // startup. That re-exec is itself a kernel-checked `process-exec`, so a
        // granted `/bin/sh` is UNRUNNABLE under a restricted exec axis unless its
        // variant `/bin/bash` is also on the allow-list — the child dies at its own
        // startup re-exec ("Failed to exec /bin/bash as variant for /bin/sh") before
        // running a single line, so a confined `sh -c '…'` returns immediately
        // (agent-bridle#318). Granting the variant is faithful to the grant (macOS's
        // `sh` *is* `bash`), never a widening: it only makes a granted shell run.
        if out.iter().any(|p| p == "/bin/sh") {
            canon_file(Path::new("/bin/bash"), &mut out);
        }
        out.sort();
        out.dedup();
        out
    }

    /// Resolve `p` to an absolute, symlink-free path suitable for `(subpath …)`
    /// matching, which the kernel performs against the *resolved* path (so a
    /// granted `/tmp/x` must become `/private/tmp/x` or it never matches). If the
    /// leaf does not yet exist, canonicalize the longest existing ancestor and
    /// re-append the remainder. `None` if not even an ancestor resolves.
    fn canonical_path(p: &str) -> Option<String> {
        let path = Path::new(p);
        if let Ok(c) = std::fs::canonicalize(path) {
            return Some(c.to_string_lossy().into_owned());
        }
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut cur = path;
        while let Some(parent) = cur.parent() {
            if let Some(name) = cur.file_name() {
                tail.push(name.to_owned());
            }
            if let Ok(c) = std::fs::canonicalize(parent) {
                let mut resolved = c;
                for seg in tail.iter().rev() {
                    resolved.push(seg);
                }
                return Some(resolved.to_string_lossy().into_owned());
            }
            cur = parent;
        }
        None
    }

    /// Quote `s` as an SBPL string literal, escaping `\` and `"` so a crafted
    /// path can never break out of the quotes and inject profile syntax.
    fn sbpl_string(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for ch in s.chars() {
            if ch == '\\' || ch == '"' {
                out.push('\\');
            }
            out.push(ch);
        }
        out.push('"');
        out
    }

    #[cfg(test)]
    mod unit {
        use super::*;
        use crate::{ResolvedScope, Scope};

        /// The production `net:none` profile installs the ZERO Mach-lookup floor
        /// (agent-bridle#405): default-deny named lookup and re-allow NOTHING —
        /// no candidate, no former compatibility service, no `nsurlsessiond`.
        #[test]
        fn net_none_profile_installs_the_zero_mach_floor() {
            let cav = Caveats {
                net: Scope::none(),
                ..Caveats::top()
            };
            let profile = seatbelt_profile(&cav);
            assert!(profile.contains("(deny network*)"), "{profile}");
            assert!(
                profile.contains("(deny mach-lookup)"),
                "net:none production profile must install the Mach floor: {profile}"
            );
            assert!(
                !profile.contains("(allow mach-lookup"),
                "zero floor: nothing is re-allowed without a grant: {profile}"
            );
            for candidate in super::super::MACH_SERVICE_CANDIDATES {
                assert!(
                    !profile.contains(candidate),
                    "candidate {candidate} must be withheld by default: {profile}"
                );
            }
            assert!(!profile.contains("nsurlsessiond"), "{profile}");
        }

        /// A `mach:` grant emits exactly one `global-name` literal per granted
        /// service after the deny (last-match-wins) and no other re-allow.
        #[test]
        fn mach_grant_reopens_exactly_the_named_service() {
            let cav = Caveats {
                net: Scope::only([
                    "mach:com.apple.SecurityServer".to_string(),
                    "mach:com.apple.system.opendirectoryd.libinfo".to_string(),
                ]),
                ..Caveats::top()
            };
            let profile = seatbelt_profile(&cav);
            assert!(profile.contains("(deny network*)"), "{profile}");
            let deny = profile.find("(deny mach-lookup)").expect("floor present");
            let allow = profile
                .find("(allow mach-lookup (global-name \"com.apple.SecurityServer\") (global-name \"com.apple.system.opendirectoryd.libinfo\"))")
                .expect("exactly the two grants, sorted: {profile}");
            assert!(allow > deny, "the re-allow must follow the deny: {profile}");
            assert_eq!(
                profile.matches("(allow mach-lookup").count(),
                1,
                "{profile}"
            );
            assert!(
                !profile.contains("trustd"),
                "an ungranted candidate stays denied: {profile}"
            );
            // A grant alongside a `unix:` endpoint is still a direct-denied shape.
            let sock = std::env::temp_dir().join(format!("ab405-{}.sock", std::process::id()));
            let _ = std::fs::remove_file(&sock);
            let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
            let canonical = std::fs::canonicalize(&sock)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let mixed = Caveats {
                net: Scope::only([
                    format!("unix:{canonical}"),
                    "mach:com.apple.logd".to_string(),
                ]),
                ..Caveats::top()
            };
            let profile = seatbelt_profile(&mixed);
            assert!(profile.contains("(deny mach-lookup)"), "{profile}");
            assert!(
                profile.contains("(global-name \"com.apple.logd\")"),
                "{profile}"
            );
            assert!(
                profile.contains("(allow network-outbound (literal"),
                "{profile}"
            );
            let _ = std::fs::remove_file(&sock);
        }

        /// A malformed grant refuses before any profile is built, and a grant
        /// under a shape with no Mach floor (loopback) refuses rather than being
        /// accepted as a no-op the fence would never act on.
        #[test]
        fn mach_grant_refuses_when_malformed_or_floorless() {
            let bad = Caveats {
                net: Scope::only(["mach:com.apple.*".to_string()]),
                ..Caveats::top()
            };
            assert!(SeatbeltSandbox::new().command_prefix(&bad).is_err());
            let floorless = Caveats {
                net: Scope::only(["localhost".to_string(), "mach:com.apple.logd".to_string()]),
                ..Caveats::top()
            };
            let err = SeatbeltSandbox::new()
                .command_prefix(&floorless)
                .expect_err("loopback leaves Mach ambient; a grant there is refused");
            assert!(err.to_string().contains("network-denied"), "{err}");
        }

        /// A granted (non-empty) net axis, or an unrestricted one, does not add the
        /// mach-lookup deny — the deputy close is only for the no-egress claim.
        #[test]
        fn granted_net_does_not_deny_mach_lookup() {
            let profile = seatbelt_profile(&Caveats::top());
            assert!(!profile.contains("(deny mach-lookup)"), "{profile}");
        }

        /// agent-bridle#405/ADR 0015 amendment E6: the deputy audit is now
        /// `Complete` (the full channel sweep closed, AppleEvents included).
        /// `net:none` zero grants resolves to the bottom element `∅` — the
        /// promotion this whole audit exists to back.
        #[test]
        fn net_none_resolves_bounded_empty_now_the_audit_is_complete() {
            assert_eq!(MACH_DEPUTY_AUDIT, MachDeputyAudit::Complete);
            let cav = Caveats {
                net: Scope::none(),
                ..Caveats::top()
            };
            assert_eq!(
                SeatbeltSandbox::new()
                    .resolved_authority(&cav, crate::StdioPosture::Audited)
                    .net,
                ResolvedScope::empty(),
                "net:none zero grants must resolve to the bottom element now the audit is complete"
            );
        }

        /// A named `mach:` grant resolves to its own class, never to `∅` and
        /// never to `Unknown` — the "never collapse to ∅" requirement from
        /// #405's acceptance criteria, now exercised through the real
        /// `resolved_authority`, not just the pure projection function.
        #[test]
        fn mach_grant_resolves_its_named_class_not_unknown_or_empty() {
            let cav = Caveats {
                net: Scope::only(["mach:com.apple.SecurityServer".to_string()]),
                ..Caveats::top()
            };
            assert_eq!(
                SeatbeltSandbox::new()
                    .resolved_authority(&cav, crate::StdioPosture::Audited)
                    .net,
                ResolvedScope::class(super::super::seatbelt_mach_service_class(
                    "com.apple.SecurityServer"
                )),
                "a named grant must never collapse to ∅ nor remain Unknown"
            );
        }

        /// Support remains held for shapes this audit never covered: loopback
        /// and a general remote-host allowlist carry no Mach floor at all (no
        /// `(deny mach-lookup)` is ever emitted for them — see
        /// `seatbelt_profile_with`), so a complete deputy audit for the
        /// deny-all shape says nothing about them. #405's acceptance criteria
        /// ("named grants stay Unknown until each is audited" extends to
        /// shapes with no floor to audit at all).
        #[test]
        fn loopback_and_remote_host_stay_unknown_even_with_a_complete_audit() {
            for net in [
                Scope::only(["127.0.0.1".to_string()]),
                Scope::only(["example.com".to_string()]),
            ] {
                let cav = Caveats {
                    net: net.clone(),
                    ..Caveats::top()
                };
                assert_eq!(
                    SeatbeltSandbox::new()
                        .resolved_authority(&cav, crate::StdioPosture::Audited)
                        .net,
                    ResolvedScope::Unknown,
                    "{net:?} has no Mach floor at all; the audit doesn't bound it"
                );
            }
            assert_eq!(
                SeatbeltSandbox::new()
                    .resolved_authority(&Caveats::top(), crate::StdioPosture::Audited)
                    .net,
                ResolvedScope::Unbounded
            );
        }

        /// The PROJECTION a grant takes once the deputy audit is complete: a
        /// named class per granted service (never `∅` while a grant exists),
        /// `unix:` endpoints concrete, and the lattice-level scope comparison
        /// honest — the class compares as `Subset` only because the Seatbelt
        /// closure declares exactly that class for the grant; without the
        /// declaration it is Incomparable and refuses. This exercises the pure
        /// `admit` lattice law only: it is set bookkeeping for the L3 bound, not
        /// `AdmittedFence::admit` (which also applies the L4 strength floor and
        /// the Advisory net report) and not native deputy safety. Operational
        /// admission under `Complete` is deferred to the promotion PR.
        #[test]
        fn complete_audit_projects_grants_as_named_classes() {
            use crate::{admit, empty_closure, AdmissionDecision};
            use std::collections::BTreeSet;
            let cav = Caveats {
                net: Scope::only([
                    "mach:com.apple.SecurityServer".to_string(),
                    "mach:com.apple.logd".to_string(),
                ]),
                ..Caveats::top()
            };
            let net = seatbelt_net_projection(&cav, MachDeputyAudit::Complete);
            assert_eq!(
                net,
                ResolvedScope::Bounded {
                    concrete: BTreeSet::new(),
                    classes: BTreeSet::from([
                        "seatbelt-mach-service:com.apple.SecurityServer".to_string(),
                        "seatbelt-mach-service:com.apple.logd".to_string(),
                    ]),
                },
                "a grant must appear by name and never collapse to ∅"
            );
            // net:none with no grants is exactly ∅ under a complete audit.
            let none = Caveats {
                net: Scope::none(),
                ..Caveats::top()
            };
            assert_eq!(
                seatbelt_net_projection(&none, MachDeputyAudit::Complete),
                ResolvedScope::empty()
            );
            // Loopback keeps Mach ambient: no floor ⇒ still Unknown.
            let lo = Caveats {
                net: Scope::only(["localhost".to_string()]),
                ..Caveats::top()
            };
            assert_eq!(
                seatbelt_net_projection(&lo, MachDeputyAudit::Complete),
                ResolvedScope::Unknown
            );
            // Admission: the closure declares the class ⇒ Subset ⇒ admit;
            // no declaration ⇒ Incomparable ⇒ refuse (the class is not in the
            // delegated concrete set).
            let sandbox = SeatbeltSandbox::new();
            let mut resolved = crate::ResolvedAuthority::from_delegated(&cav);
            resolved.net = net;
            let closure = sandbox.runtime_closure(&cav);
            assert_eq!(
                closure.net,
                ResolvedScope::Bounded {
                    concrete: BTreeSet::new(),
                    classes: BTreeSet::from([
                        "seatbelt-mach-service:com.apple.SecurityServer".to_string(),
                        "seatbelt-mach-service:com.apple.logd".to_string(),
                    ]),
                }
            );
            assert!(crate::admitted::closure_is_harness_disjoint(&closure));
            assert!(matches!(
                admit(&resolved, &cav, &closure),
                AdmissionDecision::Admit
            ));
            assert!(matches!(
                admit(&resolved, &cav, &empty_closure()),
                AdmissionDecision::Reject(_)
            ));
            // The closure never invents a class the caveats did not name.
            assert_eq!(sandbox.runtime_closure(&none).net, ResolvedScope::empty());
        }

        /// #1220: a write-confined profile must re-allow the device sinks as
        /// literals — git's O_RDWR open of /dev/null dies otherwise.
        #[test]
        fn write_confined_profile_allows_the_device_sinks() {
            let confined = Caveats {
                fs_write: Scope::only(["/tmp/x".to_string()]),
                ..Caveats::top()
            };
            let profile = seatbelt_profile(&confined);
            assert!(profile.contains("(deny file-write*)"), "{profile}");
            assert!(
                profile.contains("(literal \"/dev/null\")"),
                "the null sink must stay write-openable: {profile}"
            );
        }

        #[test]
        fn unrestricted_caveats_make_no_wrapper() {
            assert!(SeatbeltSandbox::new()
                .command_prefix(&Caveats::top())
                .unwrap()
                .is_empty());
        }

        /// #144 (I5-B) regression guard: the Seatbelt backend must read its base
        /// allow-list from `self.policy` on the PRODUCTION path (`command_prefix`
        /// → `seatbelt_profile_with`), not a hardcoded const. A widened
        /// `base_read_paths` must appear in the generated SBPL profile; the
        /// default policy must not admit it. Mirrors the Landlock proof
        /// `landlock_config_widens_base_read`, so a revert of the const path is
        /// caught on macOS too (previously only Landlock had this coverage).
        #[test]
        fn command_prefix_widens_the_read_base_from_policy() {
            if !seatbelt_is_supported() {
                eprintln!("skipping: /usr/bin/sandbox-exec unavailable");
                return;
            }
            let extra = std::env::temp_dir().join("abridle-seatbelt-cfg-widen");
            std::fs::create_dir_all(&extra).unwrap();
            let extra_str = extra.to_string_lossy().into_owned();
            // The profile carries the canonicalized path (e.g. /tmp → /private/tmp).
            let want = canonical_path(&extra_str).expect("temp dir canonicalizes");

            // fs_read must be restricted for the read base to be emitted at all.
            let cav = Caveats {
                fs_read: Scope::only(["/usr".to_string()]),
                ..Caveats::top()
            };

            // Control: the default read base does NOT admit the extra dir.
            let default_prefix = SeatbeltSandbox::new().command_prefix(&cav).unwrap();
            assert!(
                !default_prefix.iter().any(|a| a.contains(&want)),
                "default read base must not include the extra dir: {default_prefix:?}"
            );

            // Widened policy: add `extra` to base_read_paths → it appears.
            let mut base = SandboxPolicy::default().base_read_paths;
            base.extra.push(extra_str);
            let policy = Arc::new(SandboxPolicy {
                base_read_paths: base,
                ..SandboxPolicy::default()
            });
            let widened_prefix = SeatbeltSandbox::with_policy(policy)
                .command_prefix(&cav)
                .unwrap();
            assert!(
                widened_prefix.iter().any(|a| a.contains(&want)),
                "config-widened base_read_paths must reach the SBPL profile: {widened_prefix:?}"
            );

            let _ = std::fs::remove_dir_all(&extra);
        }

        #[test]
        fn empty_net_denies_direct_socket_egress_and_engages_the_wrapper() {
            // net:none with fs unrestricted still confines (network), so the
            // wrapper must engage and the profile must deny direct socket egress.
            let cav = Caveats {
                net: Scope::none(),
                ..Caveats::top()
            };
            let prof = seatbelt_profile(&cav);
            assert!(prof.contains("(deny network*)"), "{prof}");
            assert!(
                !SeatbeltSandbox::new()
                    .command_prefix(&cav)
                    .unwrap()
                    .is_empty(),
                "net:none must engage the sandbox-exec wrapper"
            );
        }

        #[test]
        fn nonempty_net_allowlist_is_not_denied() {
            // A general (non-loopback) host allowlist is not expressible in SBPL —
            // it can name only `*`/`localhost` + ports as a remote — so no network
            // rule is emitted; left ambient (advisory), never silently dropped.
            let cav = Caveats {
                net: Scope::only(["example.com".to_string()]),
                ..Caveats::top()
            };
            let prof = seatbelt_profile(&cav);
            assert!(
                !prof.contains("network"),
                "non-loopback net must stay ambient: {prof}"
            );
        }

        #[test]
        fn loopback_only_net_confines_to_loopback_and_engages() {
            // A loopback-only allowlist IS expressible: deny all egress, then
            // re-allow the loopback interface (ADR 0015). Off-box egress stays
            // kernel-denied; the wrapper engages even with fs/exec unrestricted.
            for host in ["localhost", "127.0.0.1", "::1"] {
                let cav = Caveats {
                    net: Scope::only([host.to_string()]),
                    ..Caveats::top()
                };
                let prof = seatbelt_profile(&cav);
                assert!(prof.contains("(deny network*)"), "{host}: {prof}");
                assert!(
                    prof.contains("(allow network* (remote ip \"localhost:*\"))"),
                    "{host}: loopback re-allow missing: {prof}"
                );
                assert!(
                    prof.contains(
                        "(allow network-bind network-inbound (local ip \"localhost:*\"))"
                    ),
                    "{host}: loopback listener rule missing: {prof}"
                );
                assert!(
                    !prof.contains("(local ip \"*:*\")") && !prof.contains("(remote ip \"*:*\")"),
                    "{host}: no rule may name a non-loopback address: {prof}"
                );
                assert!(
                    !SeatbeltSandbox::new()
                        .command_prefix(&cav)
                        .unwrap()
                        .is_empty(),
                    "{host}: a loopback-only net grant must engage the wrapper"
                );
            }
        }

        #[test]
        fn mixed_loopback_and_remote_host_stays_ambient() {
            // A single non-loopback host taints the set: SBPL cannot express the
            // remote, so the whole allowlist stays ambient (advisory) rather than
            // emit a rule that would silently drop `example.com`.
            let cav = Caveats {
                net: Scope::only(["localhost".to_string(), "example.com".to_string()]),
                ..Caveats::top()
            };
            let prof = seatbelt_profile(&cav);
            assert!(
                !prof.contains("network"),
                "a mixed loopback+remote allowlist must stay ambient: {prof}"
            );
        }

        #[test]
        fn loopback_fenced_caveats_emit_the_egress_proxy_fence() {
            // The egress-proxy mechanism (#124, ADR 0016) fences a remote-host
            // grant to loopback via `loopback_fenced_caveats`: the resulting
            // profile must carry the ADR 0015 loopback fence AND preserve fs/exec.
            let granted = Caveats {
                net: Scope::only(["example.com".to_string()]),
                fs_write: Scope::only(["/tmp".to_string()]),
                ..Caveats::top()
            };
            // The remote grant alone emits NO net rule (advisory) …
            assert!(!seatbelt_profile(&granted).contains("network"));
            // … but its loopback-fenced form emits the kernel egress fence.
            let prof = seatbelt_profile(&super::super::loopback_fenced_caveats(&granted));
            assert!(prof.contains("(deny network*)"), "{prof}");
            assert!(
                prof.contains("(allow network* (remote ip \"localhost:*\"))"),
                "fence must re-allow loopback: {prof}"
            );
            assert!(
                prof.contains("(deny file-write*)"),
                "fs_write rule must survive the fence: {prof}"
            );
        }

        #[test]
        fn restricted_write_yields_sandbox_exec_wrapper() {
            let cav = Caveats {
                fs_write: Scope::only(["/tmp".to_string()]),
                ..Caveats::top()
            };
            let prefix = SeatbeltSandbox::new().command_prefix(&cav).unwrap();
            assert_eq!(prefix[0], SANDBOX_EXEC);
            assert_eq!(prefix[1], "-p");
            assert!(prefix[2].contains("(deny file-write*)"));
        }

        #[test]
        fn profile_denies_then_reallows_write_roots() {
            let cav = Caveats {
                fs_write: Scope::only(["/tmp".to_string()]),
                ..Caveats::top()
            };
            let prof = seatbelt_profile(&cav);
            assert!(prof.contains("(allow default)"));
            assert!(prof.contains("(deny file-write*)"));
            // `/tmp` must be canonicalized to its real target for subpath match.
            assert!(prof.contains("(subpath \"/private/tmp\")"), "{prof}");
            // No read axis restricted => no read deny.
            assert!(!prof.contains("(deny file-read*)"));
        }

        #[test]
        fn empty_write_scope_denies_all_writes_no_allow() {
            let cav = Caveats {
                fs_write: Scope::none(),
                ..Caveats::top()
            };
            let prof = seatbelt_profile(&cav);
            assert!(prof.contains("(deny file-write*)"));
            assert!(
                !prof.contains("(subpath"),
                "an empty scope must grant no write roots: {prof}"
            );
            // #1220: the device sinks stay write-openable even with an empty
            // write scope — that re-allow is a `literal`, not a `subpath` root.
            assert!(
                prof.contains("(literal \"/dev/null\")"),
                "device sinks must still be re-allowed: {prof}"
            );
        }

        #[test]
        fn restricted_read_includes_loader_base_and_root_entry() {
            let cav = Caveats {
                fs_read: Scope::only(["/tmp".to_string()]),
                ..Caveats::top()
            };
            let prof = seatbelt_profile(&cav);
            assert!(prof.contains("(deny file-read*)"));
            assert!(prof.contains("(literal \"/\")"), "{prof}");
            assert!(prof.contains("(subpath \"/usr\")"), "{prof}");
            assert!(prof.contains("(subpath \"/System\")"), "{prof}");
        }

        #[test]
        fn sbpl_string_escapes_quotes_and_backslashes() {
            assert_eq!(sbpl_string("/a/b"), "\"/a/b\"");
            assert_eq!(sbpl_string("/a\"b"), "\"/a\\\"b\"");
            assert_eq!(sbpl_string("/a\\b"), "\"/a\\\\b\"");
        }

        /// Count double-quotes that are *not* backslash-escaped — the structural
        /// quotes SBPL actually sees. Each `(subpath "…")` term I emit
        /// contributes exactly two; any extra would mean a path broke out of its
        /// literal.
        fn unescaped_quotes(s: &str) -> usize {
            let b = s.as_bytes();
            (0..b.len())
                .filter(|&i| b[i] == b'"' && (i == 0 || b[i - 1] != b'\\'))
                .count()
        }

        #[test]
        fn crafted_path_cannot_inject_profile_syntax() {
            // A path crafted to close the string and add its own allow rule must
            // stay inside one escaped literal — its quotes get backslash-escaped,
            // so SBPL sees exactly the two structural quotes of the single term.
            let cav = Caveats {
                fs_write: Scope::only(["/tmp/x\") (allow file-write* (subpath \"/".to_string()]),
                ..Caveats::top()
            };
            let prof = seatbelt_profile(&cav);
            // Every other structural term is a plain, non-crafted literal (the
            // #1220 device sinks); the crafted root contributes exactly one
            // structural (subpath "…") term — 2 unescaped quotes — on top of
            // those.
            let sinks = SandboxPolicy::default().device_sink_paths.resolve();
            assert_eq!(
                unescaped_quotes(&prof),
                2 + 2 * sinks.len(),
                "exactly one structural (subpath \"…\") term — no breakout: {prof}"
            );
            assert!(
                prof.contains("\\\""),
                "the crafted quotes must be backslash-escaped: {prof}"
            );
        }

        #[test]
        fn restricted_exec_emits_deny_and_allowlist() {
            let cav = Caveats {
                exec: Scope::only(["/bin/echo".to_string()]),
                ..Caveats::top()
            };
            let prof = seatbelt_profile(&cav);
            assert!(prof.contains("(deny process-exec*)"), "{prof}");
            assert!(
                prof.contains("(allow process-exec* (literal \"/bin/echo\")"),
                "{prof}"
            );
        }

        #[test]
        fn bare_name_exec_resolves_through_trusted_dirs() {
            // A bare name is pinned to the fixed trusted system dirs, never $PATH.
            let cav = Caveats {
                exec: Scope::only(["true".to_string()]),
                ..Caveats::top()
            };
            let prof = seatbelt_profile(&cav);
            // `/usr/bin/true` exists on every macOS host and canonicalizes to
            // itself, so the literal must name the absolute resolved path.
            assert!(
                prof.contains("(literal \"/usr/bin/true\")"),
                "bare name must resolve to its trusted-dir absolute path: {prof}"
            );
        }

        #[test]
        fn granting_sh_also_allows_its_bash_variant() {
            // agent-bridle#318: Apple's `/bin/sh` re-execs `/bin/bash` at startup,
            // a kernel-checked `process-exec`. A restricted exec grant of `sh`
            // must therefore also anchor `/bin/bash`, or the confined shell dies
            // at its own variant re-exec and never runs its body.
            let cav = Caveats {
                exec: Scope::only(["sh".to_string()]),
                ..Caveats::top()
            };
            let prof = seatbelt_profile(&cav);
            assert!(
                prof.contains("(literal \"/bin/sh\")"),
                "the granted shell itself must be allowed: {prof}"
            );
            assert!(
                prof.contains("(literal \"/bin/bash\")"),
                "sh's /bin/bash interpreter variant must be allowed too (#318): {prof}"
            );
            // Control: a non-sh grant does NOT pull in bash.
            let echo = Caveats {
                exec: Scope::only(["echo".to_string()]),
                ..Caveats::top()
            };
            assert!(
                !seatbelt_profile(&echo).contains("/bin/bash"),
                "granting echo must not add bash",
            );
        }

        #[test]
        fn restricted_exec_engages_the_wrapper() {
            // exec-only (no fs/net restriction) must still engage sandbox-exec.
            let cav = Caveats {
                exec: Scope::only(["/bin/echo".to_string()]),
                ..Caveats::top()
            };
            let prefix = SeatbeltSandbox::new().command_prefix(&cav).unwrap();
            assert_eq!(prefix.first().map(String::as_str), Some(SANDBOX_EXEC));
        }

        #[test]
        fn empty_exec_scope_denies_all_exec_with_no_allow() {
            // exec:none — the program may exec nothing. The deny is emitted with no
            // re-allow, so even the wrapped program's launch is denied: fail-closed,
            // never silently ambient.
            let cav = Caveats {
                exec: Scope::none(),
                ..Caveats::top()
            };
            let prof = seatbelt_profile(&cav);
            assert!(prof.contains("(deny process-exec*)"), "{prof}");
            assert!(
                !prof.contains("(allow process-exec*"),
                "an empty exec scope must grant no exec targets: {prof}"
            );
        }

        #[test]
        fn relative_and_unresolvable_exec_grants_are_dropped() {
            // A relative-path grant cannot anchor a kernel rule; a bare name with no
            // trusted-dir hit resolves to nothing. Either way: deny with no allow.
            let cav = Caveats {
                exec: Scope::only(["./payload".to_string(), "no-such-binary-xyzzy".to_string()]),
                ..Caveats::top()
            };
            let prof = seatbelt_profile(&cav);
            assert!(prof.contains("(deny process-exec*)"), "{prof}");
            assert!(
                !prof.contains("(allow process-exec*"),
                "unresolvable/relative grants must not anchor an allow: {prof}"
            );
        }

        #[test]
        fn unrestricted_exec_emits_no_exec_rules() {
            // exec:All (the default) is ambient on the exec axis — no rules.
            let prof = seatbelt_profile(&Caveats::top());
            assert!(!prof.contains("process-exec"), "{prof}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Scope;

    #[test]
    fn noop_reports_none_and_never_fails() {
        let s = NoopSandbox;
        assert_eq!(s.kind(), SandboxKind::None);
        assert!(s.apply(&Caveats::top()).is_ok());
    }

    // ── agent-bridle#405: `mach:` service grants are structural net tokens ──

    fn with_net<I: IntoIterator<Item = &'static str>>(names: I) -> Caveats {
        Caveats {
            net: Scope::only(names.into_iter().map(str::to_string)),
            ..Caveats::top()
        }
    }

    /// The grant vocabulary is exact: launchd global-names only. A malformed
    /// token refuses (fail-closed) rather than reaching the profile.
    #[test]
    fn mach_service_grants_validate_sort_and_dedup() {
        assert_eq!(
            mach_service_grants(&Caveats::top()).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            mach_service_grants(&with_net([
                "mach:com.apple.trustd.agent",
                "mach:com.apple.SecurityServer",
                "mach:com.apple.SecurityServer",
                "unix:/private/tmp/s.sock",
            ]))
            .unwrap(),
            vec![
                "com.apple.SecurityServer".to_string(),
                "com.apple.trustd.agent".to_string()
            ]
        );
        for bad in [
            "mach:",
            "mach:com.apple.*",
            "mach:a b",
            "mach:x\"y",
            "mach:é",
        ] {
            assert!(
                mach_service_grants(&with_net([bad])).is_err(),
                "{bad:?} must refuse"
            );
        }
    }

    /// A `mach:` token is structural, like `unix:`: it never becomes an egress
    /// proxy host, and it does not stop a set from being direct-denied.
    #[test]
    fn mach_grants_are_structural_not_hosts() {
        let m = "mach:com.apple.system.opendirectoryd.libinfo";
        assert!(net_direct_denied(&with_net([m])));
        assert!(net_direct_denied(&with_net([
            m,
            "unix:/private/tmp/s.sock"
        ])));
        assert!(net_direct_denied(&Caveats {
            net: Scope::none(),
            ..Caveats::top()
        }));
        assert!(!net_direct_denied(&with_net([m, "localhost"])));
        assert!(!net_direct_denied(&with_net(["example.com"])));
        assert!(!net_fully_denied(&with_net([m])), "not the EMPTY set");
        assert_eq!(net_egress_proxy_hosts(&with_net([m])), None);
        assert_eq!(
            net_egress_proxy_hosts(&with_net([m, "example.com"])),
            Some(vec!["example.com".to_string()]),
            "the grant never enters the proxy host list"
        );
        assert!(net_loopback_only(&with_net([m, "localhost"])));
        assert!(
            !net_loopback_full_interface(&with_net([m, "localhost"])),
            "a mach grant is authority beyond the loopback interface"
        );
        // A mach-bearing remote-host scope has no proxy semantics: the loopback
        // fence would erase the grant, so the planner refuses on EVERY backend,
        // Seatbelt included (never a plan with the token silently dropped).
        let mixed = with_net([m, "example.com"]);
        assert!(
            egress_proxy_plan_for(SandboxKind::Seatbelt, &mixed).is_none(),
            "Seatbelt must not plan a proxy that drops the mach grant"
        );
        assert!(
            !matches!(&loopback_fenced_caveats(&mixed).net, Scope::Only(set) if set.contains(m)),
            "the fence does not carry the grant, which is why the plan is refused"
        );
        assert!(
            egress_proxy_plan_for(SandboxKind::Landlock, &with_net([m, "example.com"])).is_none()
        );
        assert!(
            egress_proxy_plan_for(SandboxKind::AppContainer, &with_net([m, "example.com"]))
                .is_none()
        );
        assert_eq!(
            effective_sandbox_kind(SandboxKind::Seatbelt, &with_net([m])),
            SandboxKind::Seatbelt
        );
        assert_eq!(
            effective_sandbox_kind(SandboxKind::AppContainer, &with_net([m])),
            SandboxKind::None
        );
    }

    /// The structured "service X denied" result: only a network-denied
    /// Seatbelt run carries a Mach floor, so only it discloses; the grant is
    /// listed by name and every other known candidate is listed as withheld.
    #[test]
    fn mach_service_disclosure_names_granted_and_withheld() {
        let m = "mach:com.apple.SecurityServer";
        let d = mach_service_disclosure(&with_net([m]), SandboxKind::Seatbelt)
            .unwrap()
            .expect("a network-denied Seatbelt run discloses");
        assert_eq!(d.granted, vec!["com.apple.SecurityServer".to_string()]);
        assert_eq!(d.withheld.len(), MACH_SERVICE_CANDIDATES.len() - 1);
        assert!(!d.withheld.iter().any(|s| s == "com.apple.SecurityServer"));
        assert!(d
            .withheld
            .iter()
            .any(|s| s == "com.apple.system.opendirectoryd.libinfo"));

        let none = Caveats {
            net: Scope::none(),
            ..Caveats::top()
        };
        let d = mach_service_disclosure(&none, SandboxKind::Seatbelt)
            .unwrap()
            .expect("net:none discloses");
        assert!(d.granted.is_empty());
        assert_eq!(
            d.withheld.len(),
            MACH_SERVICE_CANDIDATES.len(),
            "zero floor: every candidate withheld"
        );

        // No floor ⇒ nothing to disclose: other backends, ambient net, loopback.
        for kind in [
            SandboxKind::Landlock,
            SandboxKind::AppContainer,
            SandboxKind::None,
        ] {
            assert_eq!(
                mach_service_disclosure(&none, kind).unwrap(),
                None,
                "{kind:?}"
            );
        }
        assert_eq!(
            mach_service_disclosure(&Caveats::top(), SandboxKind::Seatbelt).unwrap(),
            None
        );
        assert_eq!(
            mach_service_disclosure(&with_net(["localhost"]), SandboxKind::Seatbelt).unwrap(),
            None
        );
        assert!(
            mach_service_disclosure(&with_net(["mach:bad name"]), SandboxKind::Seatbelt).is_err()
        );
        assert_eq!(
            seatbelt_mach_service_class("com.apple.logd"),
            "seatbelt-mach-service:com.apple.logd"
        );
    }

    #[test]
    fn net_egress_proxy_hosts_triggers_only_on_a_general_remote_allowlist() {
        let with_net = |net| {
            net_egress_proxy_hosts(&Caveats {
                net,
                ..Caveats::top()
            })
        };
        // No trigger: unrestricted, deny-all, or loopback-only — owned elsewhere.
        assert_eq!(with_net(Scope::All), None);
        assert_eq!(with_net(Scope::none()), None); // empty = deny-all (net_fully_denied)
        for lo in ["localhost", "127.0.0.1", "::1"] {
            assert_eq!(
                with_net(Scope::only([lo.to_string()])),
                None,
                "{lo} is loopback-only"
            );
        }
        // Trigger: a remote host, alone or mixed with loopback (full set returned).
        assert_eq!(
            with_net(Scope::only(["example.com".to_string()])),
            Some(vec!["example.com".to_string()])
        );
        let mixed = with_net(Scope::only([
            "example.com".to_string(),
            "localhost".to_string(),
        ]))
        .expect("mixed set triggers");
        assert_eq!(
            mixed.len(),
            2,
            "the FULL grant is returned, loopback included: {mixed:?}"
        );
        assert!(
            mixed.contains(&"example.com".to_string()) && mixed.contains(&"localhost".to_string())
        );
    }

    #[test]
    fn loopback_fenced_caveats_swaps_net_to_loopback_preserving_other_axes() {
        let granted = Caveats {
            net: Scope::only(["example.com".to_string()]),
            fs_write: Scope::only(["/tmp/x".to_string()]),
            exec: Scope::only(["git".to_string()]),
            ..Caveats::top()
        };
        let fenced = loopback_fenced_caveats(&granted);
        // net is now loopback-only, so it engages the ADR 0015 kernel fence …
        assert!(
            net_loopback_only(&fenced),
            "fenced net must be loopback-only"
        );
        assert!(
            net_egress_proxy_hosts(&fenced).is_none(),
            "fenced caveats no longer trigger the proxy"
        );
        // … while fs/exec are preserved verbatim (the fence keeps their rules).
        assert_eq!(fenced.fs_write, granted.fs_write);
        assert_eq!(fenced.exec, granted.exec);
    }

    /// Regression (#257/#275 fail-open): the egress proxy must engage ONLY where
    /// the backend can address-fence the child's egress to loopback. The prior
    /// gate (`effective_sandbox_kind != None`) let Landlock through whenever the
    /// *fs* axis engaged — but Landlock's `net` fence is port-based and cannot
    /// confine a loopback-only host set, so the child could dial around the proxy
    /// while the system reported it fenced. This asserts the net-axis-specific gate.
    #[test]
    fn egress_proxy_plan_engages_only_where_loopback_net_is_enforceable() {
        // The Leg-4 config that triggered the fail-open: a remote-host `net`
        // allow-list AND a restricted fs axis (so Landlock engages on fs).
        let leg4 = Caveats {
            net: Scope::only(["api.github.com".to_string()]),
            fs_write: Scope::only(["/work".to_string()]),
            ..Caveats::top()
        };
        // Address-fenceable backends engage the proxy (real confinement).
        assert!(
            egress_proxy_plan_for(SandboxKind::Seatbelt, &leg4).is_some(),
            "Seatbelt fences net to loopback (SBPL) → proxy is real confinement"
        );
        assert!(
            egress_proxy_plan_for(SandboxKind::AppContainer, &leg4).is_some(),
            "AppContainer loopback-exemption → proxy is real confinement"
        );
        // THE FIX: Landlock engages on fs but CANNOT address-fence net, so the
        // proxy must NOT engage — otherwise it is walk-around-able false
        // confinement. This assertion fails against the pre-fix gate.
        assert_eq!(
            egress_proxy_plan_for(SandboxKind::Landlock, &leg4),
            None,
            "Landlock is port-based; loopback-only net is unenforceable → advisory, no walk-around proxy"
        );
        // Tiers that don't namespace net at their level, and 'no backend', are
        // advisory too — never a walk-around proxy.
        for k in [
            SandboxKind::MinimalRootfs,
            SandboxKind::MicroVm,
            SandboxKind::None,
        ] {
            assert_eq!(
                egress_proxy_plan_for(k, &leg4),
                None,
                "{k:?} does not address-fence net → advisory"
            );
        }
        // A non-proxy grant (net: All) never engages, even on a fenceable backend.
        assert_eq!(
            egress_proxy_plan_for(
                SandboxKind::Seatbelt,
                &Caveats {
                    net: Scope::All,
                    ..Caveats::top()
                }
            ),
            None,
            "net: All needs no fence"
        );
    }

    #[test]
    fn sandbox_kind_serde_is_snake_case() {
        assert_eq!(
            serde_json::to_string(&SandboxKind::None).unwrap(),
            "\"none\""
        );
        assert_eq!(
            serde_json::to_string(&SandboxKind::Landlock).unwrap(),
            "\"landlock\""
        );
        assert_eq!(
            serde_json::to_string(&SandboxKind::Seatbelt).unwrap(),
            "\"seatbelt\""
        );
        assert_eq!(
            serde_json::to_string(&SandboxKind::AppContainer).unwrap(),
            "\"app_container\""
        );
        assert_eq!(
            serde_json::to_string(&SandboxKind::MinimalRootfs).unwrap(),
            "\"minimal_rootfs\""
        );
        assert_eq!(
            serde_json::to_string(&SandboxKind::MicroVm).unwrap(),
            "\"micro_vm\""
        );
    }

    #[test]
    fn effective_kind_downgrades_to_none_when_no_axis_is_restricted() {
        // The honesty rule (I9): a backend that confines nothing must not be
        // reported. With every axis `All`, even a real backend reports None.
        for available in [
            SandboxKind::Landlock,
            SandboxKind::Seatbelt,
            SandboxKind::AppContainer,
            SandboxKind::None,
        ] {
            assert_eq!(
                effective_sandbox_kind(available, &Caveats::top()),
                SandboxKind::None,
                "unrestricted fs must report None for {available:?}"
            );
        }
        // With a restricted fs axis, the backend's own kind is reported …
        let restricted = Caveats {
            fs_write: Scope::only(["/w".to_string()]),
            ..Caveats::top()
        };
        assert_eq!(
            effective_sandbox_kind(SandboxKind::Landlock, &restricted),
            SandboxKind::Landlock
        );
        assert_eq!(
            effective_sandbox_kind(SandboxKind::Seatbelt, &restricted),
            SandboxKind::Seatbelt
        );
        // … except a None host is always None (nothing to enforce with).
        assert_eq!(
            effective_sandbox_kind(SandboxKind::None, &restricted),
            SandboxKind::None
        );
        // A restricted *read* axis also engages (Landlock/Seatbelt govern reads).
        let read_only = Caveats {
            fs_read: Scope::only(["/r".to_string()]),
            ..Caveats::top()
        };
        assert_eq!(
            effective_sandbox_kind(SandboxKind::Seatbelt, &read_only),
            SandboxKind::Seatbelt
        );
        // An empty net scope (all egress denied), even with fs unrestricted,
        // engages Seatbelt. Landlock engages only on V4+ kernels (≥ 6.7) where
        // TCP deny-all is expressible; on older kernels it falls back to None.
        let net_denied = Caveats {
            net: Scope::none(),
            ..Caveats::top()
        };
        assert_eq!(
            effective_sandbox_kind(SandboxKind::Seatbelt, &net_denied),
            SandboxKind::Seatbelt,
            "Seatbelt kernel-denies egress, so net:none engages it"
        );
        let expected_landlock_net = if landlock_net_capable() {
            SandboxKind::Landlock
        } else {
            SandboxKind::None
        };
        assert_eq!(
            effective_sandbox_kind(SandboxKind::Landlock, &net_denied),
            expected_landlock_net,
            "Landlock engages for net:none only when V4 TCP-deny support is present"
        );
    }

    /// AppContainer engages for a loopback-only net scope (#133, ADR 0016).
    /// This enables the egress-proxy pattern: `loopback_fenced_caveats` produces
    /// a net=loopback grant, and with AppContainer that fence is kernel-expressed
    /// (off-box egress is denied; loopback exemption lets the child reach the proxy).
    #[test]
    fn appcontainer_engages_for_loopback_only_net() {
        for host in ["localhost", "127.0.0.1", "::1"] {
            let loopback_only = Caveats {
                net: Scope::only([host.to_string()]),
                ..Caveats::top()
            };
            assert_eq!(
                effective_sandbox_kind(SandboxKind::AppContainer, &loopback_only),
                SandboxKind::AppContainer,
                "AppContainer must engage for loopback host {host}"
            );
        }
        // A general remote host is NOT loopback-only → falls through to None
        // (net advisory; handled by egress-proxy when the sandbox is AppContainer).
        let remote = Caveats {
            net: Scope::only(["example.com".to_string()]),
            ..Caveats::top()
        };
        assert_eq!(
            effective_sandbox_kind(SandboxKind::AppContainer, &remote),
            SandboxKind::None,
            "general remote host must not directly engage AppContainer"
        );
    }

    /// `loopback_fenced_caveats` + AppContainer engages the backend, enabling
    /// `egress_proxy_plan` to route through the loopback proxy on Windows (#133).
    #[test]
    fn loopback_fenced_caveats_engages_appcontainer() {
        let remote = Caveats {
            net: Scope::only(["example.com".to_string()]),
            ..Caveats::top()
        };
        let fenced = loopback_fenced_caveats(&remote);
        assert!(
            net_loopback_only(&fenced),
            "loopback_fenced_caveats must produce a loopback-only net scope"
        );
        assert_eq!(
            effective_sandbox_kind(SandboxKind::AppContainer, &fenced),
            SandboxKind::AppContainer,
            "loopback-fenced caveats must engage AppContainer"
        );
    }

    #[test]
    fn best_available_sandbox_is_a_sandbox() {
        // Always returns *some* sandbox; on a non-landlock build/kernel it is the
        // advisory Noop. Just exercise the trait object.
        // AppContainer's `apply` is a deliberate no-op (confinement is applied at
        // process creation via `command_prefix`, not to the current thread).
        let sb = best_available_sandbox(&Arc::new(SandboxPolicy::default()));
        assert!(sb.apply(&Caveats::top()).is_ok());
    }

    #[cfg(all(target_os = "windows", feature = "windows-appcontainer"))]
    #[test]
    fn windows_appcontainer_feature_selects_appcontainer_backend() {
        assert_eq!(
            best_available_sandbox(&Arc::new(SandboxPolicy::default())).kind(),
            SandboxKind::AppContainer
        );
    }

    /// A descriptor that does not name the label it is bound to — the fd is
    /// open on object B while the caller claims it is root A — is refused by
    /// the validated constructor itself: `HeldReadRoot::bind` never returns an
    /// object for a (label, descriptor) pair that disagree about which object
    /// they name. No sandbox backend is involved; this is pure `fstat`/`stat`.
    ///
    /// Regression for #2674 P1: previously `HeldReadRoot::new` took any
    /// `(String, OwnedFd)` pair with no check at all, so a falsely labelled
    /// `HeldReadRoot(A, fd(B))` was fully constructible.
    #[cfg(unix)]
    #[test]
    fn falsely_labelled_held_root_is_refused_at_construction() {
        let pid = std::process::id();
        let a = std::env::temp_dir().join(format!("agent-bridle-bind-a-{pid}"));
        let b = std::env::temp_dir().join(format!("agent-bridle-bind-b-{pid}"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let fd_b = std::os::fd::OwnedFd::from(std::fs::File::open(&b).unwrap());

        let err = crate::HeldReadRoot::bind(a.to_string_lossy().into_owned(), fd_b)
            .expect_err("a's label over b's descriptor must not bind");
        assert!(
            err.to_string().contains("does not name"),
            "unexpected error: {err}"
        );

        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }
}

// Real kernel enforcement test. Only meaningful with the feature on Linux; it
// asserts the leash is the *kernel's*, not ours — the regression proof that
// `fs_write` confines a process even outside the in-process L2 interceptor.
#[cfg(all(target_os = "linux", feature = "linux-landlock", test))]
mod landlock_kernel_tests {
    use super::*;
    use crate::Scope;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    fn unique_dir(tag: &str) -> PathBuf {
        // No rand dep: derive a unique path from pid + a per-call atomic counter.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut d = std::env::temp_dir();
        d.push(format!(
            "agent-bridle-ll-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A hermetically-configured `git` `Command`: no inherited environment
    /// beyond what's explicitly set here, and every ambient
    /// config/hooks/template source disabled. Used for BOTH the plain
    /// (unconfined) fixture setup and the Landlock-confined proof command
    /// below, per bridle PR #407 review (P1-3/P2-4) — one constructor means
    /// "isolated enough not to touch the real repository" and "isolated
    /// enough to be a fair confinement proof" can't drift apart.
    ///
    /// `env_clear()` (not a denylist of individually-removed vars) is what
    /// closes the whole class the review named: `GIT_DIR`, `GIT_WORK_TREE`,
    /// `GIT_CEILING_DIRECTORIES` (the leak that once left a stray commit on
    /// this very branch), plus `GIT_INDEX_FILE`, `GIT_COMMON_DIR`,
    /// `GIT_OBJECT_DIRECTORY` (named in review finding 3) and anything else
    /// ambient — none of them can leak in if nothing is inherited.
    /// `GIT_CONFIG_NOSYSTEM=1` + `GIT_CONFIG_GLOBAL=/dev/null` +
    /// `GIT_TEMPLATE_DIR=/dev/null` additionally close config/template
    /// sources that live outside the process environment entirely (system
    /// `/etc/gitconfig`, the operator's own `~/.gitconfig`).
    fn hermetic_git_command(git: &str, dir: &std::path::Path, home: &std::path::Path) -> Command {
        let mut cmd = Command::new(git);
        cmd.current_dir(dir);
        cmd.env_clear();
        cmd.env("HOME", home);
        cmd.env("GIT_AUTHOR_NAME", "t");
        cmd.env("GIT_AUTHOR_EMAIL", "t@example.invalid");
        cmd.env("GIT_COMMITTER_NAME", "t");
        cmd.env("GIT_COMMITTER_EMAIL", "t@example.invalid");
        cmd.env("GIT_CONFIG_NOSYSTEM", "1");
        cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
        cmd.env("GIT_TEMPLATE_DIR", "/dev/null");
        cmd
    }

    /// Whether a kernel-enforcement proof should run, skip, or hard-**FAIL** — a
    /// pure decision over (Landlock supported?, enforcement required?). Required
    /// but unsupported is a FAILURE: a security library must not ship a green
    /// build in which its kernel boundary was never exercised (#74).
    #[derive(Debug, PartialEq, Eq)]
    enum ProofGate {
        Run,
        Skip,
        Fail,
    }

    fn proof_gate(supported: bool, required: bool) -> ProofGate {
        match (supported, required) {
            (true, _) => ProofGate::Run,
            (false, true) => ProofGate::Fail,
            (false, false) => ProofGate::Skip,
        }
    }

    /// `true` iff `BRIDLE_REQUIRE_LANDLOCK` is set (non-empty, not `"0"`) —
    /// the same flag [`skip_proof_unless_landlock`] gates on, factored out
    /// so [`require_trusted_git_or_fail`] can apply the identical
    /// require-not-skip posture to a DIFFERENT missing prerequisite (the
    /// fixture `git` binary, not the kernel feature).
    fn landlock_is_required() -> bool {
        std::env::var("BRIDLE_REQUIRE_LANDLOCK")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    }

    /// `true` iff `git` exists on this host. **Panics** when Landlock is
    /// *required* (`BRIDLE_REQUIRE_LANDLOCK=1`, as CI sets) but the fixture
    /// `git` is absent — bridle PR #407 review, round 3 finding 2: a
    /// required run that silently `eprintln!`+`return`ed on a missing
    /// `/usr/bin/git` could go green in CI without ever exercising the
    /// #2630 fence, exactly the failure mode [`skip_proof_unless_landlock`]
    /// already closes for a missing KERNEL feature. A local, non-required
    /// run still legitimately skips.
    fn require_trusted_git_or_fail(git: &str) -> bool {
        require_git_gated(git, landlock_is_required())
    }

    /// The pure decision [`require_trusted_git_or_fail`] wraps: separated so
    /// the require-vs-skip posture is testable directly against an explicit
    /// `required` flag, with no process-global `BRIDLE_REQUIRE_LANDLOCK`
    /// mutation needed to exercise the `true` branch.
    fn require_git_gated(git: &str, required: bool) -> bool {
        if std::path::Path::new(git).exists() {
            return true;
        }
        if required {
            panic!(
                "BRIDLE_REQUIRE_LANDLOCK is set but {git} is absent — the #2630 \
                 git-exec-path proof cannot be verified"
            );
        }
        eprintln!("skipping: no {git} on this host");
        false
    }

    /// `true` if the caller should `return` (skip the proof). **Panics** when
    /// Landlock is *required* (`BRIDLE_REQUIRE_LANDLOCK` set, as CI does) but the
    /// kernel lacks it — so a flagged run cannot pass without actually exercising
    /// the boundary. A local run without the flag legitimately skips (#74).
    fn skip_proof_unless_landlock() -> bool {
        match proof_gate(landlock_is_supported(), landlock_is_required()) {
            ProofGate::Run => false,
            ProofGate::Skip => {
                eprintln!(
                    "skipping Landlock proof: kernel lacks Landlock \
                     (set BRIDLE_REQUIRE_LANDLOCK=1 to require it, as CI does)"
                );
                true
            }
            ProofGate::Fail => panic!(
                "BRIDLE_REQUIRE_LANDLOCK is set but this kernel lacks Landlock — the \
                 fs_write/fs_read kernel-enforcement proofs cannot be verified (#74)"
            ),
        }
    }

    #[test]
    fn proof_gate_required_but_unsupported_is_a_failure() {
        assert_eq!(proof_gate(true, false), ProofGate::Run);
        assert_eq!(proof_gate(true, true), ProofGate::Run);
        assert_eq!(proof_gate(false, false), ProofGate::Skip);
        // The crux (#74): required + unsupported must FAIL, never silently skip,
        // so CI cannot pass without exercising the kernel boundary.
        assert_eq!(proof_gate(false, true), ProofGate::Fail);
    }

    /// #2630 round 4, finding 2's require-not-skip half: a missing fixture
    /// `git` with `required = false` legitimately skips (no panic).
    #[test]
    fn require_git_gated_skips_a_missing_git_when_not_required() {
        assert!(!require_git_gated(
            "/definitely/does/not/exist/git-2630",
            false
        ));
    }

    /// The crux: the SAME missing `git`, with `required = true`, must FAIL
    /// rather than silently return `false` — mirroring `proof_gate`'s own
    /// required-but-unsupported posture (#74), applied to a different
    /// missing prerequisite.
    #[test]
    #[should_panic(expected = "is absent")]
    fn require_git_gated_panics_on_a_missing_git_when_required() {
        require_git_gated("/definitely/does/not/exist/git-2630", true);
    }

    #[test]
    fn fs_write_is_kernel_enforced_outside_scope_denied_inside_allowed() {
        if skip_proof_unless_landlock() {
            return;
        }

        let allowed = unique_dir("allowed");
        let forbidden = unique_dir("forbidden");
        let allowed_t = allowed.clone();
        let forbidden_t = forbidden.clone();

        // `restrict_self` is per-thread and irreversible, so confine a throwaway
        // thread rather than poisoning the test runner's threads.
        let (inside_ok, outside) = std::thread::spawn(move || {
            let cav = Caveats {
                fs_write: Scope::only([allowed_t.to_string_lossy().into_owned()]),
                ..Caveats::top()
            };
            LandlockSandbox::new().apply(&cav).expect("apply landlock");

            let inside = fs::write(allowed_t.join("ok.txt"), b"hi");
            let outside = fs::write(forbidden_t.join("escape.txt"), b"nope");
            (inside.is_ok(), outside)
        })
        .join()
        .unwrap();

        assert!(inside_ok, "writing within fs_write scope must succeed");
        let err = outside.expect_err("writing outside fs_write scope must be denied by Landlock");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied,
            "the denial must come from the kernel (EACCES)"
        );

        let _ = fs::remove_dir_all(&allowed);
        let _ = fs::remove_dir_all(&forbidden);
    }

    /// A read root bound to a HELD descriptor anchors on the object, not the
    /// pathname: after the held directory is renamed aside and a different
    /// directory takes its path, the confined thread still reads the held
    /// object at its new name and is denied at the old path. Control: the same
    /// grant with no held root follows the pathname — the swapped-in directory
    /// is readable and the held object is not.
    #[test]
    fn held_read_root_anchors_on_the_object_not_the_path() {
        if skip_proof_unless_landlock() {
            return;
        }
        let root = unique_dir("held-root");
        let aside = std::path::PathBuf::from(format!("{}.aside", root.display()));
        fs::write(root.join("held.txt"), b"held").unwrap();
        let fd = std::os::fd::OwnedFd::from(fs::File::open(&root).unwrap());
        let cav = Caveats {
            fs_read: Scope::only([root.to_string_lossy().into_owned()]),
            ..Caveats::top()
        };
        // Bind while `root` still names the held object — the validated
        // constructor checks `(dev, ino)` NOW, before the swap below.
        let held = crate::HeldReadRoot::bind(root.to_string_lossy().into_owned(), fd)
            .expect("bind before the swap");

        // The swap: the held directory goes aside; a different directory takes
        // its pathname.
        fs::rename(&root, &aside).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("swapped.txt"), b"swapped").unwrap();

        let (root_t, aside_t, cav_t) = (root.clone(), aside.clone(), cav.clone());
        let (held_read, swapped_read) = std::thread::spawn(move || {
            LandlockSandbox::new()
                .apply_with_held_roots(&cav_t, &[held])
                .expect("apply landlock");
            (
                fs::read(aside_t.join("held.txt")),
                fs::read(root_t.join("swapped.txt")),
            )
        })
        .join()
        .unwrap();
        assert_eq!(
            held_read.expect("the held object stays readable at its new name"),
            b"held"
        );
        assert_eq!(
            swapped_read
                .expect_err("the swapped-in directory is outside the held fence")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );

        // Control: the path-opened rule follows the pathname.
        let (root_c, aside_c) = (root.clone(), aside.clone());
        let (held_read, swapped_read) = std::thread::spawn(move || {
            LandlockSandbox::new().apply(&cav).expect("apply landlock");
            (
                fs::read(aside_c.join("held.txt")),
                fs::read(root_c.join("swapped.txt")),
            )
        })
        .join()
        .unwrap();
        assert!(held_read.is_err(), "control: the renamed-aside object is ungranted");
        assert_eq!(
            swapped_read.expect("control: the path rule admits the swapped-in directory"),
            b"swapped"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&aside);
    }

    /// #144 (I5-B): the Landlock read base is config-driven. Widening
    /// `base_read_paths` lets a confined thread read a path that is otherwise
    /// outside `fs_read` scope — proving `apply` reads `self.policy`, not the old
    /// module const. The control (default policy) denies the same read.
    #[test]
    fn landlock_config_widens_base_read() {
        if skip_proof_unless_landlock() {
            return;
        }
        let allowed = unique_dir("cfg-allowed");
        let extra = unique_dir("cfg-extra");
        fs::write(extra.join("data.txt"), b"configured").unwrap();

        let cav = Caveats {
            fs_read: Scope::only([allowed.to_string_lossy().into_owned()]),
            ..Caveats::top()
        };

        // Control: with the DEFAULT policy the out-of-scope `extra` dir is denied.
        let (extra_c, cav_c) = (extra.clone(), cav.clone());
        let denied = std::thread::spawn(move || {
            LandlockSandbox::new().apply(&cav_c).expect("apply");
            fs::read(extra_c.join("data.txt"))
        })
        .join()
        .unwrap();
        assert!(
            denied.is_err(),
            "default base read must NOT include the out-of-scope extra dir"
        );

        // Widened policy: add `extra` to base_read_paths → the same read succeeds.
        let mut base = SandboxPolicy::default().base_read_paths;
        base.extra.push(extra.to_string_lossy().into_owned());
        let policy = Arc::new(SandboxPolicy {
            base_read_paths: base,
            ..SandboxPolicy::default()
        });
        let extra_w = extra.clone();
        let allowed_read = std::thread::spawn(move || {
            LandlockSandbox::with_policy(policy)
                .apply(&cav)
                .expect("apply");
            fs::read(extra_w.join("data.txt"))
        })
        .join()
        .unwrap();
        assert!(
            allowed_read.is_ok(),
            "config-widened base_read_paths must allow the extra dir: {allowed_read:?}"
        );

        let _ = fs::remove_dir_all(&allowed);
        let _ = fs::remove_dir_all(&extra);
    }

    #[test]
    fn empty_fs_write_scope_denies_all_writes() {
        if skip_proof_unless_landlock() {
            return;
        }
        let dir = unique_dir("none");
        let dir_t = dir.clone();
        let outside = std::thread::spawn(move || {
            let cav = Caveats {
                fs_write: Scope::none(),
                ..Caveats::top()
            };
            LandlockSandbox::new().apply(&cav).expect("apply landlock");
            fs::write(dir_t.join("x.txt"), b"nope")
        })
        .join()
        .unwrap();
        assert_eq!(
            outside
                .expect_err("empty fs_write must deny all writes")
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_read_is_kernel_enforced_outside_scope_denied_inside_allowed() {
        if skip_proof_unless_landlock() {
            return;
        }
        let allowed = unique_dir("read-allowed");
        let forbidden = unique_dir("read-forbidden");
        // Create both files BEFORE confining (afterwards the forbidden dir is
        // unreadable, but it must already hold a file to attempt the read).
        fs::write(allowed.join("ok.txt"), b"in-scope").unwrap();
        fs::write(forbidden.join("secret.txt"), b"out-of-scope").unwrap();
        let allowed_t = allowed.clone();
        let forbidden_t = forbidden.clone();

        let (inside, outside) = std::thread::spawn(move || {
            let cav = Caveats {
                fs_read: Scope::only([allowed_t.to_string_lossy().into_owned()]),
                ..Caveats::top()
            };
            LandlockSandbox::new().apply(&cav).expect("apply landlock");
            let inside = fs::read(allowed_t.join("ok.txt"));
            let outside = fs::read(forbidden_t.join("secret.txt"));
            (inside, outside)
        })
        .join()
        .unwrap();

        assert_eq!(inside.expect("in-scope read must succeed"), b"in-scope");
        assert_eq!(
            outside
                .expect_err("reading outside fs_read scope must be denied by Landlock")
                .kind(),
            std::io::ErrorKind::PermissionDenied,
            "the denial must come from the kernel (EACCES)"
        );

        let _ = fs::remove_dir_all(&allowed);
        let _ = fs::remove_dir_all(&forbidden);
    }

    #[test]
    fn read_confined_binary_still_loads_via_base_allowlist() {
        if skip_proof_unless_landlock() {
            return;
        }
        let allowed = unique_dir("rc-allowed");
        let forbidden = unique_dir("rc-forbidden");
        fs::write(allowed.join("ok.txt"), b"hello\n").unwrap();
        fs::write(forbidden.join("secret.txt"), b"nope\n").unwrap();
        let allowed_t = allowed.clone();
        let forbidden_t = forbidden.clone();

        // Confine reads, then run a *real* dynamically-linked binary (`cat`):
        // it must still load (proving the base allow-list covers the loader and
        // libc) and read the in-scope file, but be denied the out-of-scope one.
        let (inside, outside) = std::thread::spawn(move || {
            let cav = Caveats {
                fs_read: Scope::only([allowed_t.to_string_lossy().into_owned()]),
                ..Caveats::top()
            };
            LandlockSandbox::new().apply(&cav).expect("apply landlock");
            let inside = std::process::Command::new("cat")
                .arg(allowed_t.join("ok.txt"))
                .output();
            let outside = std::process::Command::new("cat")
                .arg(forbidden_t.join("secret.txt"))
                .output();
            (inside, outside)
        })
        .join()
        .unwrap();

        let inside = inside.expect("cat must still load+run under read confinement");
        assert!(
            inside.status.success(),
            "in-scope cat must succeed: {inside:?}"
        );
        assert_eq!(inside.stdout, b"hello\n");

        let outside = outside.expect("cat launches (loader is allowed) even for a denied target");
        assert!(
            !outside.status.success(),
            "cat of an out-of-scope file must fail (read denied): {outside:?}"
        );

        let _ = fs::remove_dir_all(&allowed);
        let _ = fs::remove_dir_all(&forbidden);
    }

    #[test]
    fn fs_read_all_leaves_reads_ambient() {
        if skip_proof_unless_landlock() {
            return;
        }
        // With fs_read: All (only fs_write restricted), reads are NOT governed —
        // a path outside the write scope is still readable.
        let outside_dir = unique_dir("ambient-read");
        fs::write(outside_dir.join("readable.txt"), b"still readable").unwrap();
        let write_scope = unique_dir("ambient-write");
        let outside_t = outside_dir.clone();
        let write_t = write_scope.clone();

        let read = std::thread::spawn(move || {
            let cav = Caveats {
                fs_write: Scope::only([write_t.to_string_lossy().into_owned()]),
                ..Caveats::top() // fs_read stays All
            };
            LandlockSandbox::new().apply(&cav).expect("apply landlock");
            fs::read(outside_t.join("readable.txt"))
        })
        .join()
        .unwrap();

        assert_eq!(
            read.expect("fs_read: All must leave reads ambient"),
            b"still readable"
        );
        let _ = fs::remove_dir_all(&outside_dir);
        let _ = fs::remove_dir_all(&write_scope);
    }

    /// #57 boundary: with `exec` confined to `cat`, the granted program (and its
    /// libraries) still runs, but a DIRECT `execve` of an un-granted tool (`head`)
    /// — the `find -exec curl` escape in miniature — is kernel-denied by the
    /// `Execute` allow-list. (This is the boundary/direct-execve close, NOT the
    /// trampoline; `exec` stays reported `interceptor`, ADR 0011 D7.)
    #[test]
    fn exec_direct_execve_of_ungranted_tool_is_kernel_denied() {
        if skip_proof_unless_landlock() {
            return;
        }
        let dir = unique_dir("exec");
        fs::write(dir.join("data.txt"), b"payload\n").unwrap();
        let dir_t = dir.clone();

        let (granted, ungranted) = std::thread::spawn(move || {
            let cav = Caveats {
                exec: Scope::only(["cat".to_string()]),
                ..Caveats::top()
            };
            LandlockSandbox::new().apply(&cav).expect("apply landlock");
            let granted = std::process::Command::new("cat")
                .arg(dir_t.join("data.txt"))
                .output();
            let ungranted = std::process::Command::new("head")
                .arg(dir_t.join("data.txt"))
                .output();
            (granted, ungranted)
        })
        .join()
        .unwrap();

        let granted = granted.expect("granted `cat` must still load and run");
        assert!(
            granted.status.success(),
            "granted cat must succeed: {granted:?}"
        );
        assert_eq!(granted.stdout, b"payload\n");

        // execve of the un-granted binary is kernel-denied: std surfaces the
        // post-fork exec failure as a PermissionDenied spawn error.
        let err = ungranted.expect_err("un-granted `head` must be exec-denied by Landlock");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::PermissionDenied,
            "the denial must come from the kernel (EACCES on execve)"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// #57 adversarial sweep: with `exec` confined to `cat` and writes confined to
    /// a scratch dir, EVERY classic "make the permitted program launch something
    /// else" DIRECT-execve escape must be kernel-denied — an un-granted tool, a
    /// payload the context could write+run, a shebang script (un-granted
    /// interpreter), a symlink to an un-granted tool, and the real
    /// shells/interpreters that live under `/usr/lib*` (which a recursive lib-dir
    /// Execute grant — the narrowing this avoids — would have exposed). The
    /// granted program still works (control). (Direct-execve boundary only; the
    /// ld.so/interpreter trampoline is out of scope — `exec` stays `interceptor`.)
    #[test]
    fn exec_escape_attempts_are_all_denied() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        if skip_proof_unless_landlock() {
            return;
        }
        let scratch = unique_dir("exec-escape"); // in fs_write scope
        fs::write(scratch.join("data.txt"), b"ok\n").unwrap();

        // A real ELF the confined context could try to run from the scratch dir (a
        // "written payload"); copy an existing binary to avoid needing a compiler.
        let payload = scratch.join("payload");
        if let Ok(src) = std::fs::read("/bin/cat").or_else(|_| std::fs::read("/usr/bin/cat")) {
            fs::write(&payload, src).unwrap();
            fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // A shebang script + a symlink to an un-granted interpreter.
        let script = scratch.join("script.sh");
        fs::write(&script, b"#!/bin/sh\necho pwned\n").unwrap();
        fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = scratch.join("sh-link");
        let _ = symlink("/bin/sh", &link);

        // Real shells/interpreters that live UNDER the library tree (/usr/lib*):
        // loader-only Execute must deny them. Tested only where present.
        let lib_execs: Vec<PathBuf> = [
            "/usr/lib/klibc/bin/sh",
            "/usr/lib/initramfs-tools/bin/busybox",
            "/usr/lib/git-core/git",
        ]
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();

        let scratch_t = scratch.clone();
        let (attempts, control) = std::thread::spawn(move || {
            let cav = Caveats {
                exec: Scope::only(["cat".to_string()]),
                fs_write: Scope::only([scratch_t.to_string_lossy().into_owned()]),
                ..Caveats::top()
            };
            LandlockSandbox::new().apply(&cav).expect("apply landlock");

            let mut attempts = vec![
                (
                    "ungranted-tool".to_string(),
                    std::process::Command::new("head")
                        .arg("/etc/hostname")
                        .output(),
                ),
                (
                    "written-payload".to_string(),
                    std::process::Command::new(scratch_t.join("payload")).output(),
                ),
                (
                    "shebang-script".to_string(),
                    std::process::Command::new(scratch_t.join("script.sh")).output(),
                ),
                (
                    "symlink-to-sh".to_string(),
                    std::process::Command::new(scratch_t.join("sh-link"))
                        .arg("-c")
                        .arg("echo pwned")
                        .output(),
                ),
            ];
            for p in &lib_execs {
                attempts.push((
                    format!("under-usr-lib:{}", p.display()),
                    std::process::Command::new(p).arg("--version").output(),
                ));
            }
            // Control: the granted program still runs.
            let control = std::process::Command::new("cat")
                .arg(scratch_t.join("data.txt"))
                .output();
            (attempts, control)
        })
        .join()
        .unwrap();

        for (label, res) in attempts {
            match res {
                Err(e) => assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied,
                    "escape `{label}` failed for the wrong reason: {e:?}"
                ),
                Ok(out) => panic!(
                    "escape `{label}` was NOT denied — it ran (status {:?}, stdout {:?})",
                    out.status, out.stdout
                ),
            }
        }
        let control = control.expect("granted `cat` must still run");
        assert!(
            control.status.success() && control.stdout == b"ok\n",
            "control: {control:?}"
        );

        let _ = fs::remove_dir_all(&scratch);
    }

    /// #57 / ADR 0011 D3: when BOTH `exec` and `fs_read` are confined, the read
    /// base excludes the bin dirs — the granted program (and its libs) still
    /// loads, but an un-granted system binary is NOT readable, so it cannot be
    /// `ld.so`-trampolined (the trampoline corpus is shrunk to the granted set).
    #[test]
    fn read_base_excludes_bin_dirs_when_exec_confined() {
        if skip_proof_unless_landlock() {
            return;
        }
        let dir = unique_dir("read-narrow");
        fs::write(dir.join("data.txt"), b"payload\n").unwrap();
        let dir_t = dir.clone();

        let (granted, head_bytes) = std::thread::spawn(move || {
            let cav = Caveats {
                exec: Scope::only(["cat".to_string()]),
                fs_read: Scope::only([dir_t.to_string_lossy().into_owned()]),
                ..Caveats::top()
            };
            LandlockSandbox::new().apply(&cav).expect("apply landlock");
            // Granted `cat` loads (its binary + libs are read-allowed) and reads
            // the in-scope file.
            let granted = std::process::Command::new("cat")
                .arg(dir_t.join("data.txt"))
                .output();
            // Reading an un-granted bin-dir binary's bytes (a would-be trampoline
            // payload) is denied — the bin dirs are not in the read set.
            let head_bytes = std::fs::read("/usr/bin/head").or_else(|_| std::fs::read("/bin/head"));
            (granted, head_bytes)
        })
        .join()
        .unwrap();

        let granted = granted.expect("granted `cat` must load + run under narrowed reads");
        assert!(
            granted.status.success() && granted.stdout == b"payload\n",
            "granted cat under narrowed reads: {granted:?}"
        );
        assert!(
            head_bytes.is_err(),
            "an un-granted bin-dir binary must be unreadable (trampoline corpus shrunk): {head_bytes:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // ── ChildNetworkPolicy::DenyDirect — the seccomp socket()-family egress
    //    floor. These use safe `std::net` / `std::os::unix::net` (core forbids
    //    `unsafe`): socket *creation* itself is what the seccomp filter EACCES-
    //    fails, so a failed `bind`/`connect` at the socket step is the proof.
    //    They run on throwaway threads (seccomp, like Landlock, is per-thread and
    //    irreversible). The floor is inherited across fork/exec by kernel
    //    guarantee — descendant inheritance for the identical filter is proved
    //    end-to-end on the newt side (net_guard_executor.rs).

    /// DenyDirect under `net: none` denies AF_INET / AF_INET6 socket creation
    /// (TCP *and* UDP — the UDP/DNS leg Landlock's TCP-only rule misses) while
    /// AF_UNIX stays creatable (a path-named unix socket is fs-fenced, not a
    /// seccomp concern).
    #[test]
    fn deny_direct_seccomp_blocks_off_box_sockets_allows_af_unix() {
        if skip_proof_unless_landlock() {
            return;
        }
        let policy = std::sync::Arc::new(crate::SandboxPolicy {
            child_network: crate::ChildNetworkPolicy::DenyDirect,
            ..crate::SandboxPolicy::default()
        });
        let (udp4, udp6, tcp4, unix_ok) = std::thread::spawn(move || {
            let cav = Caveats {
                net: Scope::none(),
                ..Caveats::top()
            };
            LandlockSandbox::with_policy(policy)
                .apply(&cav)
                .expect("apply landlock + seccomp");
            let udp4 = std::net::UdpSocket::bind("127.0.0.1:0").is_err();
            let udp6 = std::net::UdpSocket::bind("[::1]:0").is_err();
            let tcp4 = std::net::TcpStream::connect("127.0.0.1:9").is_err();
            let unix_ok = std::os::unix::net::UnixDatagram::unbound().is_ok();
            (udp4, udp6, tcp4, unix_ok)
        })
        .join()
        .unwrap();
        assert!(udp4, "DenyDirect must deny AF_INET (UDP) socket creation");
        assert!(udp6, "DenyDirect must deny AF_INET6 (UDP) socket creation");
        assert!(tcp4, "DenyDirect must deny AF_INET (TCP) socket creation");
        assert!(
            unix_ok,
            "DenyDirect must still allow AF_UNIX socket creation"
        );
    }

    /// The control + backward-compat guard: the DEFAULT `LandlockOnly` policy
    /// leaves AF_INET UDP socket creation OPEN under `net: none` — Landlock's
    /// TCP-only net rule doesn't cover it. This is exactly the leak DenyDirect
    /// closes, and proves the default behavior is unchanged.
    #[test]
    fn landlock_only_default_leaves_udp_socket_creation_open() {
        if skip_proof_unless_landlock() {
            return;
        }
        // Default policy == LandlockOnly.
        let policy = std::sync::Arc::new(crate::SandboxPolicy::default());
        let udp_created = std::thread::spawn(move || {
            let cav = Caveats {
                net: Scope::none(),
                ..Caveats::top()
            };
            LandlockSandbox::with_policy(policy)
                .apply(&cav)
                .expect("apply landlock");
            std::net::UdpSocket::bind("127.0.0.1:0").is_ok()
        })
        .join()
        .unwrap();
        assert!(
            udp_created,
            "LandlockOnly (default) must leave UDP socket creation open — the leak DenyDirect closes"
        );
    }

    /// DenyDirect is inert when the caller GRANTED a net scope (they asked for
    /// egress): `net_fully_denied` is false, so no seccomp floor is installed and
    /// socket creation still works.
    #[test]
    fn deny_direct_is_inert_when_net_is_granted() {
        if skip_proof_unless_landlock() {
            return;
        }
        let policy = std::sync::Arc::new(crate::SandboxPolicy {
            child_network: crate::ChildNetworkPolicy::DenyDirect,
            ..crate::SandboxPolicy::default()
        });
        let udp_created = std::thread::spawn(move || {
            // net = All (ambient) → a granted net scope; DenyDirect must NOT fire.
            let cav = Caveats::top();
            LandlockSandbox::with_policy(policy)
                .apply(&cav)
                .expect("apply landlock");
            std::net::UdpSocket::bind("127.0.0.1:0").is_ok()
        })
        .join()
        .unwrap();
        assert!(
            udp_created,
            "DenyDirect must be inert when net is granted (caller asked for egress)"
        );
    }

    /// #2630 — a `git`-only exec grant, naming the SYSTEM `/usr/bin/git`, must
    /// admit its own internal helpers so `git worktree add` (which `execve`s
    /// `git-branch`/`git-update-ref`) succeeds under a REAL kernel-enforced
    /// Landlock fence, not merely the in-process admission check. Confirmed
    /// red on the pre-fix code (`resolve_exec_paths` admitted only the
    /// resolved `git` binary itself): `fatal: cannot exec 'branch':
    /// Permission denied`, git exit 128.
    ///
    /// Both the unconfined fixture setup AND the confined proof command go
    /// through [`hermetic_git_command`] (bridle PR #407 review, P1-3/P2-4):
    /// one constructor, `env_clear()`-based, so this test cannot silently
    /// touch the real repository the way a partial `env_remove()` denylist
    /// once did (see its doc comment).
    #[test]
    fn git_only_exec_grant_admits_worktree_add_under_real_landlock() {
        if skip_proof_unless_landlock() {
            return;
        }
        let git = "/usr/bin/git";
        if !require_trusted_git_or_fail(git) {
            return;
        }
        let root = unique_dir("git-exec-2630");
        let main = root.join("main");
        std::fs::create_dir(&main).unwrap();
        let home = root.join("home");
        std::fs::create_dir(&home).unwrap();

        let real_git = |dir: &std::path::Path, args: &[&str]| {
            let ok = hermetic_git_command(git, dir, &home)
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed");
        };
        real_git(&main, &["init", "-q"]);
        std::fs::write(main.join("seed"), "x").unwrap();
        real_git(&main, &["add", "seed"]);
        real_git(&main, &["commit", "-q", "-m", "init"]);

        let root_t = root.clone();
        let main_t = main.clone();
        let home_t = home.clone();
        // Absolute, not `../wt`: the destination must not depend on the
        // child's resolved cwd matching this exact relative hop — keeps the
        // test deterministic under heavy parallel-test-suite load.
        let wt_t = root.join("wt");
        let output = std::thread::spawn(move || {
            let cav = Caveats {
                // No `/etc/gitconfig` grant needed: `hermetic_git_command`
                // sets `GIT_CONFIG_NOSYSTEM=1`, so a confined git never
                // tries to open it in the first place.
                fs_read: Scope::only([root_t.to_string_lossy().into_owned()]),
                fs_write: Scope::only([root_t.to_string_lossy().into_owned()]),
                exec: Scope::only([git.to_string()]),
                net: Scope::none(),
                ..Caveats::top()
            };
            LandlockSandbox::new().apply(&cav).expect("apply landlock");
            hermetic_git_command(git, &main_t, &home_t)
                .arg("worktree")
                .arg("add")
                .arg("-q")
                .arg(&wt_t)
                .arg("-b")
                .arg("task")
                .output()
        })
        .join()
        .unwrap()
        .expect("spawn confined git worktree add");

        assert!(
            output.status.success(),
            "git worktree add must succeed under a git-only exec grant: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            root.join("wt").join("seed").exists(),
            "the new worktree must actually be checked out"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Env var this test binary's own re-exec checks for: its presence
    /// (any value) means "run as the dedicated hostile-env subprocess for
    /// [`hostile_inherited_git_env_is_stripped_by_hermetic_env_clear`]"
    /// instead of the normal top-level test. **Never set via
    /// `std::env::set_var` on the shared test-binary process** (bridle PR
    /// #407 review, round 3 finding 4/round 2's own P2: process-global env
    /// mutation races every other test in this binary) — set ONLY on the
    /// `Command` that spawns that one dedicated child process, below.
    const HOSTILE_ENV_SUBPROCESS_MARKER: &str = "BRIDLE_2630_HOSTILE_ENV_SUBPROCESS_ROOT";

    /// Runs (as the dedicated subprocess) the exact fixture the isolation
    /// claim is about: [`hermetic_git_command`] for BOTH the unconfined
    /// setup (`init`/`add`/`commit`) AND the Landlock-confined `worktree
    /// add`. This subprocess's OWN environment carries hostile
    /// `GIT_INDEX_FILE`/`GIT_COMMON_DIR`/`GIT_OBJECT_DIRECTORY` (set by the
    /// parent only on the `Command` that launched this process — never
    /// globally). If `hermetic_git_command`'s `env_clear()` is doing its
    /// job, none of the three reach the spawned `git` processes, so every
    /// step below succeeds exactly as the non-hostile case; if it were
    /// disabled, git would redirect into the sentinel paths this same
    /// subprocess inherited and either corrupt them or fail confusingly.
    fn run_hostile_env_subprocess_fixture(git: &str, root: &std::path::Path) {
        let main = root.join("main");
        fs::create_dir(&main).unwrap();
        let home = root.join("home");
        fs::create_dir(&home).unwrap();

        let real_git = |dir: &std::path::Path, args: &[&str]| {
            let ok = hermetic_git_command(git, dir, &home)
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed under hostile inherited env");
        };
        real_git(&main, &["init", "-q"]);
        fs::write(main.join("seed"), "x").unwrap();
        real_git(&main, &["add", "seed"]);
        real_git(&main, &["commit", "-q", "-m", "init"]);

        let git = git.to_string();
        let wt = root.join("wt");
        let confined_ok = {
            let git = git.clone();
            let main = main.clone();
            let home = home.clone();
            let root = root.to_path_buf();
            let wt = wt.clone();
            std::thread::spawn(move || {
                let cav = Caveats {
                    fs_read: Scope::only([root.to_string_lossy().into_owned()]),
                    fs_write: Scope::only([root.to_string_lossy().into_owned()]),
                    exec: Scope::only([git.clone()]),
                    net: Scope::none(),
                    ..Caveats::top()
                };
                LandlockSandbox::new().apply(&cav).expect("apply landlock");
                hermetic_git_command(&git, &main, &home)
                    .arg("worktree")
                    .arg("add")
                    .arg("-q")
                    .arg(&wt)
                    .arg("-b")
                    .arg("task")
                    .status()
                    .unwrap()
                    .success()
            })
            .join()
            .unwrap()
        };
        assert!(
            confined_ok,
            "confined git worktree add failed under hostile inherited env"
        );
        assert!(
            wt.join("seed").exists(),
            "confined worktree add must actually check out its seed file"
        );
    }

    /// #2630 round 4 (bridle PR #407 review, round 3 finding 2): the
    /// hostile-`GIT_EXEC_PATH`-style claim needs a real adversary, not
    /// sentinel files nothing ever pointed at (round 3's version). This
    /// spawns a DEDICATED subprocess — re-executing this very test binary
    /// with `--exact` against only this one test — whose `Command`
    /// environment (not this process's own) carries hostile
    /// `GIT_INDEX_FILE`/`GIT_COMMON_DIR`/`GIT_OBJECT_DIRECTORY` pointed at
    /// disposable sentinels. Inside that subprocess,
    /// [`run_hostile_env_subprocess_fixture`] runs the real fixture (setup
    /// AND the confined `worktree add`) through [`hermetic_git_command`];
    /// this parent process then inspects the sentinels afterward. No
    /// process-global env mutation anywhere in this process.
    #[test]
    fn hostile_inherited_git_env_is_stripped_by_hermetic_env_clear() {
        // Re-entry: this same test, run again as the dedicated subprocess.
        if let Ok(root) = std::env::var(HOSTILE_ENV_SUBPROCESS_MARKER) {
            let git = std::env::var("BRIDLE_2630_SUBPROCESS_GIT")
                .expect("parent must pass the git path to the subprocess");
            run_hostile_env_subprocess_fixture(&git, std::path::Path::new(&root));
            return;
        }

        if skip_proof_unless_landlock() {
            return;
        }
        let git = "/usr/bin/git";
        if !require_trusted_git_or_fail(git) {
            return;
        }

        let root = unique_dir("hostile-env-2630");
        let sentinel_index = unique_dir("hostile-sentinel-index").join("index");
        let sentinel_common = unique_dir("hostile-sentinel-common");
        let sentinel_objects = unique_dir("hostile-sentinel-objects");
        fs::write(&sentinel_index, b"untouched\n").unwrap();
        let sentinel_index_before = fs::read(&sentinel_index).unwrap();

        let exe = std::env::current_exe().expect("current_exe must resolve for the re-exec proof");
        let output = Command::new(&exe)
            .arg("--exact")
            .arg("sandbox::landlock_kernel_tests::hostile_inherited_git_env_is_stripped_by_hermetic_env_clear")
            .arg("--nocapture")
            .arg("--test-threads=1")
            // Explicit, minimal env: this Command's env is what the
            // subprocess inherits, NOT this test's own process env (which
            // is never mutated). PATH is needed for the re-executed test
            // binary's own machinery; HOME/TMPDIR are left unset — the
            // subprocess only ever touches paths this parent hands it.
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("BRIDLE_REQUIRE_LANDLOCK", "1")
            .env(HOSTILE_ENV_SUBPROCESS_MARKER, root.to_string_lossy().as_ref())
            .env("BRIDLE_2630_SUBPROCESS_GIT", git)
            // The hostile ambient redirection targets: if hermetic_git_command
            // ever inherited these instead of clearing them, git would
            // redirect its index/object-store operations straight into them.
            .env("GIT_INDEX_FILE", &sentinel_index)
            .env("GIT_COMMON_DIR", &sentinel_common)
            .env("GIT_OBJECT_DIRECTORY", &sentinel_objects)
            .output()
            .expect("spawn hostile-env subprocess");

        assert!(
            output.status.success(),
            "hostile-env subprocess fixture failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(&sentinel_index).unwrap(),
            sentinel_index_before,
            "a hostile inherited GIT_INDEX_FILE must never be honored: \
             hermetic_git_command's env_clear() must have stripped it"
        );
        assert!(
            fs::read_dir(&sentinel_common).unwrap().next().is_none(),
            "a hostile inherited GIT_COMMON_DIR must never be honored"
        );
        assert!(
            fs::read_dir(&sentinel_objects).unwrap().next().is_none(),
            "a hostile inherited GIT_OBJECT_DIRECTORY must never be honored"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(sentinel_index.parent().unwrap());
        let _ = fs::remove_dir_all(&sentinel_common);
        let _ = fs::remove_dir_all(&sentinel_objects);
    }
}

// Real kernel-enforcement proof for macOS Seatbelt. Only meaningful on macOS
// with the feature; it asserts the leash is the *kernel's* (sandbox-exec's),
// not ours — the spawned child's own out-of-scope writes/reads are denied even
// though L2 cannot see its syscalls. Mirrors the Landlock proofs above.
#[cfg(all(target_os = "macos", feature = "macos-seatbelt", test))]
mod seatbelt_kernel_tests {
    use super::*;
    use crate::Scope;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Whether a proof should run, skip, or hard-**FAIL** — the same gate as the
    /// Landlock proofs (#74): *required but unsupported is a FAILURE*, so a
    /// macOS CI job that sets `BRIDLE_REQUIRE_SEATBELT` can never go green with
    /// the kernel boundary unexercised.
    #[derive(Debug, PartialEq, Eq)]
    enum ProofGate {
        Run,
        Skip,
        Fail,
    }

    fn proof_gate(supported: bool, required: bool) -> ProofGate {
        match (supported, required) {
            (true, _) => ProofGate::Run,
            (false, true) => ProofGate::Fail,
            (false, false) => ProofGate::Skip,
        }
    }

    /// `true` if the caller should skip the proof. **Panics** when Seatbelt is
    /// *required* (`BRIDLE_REQUIRE_SEATBELT` set, as a macOS CI job does) but the
    /// host lacks `sandbox-exec`. A local run without the flag legitimately skips.
    fn skip_proof_unless_seatbelt() -> bool {
        let required = seatbelt_required();
        match proof_gate(seatbelt_is_supported(), required) {
            ProofGate::Run => false,
            ProofGate::Skip => {
                eprintln!(
                    "skipping Seatbelt proof: /usr/bin/sandbox-exec unavailable \
                     (set BRIDLE_REQUIRE_SEATBELT=1 to require it, as macOS CI does)"
                );
                true
            }
            ProofGate::Fail => panic!(
                "BRIDLE_REQUIRE_SEATBELT is set but /usr/bin/sandbox-exec is unavailable — \
                 the fs_write/fs_read kernel-enforcement proofs cannot be verified"
            ),
        }
    }

    fn seatbelt_required() -> bool {
        std::env::var("BRIDLE_REQUIRE_SEATBELT")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    }

    fn fail_required_or_skip(reason: &str) {
        if seatbelt_required() {
            panic!("required Seatbelt proof unavailable: {reason}");
        }
        eprintln!("skipping optional Seatbelt proof: {reason}");
    }

    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut d = std::env::temp_dir();
        d.push(format!(
            "agent-bridle-sb-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// Spawn `program args` through the real `sandbox-exec` wrapper that
    /// [`SeatbeltSandbox::command_prefix`] builds for `cav`, and return its exit
    /// status. This exercises the *production* profile path end to end.
    fn run_wrapped(cav: &Caveats, program: &str, args: &[&str]) -> std::process::ExitStatus {
        let prefix = SeatbeltSandbox::new()
            .command_prefix(cav)
            .expect("a restricted axis must yield a wrapper prefix");
        assert!(!prefix.is_empty(), "expected a sandbox-exec wrapper");
        std::process::Command::new(&prefix[0])
            .args(&prefix[1..])
            .arg(program)
            .args(args)
            .status()
            .expect("spawn sandbox-exec")
    }

    #[test]
    fn proof_gate_required_but_unsupported_is_a_failure() {
        assert_eq!(proof_gate(true, false), ProofGate::Run);
        assert_eq!(proof_gate(true, true), ProofGate::Run);
        assert_eq!(proof_gate(false, false), ProofGate::Skip);
        assert_eq!(proof_gate(false, true), ProofGate::Fail);
    }

    #[test]
    fn fs_write_is_kernel_enforced_outside_scope_denied_inside_allowed() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        let allowed = unique_dir("w-allowed");
        let forbidden = unique_dir("w-forbidden");
        let cav = Caveats {
            fs_write: Scope::only([allowed.to_string_lossy().into_owned()]),
            ..Caveats::top()
        };

        let inside = run_wrapped(
            &cav,
            "/usr/bin/touch",
            &[allowed.join("ok.txt").to_str().unwrap()],
        );
        assert!(
            inside.success(),
            "writing within fs_write scope must succeed"
        );
        assert!(
            allowed.join("ok.txt").exists(),
            "the in-scope file must exist"
        );

        let outside = run_wrapped(
            &cav,
            "/usr/bin/touch",
            &[forbidden.join("escape.txt").to_str().unwrap()],
        );
        assert!(
            !outside.success(),
            "the kernel must deny a write outside fs_write scope"
        );
        assert!(
            !forbidden.join("escape.txt").exists(),
            "the out-of-scope file must NOT have been created"
        );

        let _ = fs::remove_dir_all(&allowed);
        let _ = fs::remove_dir_all(&forbidden);
    }

    #[test]
    fn empty_fs_write_scope_denies_all_writes() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        let dir = unique_dir("w-none");
        let cav = Caveats {
            fs_write: Scope::none(),
            ..Caveats::top()
        };
        let target = dir.join("x.txt");
        let prefix = SeatbeltSandbox::new().command_prefix(&cav).expect("prefix");
        let out = std::process::Command::new(&prefix[0])
            .args(&prefix[1..])
            .arg("/usr/bin/touch")
            .arg(&target)
            .output()
            .expect("spawn sandbox-exec");
        assert!(!out.status.success(), "empty fs_write must deny all writes");
        // Positive control: the failure is the *kernel* denying the write (EPERM),
        // not a spurious touch error — so this assertion cannot pass vacuously.
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("Operation not permitted"),
            "denial must be a sandbox EPERM, got: {stderr:?}"
        );
        assert!(!target.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_read_is_kernel_enforced_outside_scope_denied_inside_allowed() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        let allowed = unique_dir("r-allowed");
        let forbidden = unique_dir("r-forbidden");
        fs::write(allowed.join("ok.txt"), b"in-scope").unwrap();
        fs::write(forbidden.join("secret.txt"), b"out-of-scope").unwrap();
        let cav = Caveats {
            fs_read: Scope::only([allowed.to_string_lossy().into_owned()]),
            ..Caveats::top()
        };

        // A real dynamically-linked binary (`cat`) must still load (the base
        // allow-list covers dyld) and read the in-scope file …
        let inside = run_wrapped(
            &cav,
            "/bin/cat",
            &[allowed.join("ok.txt").to_str().unwrap()],
        );
        assert!(
            inside.success(),
            "in-scope cat must load and read under read-confinement"
        );
        // … but be denied the out-of-scope one.
        let outside = run_wrapped(
            &cav,
            "/bin/cat",
            &[forbidden.join("secret.txt").to_str().unwrap()],
        );
        assert!(
            !outside.success(),
            "reading outside fs_read scope must be kernel-denied"
        );

        let _ = fs::remove_dir_all(&allowed);
        let _ = fs::remove_dir_all(&forbidden);
    }

    #[test]
    fn net_fully_denied_kernel_blocks_direct_socket_egress() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        let curl = "/usr/bin/curl";
        if !std::path::Path::new(curl).exists() {
            eprintln!("skipping: no curl(1) on this host");
            return;
        }
        let cav = Caveats {
            net: Scope::none(),
            ..Caveats::top()
        };
        // Positive control: a benign NON-network command under the SAME net:none
        // profile must succeed — proving the profile parsed and only egress is
        // denied. Without this, a malformed `(deny network*)` (sandbox-exec exit
        // 65, child never launches) would let the denial assertion pass vacuously.
        let benign = run_wrapped(&cav, "/bin/echo", &["ok"]);
        assert!(
            benign.success(),
            "net:none must still allow non-network commands (profile must parse)"
        );
        // Egress denied: curl to a literal IP (no DNS) exits **7** ("couldn't
        // connect") because the socket is kernel-denied immediately. Asserting
        // exactly 7 — not merely non-zero — rules out the vacuous passes: a
        // no-egress host times out (28), a broken profile never launches the child
        // (65). `--max-time` bounds it regardless.
        let confined = run_wrapped(&cav, curl, &["-sS", "--max-time", "5", "http://1.1.1.1/"]);
        assert_eq!(
            confined.code(),
            Some(7),
            "egress under net:none must be kernel-denied at the socket (curl exit 7)"
        );
    }

    /// Direct-wrapper inheritance proof independent of admission: a live loopback
    /// listener makes the destination reachable, yet a generation-2 descendant
    /// under the production `net:none` profile receives curl's exact socket-denied
    /// exit 7. This proves inheritance of the direct-network floor only; restricted
    /// network admission remains held.
    #[test]
    fn net_none_direct_floor_is_inherited_by_a_grandchild() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        if !std::path::Path::new("/usr/bin/curl").exists() {
            eprintln!("skipping: no curl(1) on this host");
            return;
        }
        let cav = Caveats {
            net: Scope::none(),
            ..Caveats::top()
        };
        let benign = run_wrapped(&cav, "/bin/sh", &["-c", "/bin/sh -c /usr/bin/true"]);
        assert!(
            benign.success(),
            "a benign generation-2 descendant must run under the profile"
        );

        let listener = spawn_loopback_http("127.0.0.1:0").expect("bind live loopback listener");
        let url = format!("http://127.0.0.1:{}/", listener.port());
        let script = format!("/bin/sh -c '/usr/bin/curl -sS --max-time 5 {url}'");
        let denied = run_wrapped(&cav, "/bin/sh", &["-c", &script]);
        assert_eq!(
            denied.code(),
            Some(7),
            "the generation-2 curl must inherit the direct-network deny (exact exit 7)"
        );
    }

    /// The ZERO Mach-lookup floor still keeps a representative build shell
    /// runnable — `/bin/sh` needs no named Mach service to start and exit
    /// (agent-bridle#405 breakage measurement). Operational evidence, not a
    /// bounded-authority claim.
    #[test]
    fn net_none_mach_deny_still_runs_a_build_tool() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        let cav = Caveats {
            net: Scope::none(),
            ..Caveats::top()
        };
        let sh = run_wrapped(&cav, "/bin/sh", &["-c", "exit 0"]);
        assert!(
            sh.success(),
            "the net:none zero Mach floor must keep /bin/sh runnable"
        );
    }

    struct DeputyProbe {
        dir: PathBuf,
        binary: PathBuf,
    }

    /// Compile the background-URLSession characterization probe with the exact
    /// compiler path returned by xcrun.
    fn build_deputy_probe() -> Result<DeputyProbe, String> {
        let found = std::process::Command::new("/usr/bin/xcrun")
            .args(["--find", "swiftc"])
            .output()
            .map_err(|e| format!("launch xcrun --find swiftc: {e}"))?;
        if !found.status.success() {
            return Err(format!(
                "xcrun could not find swiftc: {}",
                String::from_utf8_lossy(&found.stderr)
            ));
        }
        let swiftc = String::from_utf8(found.stdout)
            .map_err(|e| format!("xcrun returned non-UTF-8 swiftc path: {e}"))?;
        let swiftc = swiftc.trim();
        if swiftc.is_empty() {
            return Err("xcrun returned an empty swiftc path".to_string());
        }
        let sdk = std::process::Command::new("/usr/bin/xcrun")
            .args(["--sdk", "macosx", "--show-sdk-path"])
            .output()
            .map_err(|e| format!("launch xcrun --show-sdk-path: {e}"))?;
        if !sdk.status.success() {
            return Err(format!(
                "xcrun could not find the macOS SDK: {}",
                String::from_utf8_lossy(&sdk.stderr)
            ));
        }
        let sdk = String::from_utf8(sdk.stdout)
            .map_err(|e| format!("xcrun returned a non-UTF-8 SDK path: {e}"))?;
        let sdk = sdk.trim();
        if sdk.is_empty() {
            return Err("xcrun returned an empty macOS SDK path".to_string());
        }
        let dir = unique_dir("deputy");
        let src = dir.join("deputy.swift");
        let bin = dir.join("deputy");
        fs::write(
            &src,
            r#"import Darwin
import Foundation
final class D: NSObject, URLSessionDownloadDelegate {
  let done = DispatchSemaphore(value: 0)
  private let lock = NSLock()
  private var outcome = "callback_timeout"
  private var finished = false
  private func finish(_ value: String) {
    lock.lock(); defer { lock.unlock() }
    if !finished { finished = true; outcome = value; done.signal() }
  }
  func value() -> String { lock.lock(); defer { lock.unlock() }; return outcome }
  func urlSession(_ s: URLSession, downloadTask t: URLSessionDownloadTask, didFinishDownloadingTo l: URL) { finish("callback_success") }
  func urlSession(_ s: URLSession, task: URLSessionTask, didCompleteWithError e: Error?) {
    if let e = e { finish("callback_error:\(e.localizedDescription)") }
  }
}
guard CommandLine.arguments.count == 2, let url = URL(string: CommandLine.arguments[1]) else {
  print("launch_error:expected one URL"); exit(2)
}
let d = D()
let cfg = URLSessionConfiguration.background(withIdentifier: "probe.deputy.\(ProcessInfo.processInfo.processIdentifier).\(UUID().uuidString)")
cfg.isDiscretionary = false
cfg.requestCachePolicy = .reloadIgnoringLocalAndRemoteCacheData
let session = URLSession(configuration: cfg, delegate: d, delegateQueue: nil)
let request = URLRequest(url: url, cachePolicy: .reloadIgnoringLocalAndRemoteCacheData, timeoutInterval: 20)
session.downloadTask(with: request).resume()
if d.done.wait(timeout: .now() + 25) == .timedOut {
  print("callback_timeout"); exit(3)
}
print(d.value())
"#,
        )
        .map_err(|e| format!("write deputy source: {e}"))?;
        let src_str = src
            .to_str()
            .ok_or_else(|| "deputy source path is not UTF-8".to_string())?;
        let bin_str = bin
            .to_str()
            .ok_or_else(|| "deputy binary path is not UTF-8".to_string())?;
        let module_cache = dir.join("module-cache");
        fs::create_dir_all(&module_cache).map_err(|e| format!("create Swift module cache: {e}"))?;
        let built = std::process::Command::new(swiftc)
            .env("CLANG_MODULE_CACHE_PATH", &module_cache)
            .env("SWIFT_MODULE_CACHE_PATH", &module_cache)
            .args(["-sdk", sdk, "-O", src_str, "-o", bin_str])
            .output()
            .map_err(|e| format!("launch xcrun-selected swiftc: {e}"))?;
        if !built.status.success() || !bin.exists() {
            return Err(format!(
                "swiftc failed: {}",
                String::from_utf8_lossy(&built.stderr)
            ));
        }
        Ok(DeputyProbe { dir, binary: bin })
    }

    fn run_deputy(
        prefix: &[String],
        deputy: &Path,
        url: &str,
    ) -> Result<std::process::Output, String> {
        let (program, args) = prefix
            .split_first()
            .ok_or_else(|| "empty Seatbelt prefix".to_string())?;
        std::process::Command::new(program)
            .args(args)
            .arg(deputy)
            .arg(url)
            .output()
            .map_err(|e| format!("launch deputy through sandbox-exec: {e}"))
    }

    fn callback_output(output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn unique_probe_url(phase: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        format!(
            "https://captive.apple.com/hotspot-detect.html?agent_bridle_e4={}-{}-{phase}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn sha256(path: &Path) -> String {
        let output = std::process::Command::new("/usr/bin/shasum")
            .args(["-a", "256"])
            .arg(path)
            .output()
            .expect("launch shasum");
        assert!(output.status.success(), "shasum must succeed");
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .next()
            .expect("shasum digest")
            .to_string()
    }

    fn system_text(program: &str, args: &[&str]) -> String {
        std::process::Command::new(program)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().replace(' ', "_"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unavailable".to_string())
    }

    fn evidence_env(names: &[&str]) -> String {
        names
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
            .unwrap_or_else(|| "unset".to_string())
    }

    /// Strict A/B/A characterization of the selected Mach floor. Both A legs use
    /// the typed network-deny-only/Mach-ambient profile and must complete a unique,
    /// uncached HTTPS background transfer. The production B leg must launch and
    /// exit successfully through an explicit callback_error. This proves the
    /// incremental behavior of this floor, not global deputy closure; support stays
    /// held because restricted network projection is Unknown.
    #[test]
    fn net_none_mach_floor_has_strict_ambient_closed_ambient_differential() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        let probe = match build_deputy_probe() {
            Ok(probe) => probe,
            Err(reason) => {
                fail_required_or_skip(&reason);
                return;
            }
        };
        let cav = Caveats {
            net: Scope::none(),
            ..Caveats::top()
        };
        let sandbox = SeatbeltSandbox::new();
        let ambient = sandbox
            .net_none_ambient_mach_prefix(&cav)
            .expect("typed ambient-Mach characterization prefix");
        let production = sandbox
            .command_prefix(&cav)
            .expect("production net:none prefix");

        let before = match run_deputy(&ambient, &probe.binary, &unique_probe_url("before")) {
            Ok(output) => output,
            Err(reason) => {
                let _ = fs::remove_dir_all(&probe.dir);
                fail_required_or_skip(&reason);
                return;
            }
        };
        let before_marker = callback_output(&before);
        if !before.status.success() || !before_marker.contains("callback_success") {
            let reason = format!(
                "ambient-before baseline did not succeed: status={:?} stdout={before_marker:?} stderr={:?}",
                before.status.code(),
                String::from_utf8_lossy(&before.stderr)
            );
            let _ = fs::remove_dir_all(&probe.dir);
            fail_required_or_skip(&reason);
            return;
        }

        let closed = match run_deputy(
            &production,
            &probe.binary,
            &unique_probe_url("production-closed"),
        ) {
            Ok(output) => output,
            Err(reason) => {
                let _ = fs::remove_dir_all(&probe.dir);
                fail_required_or_skip(&reason);
                return;
            }
        };
        let closed_marker = callback_output(&closed);
        assert!(
            !closed_marker.contains("callback_success"),
            "production Mach floor unexpectedly permitted the characterized transfer"
        );
        if !closed.status.success() || closed_marker.contains("callback_timeout") {
            let reason = format!(
                "production leg did not exit via callback: status={:?} stdout={closed_marker:?} stderr={:?}",
                closed.status.code(),
                String::from_utf8_lossy(&closed.stderr)
            );
            let _ = fs::remove_dir_all(&probe.dir);
            fail_required_or_skip(&reason);
            return;
        }
        assert!(
            closed_marker.contains("callback_error"),
            "production leg must report an explicit callback_error: {closed_marker:?}"
        );

        let after = match run_deputy(&ambient, &probe.binary, &unique_probe_url("after")) {
            Ok(output) => output,
            Err(reason) => {
                let _ = fs::remove_dir_all(&probe.dir);
                fail_required_or_skip(&reason);
                return;
            }
        };
        let after_marker = callback_output(&after);
        if !after.status.success() || !after_marker.contains("callback_success") {
            let reason = format!(
                "ambient-after baseline did not succeed: status={:?} stdout={after_marker:?} stderr={:?}",
                after.status.code(),
                String::from_utf8_lossy(&after.stderr)
            );
            let _ = fs::remove_dir_all(&probe.dir);
            fail_required_or_skip(&reason);
            return;
        }

        let profile_path = probe.dir.join("production.sb");
        fs::write(
            &profile_path,
            production.get(2).expect("production profile argument"),
        )
        .expect("write profile evidence");
        eprintln!(
            "SEATBELT_E4_EVIDENCE head_sha={} merge_sha={} sw_vers={} kernel={} arch={} profile_sha256={} probe_sha256={} phases=ambient_before:callback_success,production_closed:callback_error,ambient_after:callback_success",
            evidence_env(&[
                "BRIDLE_E4_HEAD_SHA",
                "BRIDLE_HEAD_SHA",
                "PR_HEAD_SHA",
                "GITHUB_HEAD_SHA",
            ]),
            evidence_env(&["BRIDLE_MERGE_SHA", "MERGE_SHA", "GITHUB_SHA"]),
            system_text("/usr/bin/sw_vers", &["-productVersion"]),
            system_text("/usr/bin/uname", &["-r"]),
            system_text("/usr/bin/uname", &["-m"]),
            sha256(&profile_path),
            sha256(&probe.binary),
        );
        let _ = fs::remove_dir_all(&probe.dir);
    }

    /// A one-shot loopback listener answering a single HTTP request, so an ALLOW
    /// assertion tests a *reachable* socket (curl 0) — not "connection refused"
    /// (also 7). Detached, so an unexpected deny can't hang the test on a
    /// never-accepted connection. Returns the bound `SocketAddr`, or `None` if the
    /// family is unavailable on this host (e.g. no `::1`), so a caller can skip.
    fn spawn_loopback_http(bind: &str) -> Option<std::net::SocketAddr> {
        let listener = std::net::TcpListener::bind(bind).ok()?;
        let addr = listener.local_addr().ok()?;
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok");
            }
        });
        Some(addr)
    }

    /// The numeric-uid control for the zero-floor differential, validated so it
    /// can never be satisfied by a failed or empty child: `id -u` must exit
    /// successfully and print exactly one non-empty ASCII-decimal token. The
    /// denied legs of the proof compare against THIS value, so an `id` that
    /// died with empty stdout cannot match an empty control (review of #406,
    /// finding 1). Pure over the captured output; pinned by
    /// `validated_uid_rejects_failed_or_non_numeric_controls`.
    fn validated_uid(output: &std::process::Output) -> Result<String, String> {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !output.status.success() {
            return Err(format!(
                "`id -u` control did not exit successfully: status={:?} stdout={text:?}",
                output.status.code()
            ));
        }
        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "`id -u` control did not print a non-empty ASCII-decimal uid: {text:?}"
            ));
        }
        Ok(text)
    }

    /// A failed, empty, or non-numeric uid control is refused — so the denied
    /// legs of the differential below cannot pass against an empty string.
    #[test]
    fn validated_uid_rejects_failed_or_non_numeric_controls() {
        use std::os::unix::process::ExitStatusExt;
        let out = |code: i32, stdout: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(validated_uid(&out(0, "501\n")).unwrap(), "501");
        assert!(validated_uid(&out(1, "501\n")).is_err(), "failed status");
        assert!(validated_uid(&out(0, "")).is_err(), "empty stdout");
        assert!(validated_uid(&out(0, "\n")).is_err(), "whitespace only");
        assert!(validated_uid(&out(0, "runner")).is_err(), "not numeric");
        assert!(validated_uid(&out(0, "501 502")).is_err(), "not one token");
    }

    /// The Mach floor is ZERO and a `mach:` grant re-opens the ONE service it
    /// names, kernel-enforced (agent-bridle#405). Measured differential with
    /// positive controls, for `com.apple.system.opendirectoryd.libinfo`:
    /// uid→name resolution (`id -un`) goes through that service; unconfined it
    /// prints the user name (control: the host resolves it, and the name is
    /// distinct from the validated numeric uid); under `net:none` the zero
    /// floor denies the lookup and `id` falls back to the numeric uid; under
    /// `net: {mach:…libinfo}` the name resolves again; under an UNRELATED grant
    /// (`SecurityServer`) it does not. Under every confined profile a benign
    /// command (`/bin/echo`) still runs, proving each profile parsed. This
    /// characterizes the libinfo grant and the literal rule it emits; it does
    /// not characterize any other service's transitive authority. Deterministic
    /// and offline: no network, no compiler, no timing.
    #[test]
    fn net_none_mach_floor_is_zero_and_a_named_grant_reopens_that_service() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        let id = "/usr/bin/id";
        if !std::path::Path::new(id).exists() {
            fail_required_or_skip("no id(1) on this host");
            return;
        }
        let stdout =
            |o: &std::process::Output| String::from_utf8_lossy(&o.stdout).trim().to_string();
        // Control 1: a validated numeric uid (success + non-empty ASCII decimal).
        let uid = match validated_uid(
            &std::process::Command::new(id)
                .arg("-u")
                .output()
                .expect("run id -u"),
        ) {
            Ok(uid) => uid,
            Err(reason) => {
                fail_required_or_skip(&format!("positive control: {reason}"));
                return;
            }
        };
        // Control 2: the host resolves a NAME distinct from that uid, or the
        // differential is meaningless — refuse to pass vacuously.
        let unconfined = std::process::Command::new(id)
            .arg("-un")
            .output()
            .expect("run id unconfined");
        let name = stdout(&unconfined);
        if !unconfined.status.success() || name.is_empty() || name == uid {
            fail_required_or_skip(&format!(
                "positive control: unconfined `id -un` did not resolve a user name (got {name:?}, uid {uid:?})"
            ));
            return;
        }

        // Denied leg: the documented `id` fallback is the exact validated uid on
        // stdout. Exit status is NOT assumed (a denied lookup may or may not
        // fail the tool); the non-empty exact-uid match is the evidence.
        let none = Caveats {
            net: Scope::none(),
            ..Caveats::top()
        };
        assert!(
            run_wrapped(&none, "/bin/echo", &["ok"]).success(),
            "net:none profile must still run non-network commands (must parse)"
        );
        let denied = run_wrapped_output(&none, id, &["-un"]);
        assert_eq!(
            stdout(&denied),
            uid,
            "zero floor: with libinfo denied `id -un` must fall back to the numeric uid \
             (stderr: {})",
            String::from_utf8_lossy(&denied.stderr)
        );

        // Reopened leg: success status AND the expected name.
        let granted = Caveats {
            net: Scope::only(["mach:com.apple.system.opendirectoryd.libinfo".to_string()]),
            ..Caveats::top()
        };
        assert!(
            run_wrapped(&granted, "/bin/echo", &["ok"]).success(),
            "mach-grant profile must still run non-network commands (must parse)"
        );
        let reopened = run_wrapped_output(&granted, id, &["-un"]);
        assert!(
            reopened.status.success(),
            "under the libinfo grant `id -un` must exit successfully (stderr: {})",
            String::from_utf8_lossy(&reopened.stderr)
        );
        assert_eq!(
            stdout(&reopened),
            name,
            "the libinfo grant must re-open uid→name resolution (stderr: {})",
            String::from_utf8_lossy(&reopened.stderr)
        );

        // Unrelated-grant leg: its own launch control, then the same exact-uid
        // fallback — a grant of SecurityServer does not re-open libinfo.
        let other = Caveats {
            net: Scope::only(["mach:com.apple.SecurityServer".to_string()]),
            ..Caveats::top()
        };
        assert!(
            run_wrapped(&other, "/bin/echo", &["ok"]).success(),
            "unrelated-grant profile must still run non-network commands (must parse)"
        );
        let unrelated = run_wrapped_output(&other, id, &["-un"]);
        assert_eq!(
            stdout(&unrelated),
            uid,
            "an unrelated grant must not re-open libinfo (stderr: {})",
            String::from_utf8_lossy(&unrelated.stderr)
        );
    }

    /// A loopback-only child can also SERVE on loopback: bind, listen and
    /// accept (a test suite's mock HTTP server). The connect-side rule alone
    /// (`remote ip "localhost:*"`) refused `bind` with EPERM, so newt's build
    /// lane could not run any wiremock-based test under this fence.
    #[test]
    fn net_loopback_only_permits_a_loopback_listener() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        let perl = "/usr/bin/perl";
        if !std::path::Path::new(perl).exists() {
            eprintln!("skipping: no perl(1) on this host");
            return;
        }
        let cav = Caveats {
            net: Scope::only(["localhost".to_string()]),
            ..Caveats::top()
        };
        // Bind an ephemeral loopback port, connect to it, accept: a full
        // local round trip, all inside the fence.
        let script = "use IO::Socket::INET; \
            my $s = IO::Socket::INET->new(LocalAddr => '127.0.0.1', LocalPort => 0, Listen => 1) \
                or die \"bind: $!\"; \
            my $c = IO::Socket::INET->new(PeerAddr => '127.0.0.1', PeerPort => $s->sockport) \
                or die \"connect: $!\"; \
            $s->accept or die \"accept: $!\"; print \"served\\n\";";
        let out = run_wrapped_output(&cav, perl, &["-e", script]);
        assert!(
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("served"),
            "net:Only([localhost]) must permit a loopback listener; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A loopback-only `net` grant kernel-confines egress to the loopback
    /// *interface* (ADR 0015): the process reaches loopback (v4 **and** v6, since
    /// SBPL's `localhost` denotes both) and is kernel-DENIED any off-box host. The
    /// grant here names a **single** v4 address (`127.0.0.1`) yet `::1` is still
    /// reachable — the documented interface-granular widening (D2): a spawned child
    /// is governed only by the kernel rule, not the exact-host admission leash.
    #[test]
    fn net_loopback_only_permits_loopback_interface_denies_offbox() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        let curl = "/usr/bin/curl";
        if !std::path::Path::new(curl).exists() {
            eprintln!("skipping: no curl(1) on this host");
            return;
        }
        let v4 = spawn_loopback_http("127.0.0.1:0").expect("bind v4 loopback");

        // A single v4 loopback address — the case that widens to the interface.
        let cav = Caveats {
            net: Scope::only(["127.0.0.1".to_string()]),
            ..Caveats::top()
        };
        // Positive control: a benign non-network command runs — the loopback
        // profile parsed (a malformed one exits 65 and never launches the child).
        assert!(
            run_wrapped(&cav, "/bin/echo", &["ok"]).success(),
            "loopback-only profile must still run non-network commands (must parse)"
        );
        // ALLOW (v4): egress to the loopback listener succeeds (curl exit 0). A
        // deny-all or malformed rule would fail this — so it cannot pass vacuously.
        let v4_url = format!("http://127.0.0.1:{}/", v4.port());
        assert!(
            run_wrapped(&cav, curl, &["-sS", "--max-time", "5", &v4_url]).success(),
            "net:Only([127.0.0.1]) must kernel-PERMIT v4 loopback egress"
        );
        // ALLOW (v6): `::1` is reachable too — locking the interface-granular
        // widening documented in ADR 0015 D2 (kernel `localhost` = 127.0.0.1 + ::1,
        // broader than the single-address grant). Skipped only if v6 loopback is
        // unavailable on the host (never on stock macOS).
        if let Some(v6) = spawn_loopback_http("[::1]:0") {
            let v6_url = format!("http://[::1]:{}/", v6.port());
            assert!(
                run_wrapped(&cav, curl, &["-sS", "--max-time", "5", &v6_url]).success(),
                "net:Only([127.0.0.1]) kernel-permits the whole loopback interface, incl. ::1 (ADR 0015 D2)"
            );
        }
        // DENY: off-box egress to a literal IP (no DNS) is kernel-denied at the
        // socket. Assert both curl exit 7 AND the EPERM signal ("Operation not
        // permitted") in stderr — so a no-internet runner (ENETUNREACH, also exit
        // 7) cannot make this pass vacuously; it must be a *permission* denial.
        let offbox = run_wrapped_output(
            &cav,
            curl,
            &["-sS", "-v", "--max-time", "5", "http://1.1.1.1/"],
        );
        assert_eq!(
            offbox.status.code(),
            Some(7),
            "net:Only([127.0.0.1]) must kernel-DENY off-box egress (curl exit 7)"
        );
        let stderr = String::from_utf8_lossy(&offbox.stderr);
        assert!(
            stderr.contains("Operation not permitted"),
            "off-box denial must be a kernel EPERM, not a routing failure: {stderr}"
        );
    }

    /// Like [`run_wrapped`] but captures stdout/stderr, so a proof can assert on
    /// the *interior* exec behavior (a granted program's child exec statuses) the
    /// kernel produced — the L3-grain the `exec` axis claims.
    fn run_wrapped_output(cav: &Caveats, program: &str, args: &[&str]) -> std::process::Output {
        let prefix = SeatbeltSandbox::new()
            .command_prefix(cav)
            .expect("a restricted axis must yield a wrapper prefix");
        assert!(!prefix.is_empty(), "expected a sandbox-exec wrapper");
        std::process::Command::new(&prefix[0])
            .args(&prefix[1..])
            .arg(program)
            .args(args)
            .output()
            .expect("spawn sandbox-exec")
    }

    /// The exec allow-list is kernel-enforced at the **interior**: a granted shell
    /// runs, may exec a *listed* binary, but is kernel-denied an *unlisted* one —
    /// the L3 gap a path allow-list alone cannot reach (ADR 0014). The discriminator
    /// is exact: the unlisted `/usr/bin/false` must fail at **exec** (status 127),
    /// not run-and-return-1 — so this cannot pass vacuously.
    #[test]
    fn exec_allowlist_permits_listed_denies_unlisted_child() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        let cav = Caveats {
            exec: Scope::only(["/bin/zsh".to_string(), "/usr/bin/true".to_string()]),
            ..Caveats::top()
        };
        let out = run_wrapped_output(
            &cav,
            "/bin/zsh",
            &["-c", "/usr/bin/true; echo T=$?; /usr/bin/false; echo F=$?"],
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("T=0"),
            "a listed binary must exec and run (T=0): {stdout:?}"
        );
        assert!(
            stdout.contains("F=127"),
            "an unlisted binary must be kernel-denied at EXEC (status 127), not run: {stdout:?}"
        );
    }

    /// The `exec:none`-style floor: when the granted set is just the entry shell,
    /// the shell launches but may exec **nothing** further — every child exec is
    /// kernel-denied. This is the interior "no further exec" guarantee.
    #[test]
    fn granted_shell_cannot_exec_any_unlisted_child() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        let cav = Caveats {
            exec: Scope::only(["/bin/zsh".to_string()]),
            ..Caveats::top()
        };
        let out = run_wrapped_output(&cav, "/bin/zsh", &["-c", "/usr/bin/true; echo S=$?"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("S=127"),
            "a shell granted only itself must be denied every child exec (S=127): {stdout:?}"
        );
    }

    /// The ADR 0011 loader trampoline — the bypass that has **no Landlock hook**
    /// and forces the Linux seccomp backstop — is *closed by the platform* on
    /// macOS. A granted interpreter (`perl`) cannot reach an unlisted binary by:
    /// (a) directly `exec`ing it, nor (b) trampolining through `dyld`. Both are
    /// governed `process-exec`s; `dyld` is not allow-listed, so both are denied.
    #[test]
    fn granted_interpreter_cannot_trampoline_to_unlisted_binary() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        let cav = Caveats {
            exec: Scope::only(["/usr/bin/perl".to_string()]),
            ..Caveats::top()
        };
        // Each `exec` returns (and perl continues) only when the exec was DENIED.
        let script = "print \"PERL-RAN\\n\"; \
                      exec(\"/usr/bin/true\"); print \"DIRECT-DENIED\\n\"; \
                      exec(\"/usr/lib/dyld\", \"/usr/bin/true\"); print \"TRAMPOLINE-DENIED\\n\";";
        let out = run_wrapped_output(&cav, "/usr/bin/perl", &["-e", script]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("PERL-RAN"),
            "the granted interpreter must run: {stdout:?}"
        );
        assert!(
            stdout.contains("DIRECT-DENIED"),
            "direct exec of an unlisted binary must be denied: {stdout:?}"
        );
        assert!(
            stdout.contains("TRAMPOLINE-DENIED"),
            "the dyld loader trampoline must be denied (no standing loader entry): {stdout:?}"
        );
    }

    /// Positive control / no deny-of-function: an allow-listed **dynamically
    /// linked** binary still loads its dylibs (via the kernel-trusted dyld path,
    /// which the exec allow-list does not gate) and runs normally under exec
    /// confinement — proving the axis confines *spawning*, not legitimate linking.
    #[test]
    fn exec_confinement_does_not_break_dynamic_linking() {
        if skip_proof_unless_seatbelt() {
            return;
        }
        let curl = "/usr/bin/curl";
        if !std::path::Path::new(curl).exists() {
            eprintln!("skipping: no curl(1) on this host");
            return;
        }
        let cav = Caveats {
            exec: Scope::only([curl.to_string()]),
            ..Caveats::top()
        };
        let status = run_wrapped(&cav, curl, &["--version"]);
        assert!(
            status.success(),
            "an allow-listed dynamic binary must load + run under exec confinement"
        );
    }
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
