//! Axis-granular confinement honesty (ADR 0004 D1).
//!
//! A single [`SandboxKind`] cannot honestly describe a run where axis coverage
//! differs. For example, Landlock kernel-confines the filesystem, reports
//! `exec` only at interceptor strength because of the loader trampoline, and
//! can kernel-deny TCP only for an empty `net` scope on ABI-v4 kernels.
//! Reporting only `sandbox_kind: landlock` is true coarsely but insufficient at
//! the grain a caller reasons about.
//!
//! [`enforcement_report`] classifies each **restricted** Caveat axis (`Only(_)`,
//! not `All`) as one of [`AxisEnforcement`]. It is a pure function of the
//! effective [`Caveats`] and the governing [`ConfinementMechanism`] (the selected
//! backend PLUS the mechanism configuration that changes what it can truthfully
//! enforce — e.g. the child-network policy) — no IO. A bare [`SandboxKind`]
//! converts in with the conservative default. The coarse `sandbox_kind` stays the
//! **minimum** claim; this report refines it and is never allowed to describe an
//! `advisory` axis as confined.
//!
//! `Kernel` on an axis means the actual child boundary supplies the **complete
//! requested authority** for that axis — kernel-enforced *and* no broader than
//! the Caveat asked for — not merely that a kernel primitive was involved. A
//! kernel fence that permits MORE than the Caveat (e.g. a single-address loopback
//! grant widened to the whole interface, or a `/bin/sh` grant that pulls
//! `/bin/bash`) is reported BELOW Kernel, so a `CONFINED` floor refuses rather
//! than admit an over-broad fence as an exact witness. Kernel *strength* and
//! least *authority* are separate obligations (OCAP scope-fidelity).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Caveats, ChildNetworkPolicy, SandboxKind, Scope};

/// The domain of the executable-identity admission obligation.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecBoundary {
    /// Existing process-tree scope obligation; native residuals still apply.
    #[default]
    ProcessTree,
    /// Exact root admission; descendant identities are explicitly unrestricted
    /// inside the inherited filesystem/network fence.
    NamedRoot,
}

impl ExecBoundary {
    fn is_process_tree(&self) -> bool {
        *self == Self::ProcessTree
    }
}

/// The confinement **mechanism** actually governing a spawn: the selected OS
/// backend PLUS the mechanism configuration that changes what a given backend can
/// truthfully enforce. These include the explicit [`ExecBoundary`] proof domain
/// and child-network policy ([`ChildNetworkPolicy`]) — the difference between a Landlock TCP-only rule and
/// the seccomp `DenyDirect` socket-family deny is *material* to the net witness,
/// so the report must be computed from the mechanism, not the backend kind alone.
///
/// A bare [`SandboxKind`] converts in via [`From`] using the **conservative**
/// default ([`ChildNetworkPolicy::LandlockOnly`]) — the weakest mechanism, so a
/// caller that has not stated a stronger one can never *over*-claim. A spawn site
/// that installs a stronger mechanism passes it explicitly (via
/// [`ConfinementMechanism::new`]) so the report matches the child's real boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfinementMechanism {
    kind: SandboxKind,
    child_network: ChildNetworkPolicy,
    // Omission preserves the existing default canonical body and fence CID.
    #[serde(default, skip_serializing_if = "ExecBoundary::is_process_tree")]
    exec_boundary: ExecBoundary,
}

impl ConfinementMechanism {
    /// The mechanism governing a spawn: `kind` backend + `child_network` policy.
    #[must_use]
    pub fn new(kind: SandboxKind, child_network: ChildNetworkPolicy) -> Self {
        Self {
            kind,
            child_network,
            exec_boundary: ExecBoundary::ProcessTree,
        }
    }

    /// Explicit root admission with a filesystem/network-fenced process tree.
    #[must_use]
    pub fn for_named_root(kind: SandboxKind, child_network: ChildNetworkPolicy) -> Self {
        Self {
            kind,
            child_network,
            exec_boundary: ExecBoundary::NamedRoot,
        }
    }

    /// The actual executable-identity proof domain.
    #[must_use]
    pub fn exec_boundary(&self) -> ExecBoundary {
        self.exec_boundary
    }

    /// A backend with the **conservative** (weakest) child-network mechanism
    /// ([`ChildNetworkPolicy::LandlockOnly`]). Never over-claims the net axis.
    #[must_use]
    pub fn backend(kind: SandboxKind) -> Self {
        Self::new(kind, ChildNetworkPolicy::LandlockOnly)
    }

    /// The selected OS backend.
    #[must_use]
    pub fn kind(&self) -> SandboxKind {
        self.kind
    }

    /// The child-network mechanism policy.
    #[must_use]
    pub fn child_network(&self) -> ChildNetworkPolicy {
        self.child_network
    }
}

impl From<SandboxKind> for ConfinementMechanism {
    fn from(kind: SandboxKind) -> Self {
        Self::backend(kind)
    }
}

/// How a single restricted Caveat axis is actually enforced for a run
/// (ADR 0004 D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AxisEnforcement {
    /// An OS ruleset enforces this axis against the spawned program's
    /// **interior** (e.g. Landlock on `fs_write`). The strongest claim.
    ///
    /// **`exec → kernel` is about *identity*, not *behavior* (ADR 0013 D6 /
    /// agent-bridle#114).** It means "no **un-granted program** can run as a
    /// process" — via Seatbelt `process-exec*` (ADR 0014), or a Linux minimal
    /// rootfs that physically excludes un-granted binaries (ADR 0013). It does
    /// **NOT** mean a *granted* program — especially a granted **interpreter**
    /// (`sh`, `python`, `perl`) — is constrained in what it *does*: its interior
    /// logic is still bounded only by the `fs_read`/`fs_write`/`net` axes (read
    /// those for the data-side guarantee). Do not read `exec → kernel` as "this
    /// program will only do what I expect."
    Kernel,
    /// The in-process L2 leash gates this axis at the spawn/open chokepoint —
    /// it holds for the engine's own operations, **not** for a permitted
    /// external child's interior (a `find -exec` child's reads escape it).
    Interceptor,
    /// Validated at admission, then **ambient** — nothing backstops the spawned
    /// interior. Honest "we checked the request, we cannot confine the effect."
    Advisory,
}

impl AxisEnforcement {
    /// Ascending confinement strength: `Advisory (0) < Interceptor (1) <
    /// Kernel (2)`.
    ///
    /// The variants are *declared* strongest-first (`Kernel` first) so the type
    /// reads top-down — which means a naive `#[derive(PartialOrd, Ord)]` would
    /// order them DESCENDING (`Kernel < Advisory`) and silently invert every
    /// `min` / [`fence_strength`] into a **fail-open** (ADR 0012 D2). The order is
    /// therefore defined **explicitly** here, never derived; this hand-written
    /// `impl` also turns a future stray `#[derive(Ord)]` into a hard compile error
    /// (conflicting impls) rather than a silent security bug.
    fn rank(self) -> u8 {
        match self {
            AxisEnforcement::Advisory => 0,
            AxisEnforcement::Interceptor => 1,
            AxisEnforcement::Kernel => 2,
        }
    }
}

impl Ord for AxisEnforcement {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for AxisEnforcement {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Per-axis confinement report for the four OS-confinement Caveat axes
/// (`fs_read`, `fs_write`, `exec`, `net`).
///
/// Only **restricted** (`Only(_)`) axes appear (`Some(_)`); an axis granted
/// `All` is unrestricted — there is nothing to confine — and is `None`. The
/// `max_calls` / `valid_for_generation` axes are gate-enforced budget/causality,
/// not OS-confinement axes, so they are not part of this report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EnforcementReport {
    /// Enforcement of the `fs_read` axis, when restricted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs_read: Option<AxisEnforcement>,
    /// Enforcement of the `fs_write` axis, when restricted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs_write: Option<AxisEnforcement>,
    /// Enforcement of the `exec` axis, when restricted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec: Option<AxisEnforcement>,
    /// Enforcement of the `net` axis, when restricted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net: Option<AxisEnforcement>,
}

impl EnforcementReport {
    /// `true` when no axis is restricted (every axis is `All`) — so the report
    /// carries no information and may be omitted from a result envelope.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fs_read.is_none()
            && self.fs_write.is_none()
            && self.exec.is_none()
            && self.net.is_none()
    }
}

/// `true` if a scope actually restricts (`Only(_)`); `All` does not confine.
fn is_restricted<T: Ord + Clone>(scope: &Scope<T>) -> bool {
    matches!(scope, Scope::Only(_))
}

/// Classify each restricted axis of `effective` under the `active` sandbox
/// (ADR 0004 D1). Pure; no IO.
///
/// The mapping reflects what each layer *actually* enforces today:
///
/// - **`fs_read` / `fs_write`** — `kernel` under the native filesystem
///   boundaries (Landlock, Seatbelt, AppContainer, minimal-rootfs, micro-VM);
///   otherwise `interceptor` for the engine's own opens.
/// - **`exec`** — `kernel` under Seatbelt and identity-closing tiers, and for
///   AppContainer's deny-all scope. Landlock, AppContainer non-empty allowlists,
///   and `None` remain `interceptor`.
/// - **`net`** — `kernel` for a micro-VM; for empty or loopback-only scopes under
///   AppContainer; and — the mechanism-sensitive case — for a Landlock `net:none`
///   child ONLY when the [`ChildNetworkPolicy::DenyDirect`] seccomp socket-family
///   deny is installed (which closes UDP/DNS/raw/packet). Seatbelt is `advisory`
///   for every restricted net shape: its direct socket rules remain useful native
///   mechanism evidence, but the applied authority projection cannot yet bound
///   ambient Mach/XPC deputies, so admission refuses rather than treating those
///   rules as a complete Kernel network witness. A Landlock `net:none` under
///   [`ChildNetworkPolicy::LandlockOnly`] likewise remains `advisory` because it
///   denies only TCP connect/bind (ABI v4) and leaves other socket families ambient.
///
/// Accepts a bare [`SandboxKind`] (conservative `LandlockOnly` net mechanism) or
/// an explicit [`ConfinementMechanism`].
#[must_use]
pub fn enforcement_report(
    effective: &Caveats,
    mechanism: impl Into<ConfinementMechanism>,
) -> EnforcementReport {
    let ConfinementMechanism {
        kind: active,
        child_network,
        exec_boundary,
    } = mechanism.into();
    // Filesystem axes: kernel when an OS sandbox actually governs them, else the
    // in-process interceptor. Exhaustive over `SandboxKind` so a new backend
    // must decide its mapping rather than silently defaulting.
    let fs = |scope: &Scope<String>| {
        is_restricted(scope).then_some(match active {
            // Real OS sandboxes that govern the filesystem axes in the kernel —
            // Landlock (Linux, FS allow-list via restrict_self), Seatbelt (macOS,
            // SBPL read/write rules), and the Linux minimal-rootfs jail (read-only/
            // read-write bind-mounts inside its mount namespace, ADR 0013 D3/D4).
            SandboxKind::Landlock
            | SandboxKind::Seatbelt
            | SandboxKind::MinimalRootfs
            | SandboxKind::MicroVm => AxisEnforcement::Kernel,
            // AppContainer (#51): per-path ACEs are now wired in the launcher via
            // Win32 ACL APIs. The container's default deny-all-user-directories
            // combined with explicit DACL grants makes both read and write Kernel
            // for user-space paths. System paths remain accessible via
            // ALL_APPLICATION_PACKAGES (a known limitation documented in ADR 0009),
            // but write access to system paths is still kernel-denied by NTFS.
            SandboxKind::AppContainer => AxisEnforcement::Kernel,
            SandboxKind::None => AxisEnforcement::Interceptor,
        })
    };
    EnforcementReport {
        fs_read: fs(&effective.fs_read),
        fs_write: fs(&effective.fs_write),
        exec: is_restricted(&effective.exec).then_some(match active {
            _ if exec_boundary == ExecBoundary::NamedRoot => AxisEnforcement::Interceptor,
            // `exec → kernel` is reserved for modes that close the axis by
            // *identity*: Seatbelt (macOS) via `process-exec*` — interior-covering,
            // no trampoline bypass on Apple Silicon (ADR 0014) — and the Linux
            // minimal-rootfs jail, where no un-granted binary physically *exists*
            // to run or to `ld.so`-trampoline into (ADR 0013 D5, ADR 0011 D7's
            // precondition made physically true). Landlock's exec axis is held
            // (agent-bridle#31/#57) and a Noop host has no OS allow-list, so both
            // stay interceptor. AppContainer: when exec is *fully denied* (empty
            // allow-list), `PROCESS_CREATION_CHILD_PROCESS_RESTRICTED` prevents
            // any child-process creation at the kernel level, closing the exec axis
            // by OS enforcement (#123). A non-empty allow-list cannot be kernel-
            // expressed (no WDAC policy), so it stays interceptor.
            SandboxKind::AppContainer if crate::sandbox::exec_fully_denied(effective) => {
                AxisEnforcement::Kernel
            }
            // Seatbelt exec scope-fidelity (model B, agent-bridle#318/#320): a
            // `process-exec*` allow-list is an EXACT identity witness only when the
            // kernel profile permits exactly the granted programs. Apple's
            // `/bin/sh` re-execs `/bin/bash` at startup (a kernel-checked
            // process-exec), so a grant of `sh` forces the profile to ALSO permit
            // `/bin/bash` — a program the Caveat did not name. That is a widened
            // closure, so the axis is honestly Interceptor, not an exact Kernel
            // witness (the Newt `CONFINED` floor requires only Interceptor on exec,
            // so this refuses nothing legitimate). `exec:none` (deny-all) and an
            // allow-list with no launcher-variant stay exact Kernel.
            SandboxKind::Seatbelt
                if crate::sandbox::exec_grant_pulls_launcher_variant(effective) =>
            {
                AxisEnforcement::Interceptor
            }
            SandboxKind::Seatbelt | SandboxKind::MinimalRootfs | SandboxKind::MicroVm => {
                AxisEnforcement::Kernel
            }
            SandboxKind::Landlock | SandboxKind::AppContainer | SandboxKind::None => {
                AxisEnforcement::Interceptor
            }
        }),
        net: is_restricted(&effective.net).then_some(match active {
            // AppContainer (#133, ADR 0016): the capability model kernel-denies all
            // off-box egress when no internet capability SIDs are granted. Two net
            // scopes reach Kernel: deny-all (empty set) and loopback-only — both
            // route through the AppContainer capability block + loopback exemption.
            // A general remote-host allow-list is enforced userspace by the egress
            // proxy; net stays Advisory there (the proxy over-delivers above the
            // AppContainer floor, ADR 0006). MicroVM: no guest NIC → always Kernel.
            SandboxKind::AppContainer
                if crate::sandbox::net_fully_denied(effective)
                    || crate::sandbox::net_loopback_full_interface(effective) =>
            {
                AxisEnforcement::Kernel
            }
            SandboxKind::AppContainer => AxisEnforcement::Advisory,
            SandboxKind::MicroVm => AxisEnforcement::Kernel,
            // Landlock: a `net:none` child reaches a COMPLETE off-box egress deny
            // — the property Bridle associates with a Kernel net axis — ONLY under
            // the `DenyDirect` mechanism, whose seccomp `socket()`-family deny
            // (AF_INET/AF_INET6/AF_PACKET → EACCES) closes every off-box protocol
            // and is inherited across fork/exec. The caveat-driven Landlock rule
            // alone (`LandlockOnly`) denies only TCP connect/bind on ABI v4 and
            // leaves UDP/DNS/raw/packet ambient — an INCOMPLETE fence, so it is NOT
            // a Kernel network witness (it falls through to Advisory below). The
            // witness follows the child-network policy actually installed for THIS
            // spawn, never the backend kind alone (agent-bridle#1631).
            SandboxKind::Landlock
                if crate::sandbox::net_fully_denied(effective)
                    && child_network == ChildNetworkPolicy::DenyDirect =>
            {
                AxisEnforcement::Kernel
            }
            // Seatbelt has useful direct-socket SBPL rules for deny-all and
            // loopback, but no faithful bound for every ambient Mach/XPC deputy;
            // every restricted shape therefore stays Advisory and is refused by a
            // Kernel net floor. The minimal-rootfs jail does not namespace the
            // network this tier either, so it is advisory too (ADR 0013 D5).
            SandboxKind::Landlock
            | SandboxKind::Seatbelt
            | SandboxKind::MinimalRootfs
            | SandboxKind::None => AxisEnforcement::Advisory,
        }),
    }
}

/// The fence's overall strength: the greatest-lower-bound (weakest) enforcement
/// across the **restricted** axes of `report` — a fence is only as strong as its
/// weakest confined axis (ADR 0012 D1). Returns `None` when no axis is restricted
/// (an empty report: a top grant confining nothing — a vacuous top with nothing
/// to enforce). **Pure**: recomputed from the report on every call, never stored,
/// so it cannot diverge from the lattice it summarizes (ADR 0004 D3 / ADR 0012's
/// rejection of a parallel strength enum). Consumers that need to know *which*
/// axis dropped the strength still read the per-axis [`EnforcementReport`].
#[must_use]
pub fn fence_strength(report: &EnforcementReport) -> Option<AxisEnforcement> {
    [report.fs_read, report.fs_write, report.exec, report.net]
        .into_iter()
        .flatten()
        .min()
}

/// The **minimum enforcement strength required per authority axis** before
/// hostile, tool-controlled code may run (ADR 0012 D3, per-axis form). It pairs
/// with [`EnforcementReport`]: the *report* is what a backend **actually**
/// delivers, this *floor* is what the principal **requires**, and admission is
/// `report[axis] >= floor[axis]` for every restricted axis.
///
/// The three objects are deliberately distinct (do not conflate them):
/// [`Caveats`] answer *what authority* an invocation may exercise;
/// `EnforcementFloor` answers *how strongly* restricted authority must be
/// mechanically bounded; [`EnforcementReport`] answers *what strength* the
/// backend actually provided. This floor is **not** a second Caveats lattice.
///
/// It describes required **semantic** strength — a real OS boundary
/// ([`AxisEnforcement::Kernel`]), the in-process leash
/// ([`AxisEnforcement::Interceptor`]), or admission-only
/// ([`AxisEnforcement::Advisory`]) — and names **no platform or backend**
/// (never `Landlock`/`Seatbelt`/`AppContainer`).
///
/// Different axes legitimately carry different floors, which is the whole point:
/// a **scalar** `Kernel` floor wrongly rejects the exec axis on backends that
/// enforce exec only at the interceptor tier (e.g. Landlock's loader
/// trampoline), even though filesystem and network are genuinely kernel-fenced.
/// [`Self::CONFINED`] therefore requires Kernel for the filesystem and network
/// axes but accepts Interceptor for exec.
///
/// # The filesystem axes are structurally pinned to `Kernel`
///
/// Every legitimate floor ([`Self::DEFAULT`], [`Self::CONFINED`],
/// [`Self::from_scalar`], [`Self::uniform`]) requires [`AxisEnforcement::Kernel`]
/// on both filesystem axes (`uniform` is the sole exception and is documented as
/// a testing/uniform helper — production callers use the presets). A restricted
/// filesystem axis must **never** be admitted below a real OS boundary, so a
/// sub-Kernel `fs_read`/`fs_write` floor is made **unrepresentable** for every
/// externally constructible/deserializable value:
///
/// * the fields are private (`pub(crate)`) — no external struct literal;
/// * the type carries a **hand-written** [`serde`] impl (NOT derived): the wire
///   form is only `{exec, net}`, so no serde format (JSON, the trusted-worker
///   envelope, anything) can select a filesystem floor, and deserialization
///   always reconstructs `fs_read = fs_write = Kernel`. A payload that smuggles an
///   `fs_read`/`fs_write` field is **rejected** (`deny_unknown_fields`), never
///   silently normalized — a downgrade attempt is a hard error.
///
/// Read via [`Self::fs_read`] / [`Self::fs_write`] / [`Self::exec`] /
/// [`Self::net`] (or [`Self::requirement`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnforcementFloor {
    /// Floor for the `fs_read` axis. Always [`AxisEnforcement::Kernel`] for any
    /// externally constructed/deserialized value — see the type doc.
    pub(crate) fs_read: AxisEnforcement,
    /// Floor for the `fs_write` axis. Always [`AxisEnforcement::Kernel`] for any
    /// externally constructed/deserialized value — see the type doc.
    pub(crate) fs_write: AxisEnforcement,
    /// Floor for the `exec` / behavior axis.
    pub(crate) exec: AxisEnforcement,
    /// Floor for the `net` axis.
    pub(crate) net: AxisEnforcement,
}

/// Wire form of [`EnforcementFloor`]: **only** the caller-selectable axes. The
/// filesystem floors are deliberately absent — never on the wire, so they can
/// neither be forged weak nor silently normalized. `deny_unknown_fields` turns a
/// smuggled `fs_read`/`fs_write` (or any stray) field into a hard error.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnforcementFloorWire {
    exec: AxisEnforcement,
    net: AxisEnforcement,
}

impl Serialize for EnforcementFloor {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // The filesystem floors are invariantly Kernel; omit them so no consumer
        // can ever observe (or select) a weaker value.
        EnforcementFloorWire {
            exec: self.exec,
            net: self.net,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EnforcementFloor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = EnforcementFloorWire::deserialize(deserializer)?;
        // The filesystem floors are not caller-selectable: reconstruct the pinned
        // Kernel invariant (an `fs_*` field would already have been rejected by
        // `deny_unknown_fields`).
        Ok(Self {
            fs_read: AxisEnforcement::Kernel,
            fs_write: AxisEnforcement::Kernel,
            exec: wire.exec,
            net: wire.net,
        })
    }
}

impl EnforcementFloor {
    /// The permissive **default** floor, preserving the historic behavior of the
    /// scalar `Advisory` floor: the filesystem axes are kernel-enforceable so
    /// they always require [`AxisEnforcement::Kernel`], while exec/net impose no
    /// requirement ([`AxisEnforcement::Advisory`]) until a strong principal
    /// raises them. This is what an ordinary [`crate::Gate`] mints with.
    pub const DEFAULT: Self = Self {
        fs_read: AxisEnforcement::Kernel,
        fs_write: AxisEnforcement::Kernel,
        exec: AxisEnforcement::Advisory,
        net: AxisEnforcement::Advisory,
    };

    /// The floor a **confined executor** requires: a real OS boundary for the
    /// filesystem and network axes (or refuse), with exec accepted at the
    /// [`AxisEnforcement::Interceptor`] tier (the accepted floor for exec
    /// identity/behavior — a stronger kernel-exec witness also satisfies it).
    /// This is the correct replacement for a blanket scalar `Kernel` floor — the
    /// contract Newt requires: `{fs_read: Kernel, fs_write: Kernel, net: Kernel,
    /// exec: Interceptor}`.
    pub const CONFINED: Self = Self {
        fs_read: AxisEnforcement::Kernel,
        fs_write: AxisEnforcement::Kernel,
        exec: AxisEnforcement::Interceptor,
        net: AxisEnforcement::Kernel,
    };

    /// A uniform floor: the same requirement on every axis. **Test-only**: it can
    /// express a sub-Kernel filesystem floor, which no external caller may
    /// construct (that would break the pinned-Kernel fs invariant — see the type
    /// doc), so it is not part of the public (or even non-test) surface.
    /// Production callers use [`Self::DEFAULT`] / [`Self::CONFINED`] /
    /// [`Self::from_scalar`].
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn uniform(floor: AxisEnforcement) -> Self {
        Self {
            fs_read: floor,
            fs_write: floor,
            exec: floor,
            net: floor,
        }
    }

    /// The `fs_read`-axis floor (always [`AxisEnforcement::Kernel`] for an
    /// externally obtained value — see the type doc).
    #[must_use]
    pub fn fs_read(&self) -> AxisEnforcement {
        self.fs_read
    }

    /// The `fs_write`-axis floor (always [`AxisEnforcement::Kernel`] for an
    /// externally obtained value — see the type doc).
    #[must_use]
    pub fn fs_write(&self) -> AxisEnforcement {
        self.fs_write
    }

    /// The `exec`-axis floor.
    #[must_use]
    pub fn exec(&self) -> AxisEnforcement {
        self.exec
    }

    /// The `net`-axis floor.
    #[must_use]
    pub fn net(&self) -> AxisEnforcement {
        self.net
    }

    /// The pointwise **join** (per-axis max) of two floors — the monotonic layering
    /// operator: once an axis requires strength S, joining can only keep it at S or
    /// raise it, never lower it. This is how delegation/configuration layers a
    /// floor (see [`crate::Gate::with_enforcement_floor`]): a strong principal's
    /// requirement cannot be silently downgraded by a later, weaker one.
    #[must_use]
    pub fn join(self, other: Self) -> Self {
        Self {
            fs_read: self.fs_read.max(other.fs_read),
            fs_write: self.fs_write.max(other.fs_write),
            exec: self.exec.max(other.exec),
            net: self.net.max(other.net),
        }
    }

    /// Bridge the historic **scalar** floor to the per-axis form, exactly
    /// preserving its semantics: the filesystem axes were always required to be
    /// kernel-enforced (independent of the scalar), while exec and net took the
    /// scalar requirement. So `from_scalar(Advisory) == DEFAULT` and
    /// `from_scalar(Kernel)` demands Kernel on every axis (the old, over-strict
    /// behavior a scalar `Kernel` produced — callers wanting the correct exec
    /// relaxation use [`Self::CONFINED`]).
    #[must_use]
    pub const fn from_scalar(floor: AxisEnforcement) -> Self {
        Self {
            fs_read: AxisEnforcement::Kernel,
            fs_write: AxisEnforcement::Kernel,
            exec: floor,
            net: floor,
        }
    }

    /// The floor for one confined axis.
    #[must_use]
    pub fn requirement(&self, axis: ConfinedAxis) -> AxisEnforcement {
        match axis {
            ConfinedAxis::FsRead => self.fs_read,
            ConfinedAxis::FsWrite => self.fs_write,
            ConfinedAxis::Net => self.net,
            ConfinedAxis::Exec => self.exec,
        }
    }
}

impl Default for EnforcementFloor {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One of the four OS-confinement axes, used to name *which* axis fell below its
/// floor in an [`UnenforceableAxis`] refusal. This is the MESH's axis type
/// (`agent_mesh_protocol::ConfinedAxis`) — one axis vocabulary across the
/// strength floor here and the scope admission in [`crate::provenance`].
pub use agent_mesh_protocol::ConfinedAxis;

/// Bridle-local axis helpers over the mesh's [`ConfinedAxis`] (an extension
/// trait because the type now lives in `agent_mesh_protocol`).
trait ConfinedAxisExt {
    fn restricted_in(self, caveats: &Caveats) -> bool;
    fn witness_in(self, report: &EnforcementReport) -> Option<AxisEnforcement>;
}

impl ConfinedAxisExt for ConfinedAxis {
    /// Whether this axis is **restricted** (`Only(_)`) in `caveats`. An `All`
    /// (unrestricted) axis carries no confinement obligation.
    fn restricted_in(self, caveats: &Caveats) -> bool {
        match self {
            ConfinedAxis::FsRead => is_restricted(&caveats.fs_read),
            ConfinedAxis::FsWrite => is_restricted(&caveats.fs_write),
            ConfinedAxis::Net => is_restricted(&caveats.net),
            ConfinedAxis::Exec => is_restricted(&caveats.exec),
        }
    }

    /// This axis's witness in `report` (the strength the backend actually
    /// delivered), or `None` if the report carries no entry for it.
    fn witness_in(self, report: &EnforcementReport) -> Option<AxisEnforcement> {
        match self {
            ConfinedAxis::FsRead => report.fs_read,
            ConfinedAxis::FsWrite => report.fs_write,
            ConfinedAxis::Net => report.net,
            ConfinedAxis::Exec => report.exec,
        }
    }
}

/// A restricted axis whose enforcement could **not** be established at its
/// required floor — the typed reason a confinement site refuses to launch.
/// Carries enough structure to debug the refusal: which axis, what was required,
/// and what the governing backend actually delivered (`None` = the backend
/// produced **no witness** for a restricted axis, which is itself a refusal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnenforceableAxis {
    /// The axis that could not be enforced at its floor.
    pub axis: ConfinedAxis,
    /// The strength the principal required on that axis.
    pub required: AxisEnforcement,
    /// The strength the backend actually delivered, or `None` if it produced no
    /// witness for this restricted axis (missing report data ⇒ refuse).
    pub actual: Option<AxisEnforcement>,
}

impl fmt::Display for UnenforceableAxis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.actual {
            Some(actual) => write!(
                f,
                "{:?} axis requires {:?} but the governing backend delivers only {:?}",
                self.axis, self.required, actual
            ),
            None => write!(
                f,
                "{:?} axis requires {:?} but the governing backend produced no enforcement witness",
                self.axis, self.required
            ),
        }
    }
}

/// The first restricted axis whose enforcement (under `active`) fails to reach
/// its per-axis floor, or `None` if every restricted axis meets it. Pure; no IO.
///
/// The acceptance rule, per restricted **Caveat** axis (not per report entry —
/// the caveats decide the obligation):
///
/// * `Scope::All` (unrestricted) ⇒ **no obligation** — ignored, never
///   manufactures a confinement requirement even if the report carries a value;
/// * restricted **and** the backend's witness `>= floor[axis]` ⇒ admit;
/// * restricted **and** the witness is below `floor[axis]` ⇒ **refuse**;
/// * restricted **and** the report carries **no witness** for that axis ⇒
///   **refuse** — missing report data is never treated as vacuous success (a
///   restricted axis with no evidence of enforcement is a fail-closed denial).
///
/// There is no fallback to a weaker backend for a restricted axis: if the
/// governing report cannot satisfy the floor, the spawn refuses.
#[must_use]
pub fn unenforceable_axis(
    effective: &Caveats,
    mechanism: impl Into<ConfinementMechanism>,
    floor: EnforcementFloor,
) -> Option<UnenforceableAxis> {
    let report = enforcement_report(effective, mechanism.into());
    unenforceable_axis_in_report(effective, &report, floor)
}

/// The pure acceptance rule over an **explicit** report — the core of
/// [`unenforceable_axis`], factored out so the fail-closed missing-witness guard
/// can be exercised against a report that does not match `enforcement_report`'s
/// own (always-`Some`-for-restricted) shape. Same semantics as the public
/// function; `enforcement_report` supplies the report in production.
#[must_use]
pub(crate) fn unenforceable_axis_in_report(
    effective: &Caveats,
    report: &EnforcementReport,
    floor: EnforcementFloor,
) -> Option<UnenforceableAxis> {
    let check = |axis: ConfinedAxis| {
        if !axis.restricted_in(effective) {
            return None; // unrestricted axis ⇒ no confinement obligation
        }
        let required = floor.requirement(axis);
        // Admit ONLY when there is a witness at or above the floor; a witness
        // below the floor OR a MISSING witness both refuse (missing report data
        // for a restricted axis is never vacuous success).
        match axis.witness_in(report) {
            Some(a) if a >= required => None,
            other => Some(UnenforceableAxis {
                axis,
                required,
                actual: other,
            }),
        }
    };
    // fs first (the most consequential), then net, then exec — a deterministic
    // order so the reported axis is stable.
    check(ConfinedAxis::FsRead)
        .or_else(|| check(ConfinedAxis::FsWrite))
        .or_else(|| check(ConfinedAxis::Net))
        .or_else(|| check(ConfinedAxis::Exec))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CountBound, Scope};

    /// All axes restricted, so every axis appears in the report.
    fn fully_restricted() -> Caveats {
        Caveats {
            fs_read: Scope::only(["/r".to_string()]),
            fs_write: Scope::only(["/w".to_string()]),
            exec: Scope::only(["echo".to_string()]),
            net: Scope::only(["example.com".to_string()]),
            max_calls: CountBound::Unlimited,
            valid_for_generation: Scope::All,
        }
    }

    #[test]
    fn landlock_marks_fs_kernel_exec_interceptor_net_advisory() {
        let r = enforcement_report(&fully_restricted(), SandboxKind::Landlock);
        assert_eq!(r.fs_read, Some(AxisEnforcement::Kernel));
        assert_eq!(r.fs_write, Some(AxisEnforcement::Kernel));
        assert_eq!(r.exec, Some(AxisEnforcement::Interceptor));
        assert_eq!(r.net, Some(AxisEnforcement::Advisory));
    }

    /// Blocker 2 (mechanism-aware net witness): a Landlock `net:none` child reaches
    /// a COMPLETE off-box egress deny — the Kernel net property — ONLY under the
    /// `DenyDirect` seccomp mechanism. The caveat-driven Landlock rule alone
    /// (`LandlockOnly`) kernel-denies TCP connect/bind on ABI v4 but leaves
    /// UDP/DNS/raw/packet ambient, so it is honestly Advisory — NEVER a complete
    /// Kernel witness, regardless of the ABI probe. The exact over-claim #1631
    /// flagged; the report now follows the mechanism, not the backend kind.
    #[test]
    fn landlock_net_none_is_kernel_only_under_deny_direct() {
        let net_denied = Caveats {
            net: Scope::none(),
            ..Caveats::top()
        };
        // LandlockOnly (and a bare SandboxKind, which converts conservatively):
        // TCP-only → incomplete → Advisory, even on an ABI-v4 kernel.
        for mech in [
            ConfinementMechanism::new(SandboxKind::Landlock, ChildNetworkPolicy::LandlockOnly),
            ConfinementMechanism::backend(SandboxKind::Landlock),
        ] {
            assert_eq!(
                enforcement_report(&net_denied, mech).net,
                Some(AxisEnforcement::Advisory),
                "TCP-only Landlock net rule must not be reported as a complete Kernel witness",
            );
        }
        assert_eq!(
            enforcement_report(&net_denied, SandboxKind::Landlock).net,
            Some(AxisEnforcement::Advisory),
            "a bare SandboxKind must use the conservative (LandlockOnly) mechanism",
        );
        // DenyDirect: seccomp closes UDP/DNS/raw/packet → net:none is a complete
        // off-box egress deny → Kernel.
        assert_eq!(
            enforcement_report(
                &net_denied,
                ConfinementMechanism::new(SandboxKind::Landlock, ChildNetworkPolicy::DenyDirect)
            )
            .net,
            Some(AxisEnforcement::Kernel),
            "DenyDirect closes the UDP/DNS/raw leg, so net:none is a Kernel egress deny",
        );
        // fs is not restricted, so those axes must be absent.
        let r = enforcement_report(&net_denied, SandboxKind::Landlock);
        assert_eq!(r.fs_read, None);
        assert_eq!(r.fs_write, None);
        assert_eq!(r.exec, None);
    }

    /// Blocker 2 table (POLICY/UNIT proof — NOT native evidence). Direct Seatbelt
    /// socket behavior is grounded on-device separately, but does not promote the
    /// report while ambient Mach/XPC authority remains unbounded. The net witness
    /// follows the complete supported mechanism, not one successful primitive.
    #[test]
    fn net_witness_is_mechanism_aware_across_backends() {
        let none = || Caveats {
            net: Scope::none(),
            ..Caveats::top()
        };
        let loopback_full = || Caveats {
            net: Scope::only(["localhost".to_string()]),
            ..Caveats::top()
        };
        let loopback_v4_only = || Caveats {
            net: Scope::only(["127.0.0.1".to_string()]),
            ..Caveats::top()
        };
        let remote = || Caveats {
            net: Scope::only(["example.com".to_string()]),
            ..Caveats::top()
        };
        let ll = |p| ConfinementMechanism::new(SandboxKind::Landlock, p);
        use AxisEnforcement::{Advisory, Kernel};
        use ChildNetworkPolicy::{DenyDirect, LandlockOnly};
        let sb = ConfinementMechanism::backend(SandboxKind::Seatbelt);
        let noop = ConfinementMechanism::backend(SandboxKind::None);

        let cases: &[(&str, Caveats, ConfinementMechanism, AxisEnforcement)] = &[
            (
                "landlock-only + net:none",
                none(),
                ll(LandlockOnly),
                Advisory,
            ),
            ("deny-direct + net:none", none(), ll(DenyDirect), Kernel),
            ("seatbelt + net:none", none(), sb, Advisory),
            (
                "seatbelt + loopback full interface",
                loopback_full(),
                sb,
                Advisory,
            ),
            (
                "seatbelt + loopback single addr (widens)",
                loopback_v4_only(),
                sb,
                Advisory,
            ),
            ("seatbelt + remote allowlist", remote(), sb, Advisory),
            ("noop + net:none", none(), noop, Advisory),
            ("noop + remote allowlist", remote(), noop, Advisory),
        ];
        for (label, caveats, mech, expected) in cases {
            assert_eq!(
                enforcement_report(caveats, *mech).net,
                Some(*expected),
                "{label}: net witness must match the mechanism's real capability",
            );
        }
    }

    /// Seatbelt (macOS) governs the fs axes in the kernel like Landlock, **and**
    /// the `exec` axis via `process-exec*` (ADR 0014) — so exec is `kernel`, not
    /// `interceptor`. Every restricted `net` shape stays advisory until the
    /// applied Seatbelt authority can faithfully bound ambient deputies.
    #[test]
    fn seatbelt_marks_fs_and_exec_kernel_net_advisory() {
        let r = enforcement_report(&fully_restricted(), SandboxKind::Seatbelt);
        assert_eq!(r.fs_read, Some(AxisEnforcement::Kernel));
        assert_eq!(r.fs_write, Some(AxisEnforcement::Kernel));
        assert_eq!(r.exec, Some(AxisEnforcement::Kernel));
        assert_eq!(r.net, Some(AxisEnforcement::Advisory));
    }

    /// The macOS exec-axis honesty distinction from Landlock: a restricted `exec`
    /// is `kernel` under Seatbelt but only `interceptor` under Landlock (its exec
    /// axis is held) and a Noop host. ADR 0014.
    #[test]
    fn exec_is_kernel_under_seatbelt_interceptor_elsewhere() {
        let cav = Caveats {
            exec: Scope::only(["git".to_string()]),
            ..Caveats::top()
        };
        assert_eq!(
            enforcement_report(&cav, SandboxKind::Seatbelt).exec,
            Some(AxisEnforcement::Kernel)
        );
        assert_eq!(
            enforcement_report(&cav, SandboxKind::Landlock).exec,
            Some(AxisEnforcement::Interceptor)
        );
        assert_eq!(
            enforcement_report(&cav, SandboxKind::None).exec,
            Some(AxisEnforcement::Interceptor)
        );
    }

    /// ADR 0013 D5 (#110): a minimal-rootfs jail run governs the filesystem axes
    /// (bind-mounts) **and** the `exec` axis (identity by existence) in the kernel;
    /// `net` is not namespaced this tier, so it stays advisory.
    #[test]
    fn minimal_rootfs_marks_fs_and_exec_kernel_net_advisory() {
        let r = enforcement_report(&fully_restricted(), SandboxKind::MinimalRootfs);
        assert_eq!(r.fs_read, Some(AxisEnforcement::Kernel));
        assert_eq!(r.fs_write, Some(AxisEnforcement::Kernel));
        assert_eq!(r.exec, Some(AxisEnforcement::Kernel));
        assert_eq!(r.net, Some(AxisEnforcement::Advisory));
    }

    /// ADR 0013 D5 (#110) acceptance: a restricted `exec` is `kernel` in the
    /// minimal-rootfs mode but only `interceptor` under a Landlock-only boundary
    /// (its exec axis is held — ADR 0011). `kernel` is reserved for the rootfs mode.
    #[test]
    fn exec_is_kernel_under_minimal_rootfs_interceptor_under_landlock() {
        let cav = Caveats {
            exec: Scope::only(["cat".to_string()]),
            ..Caveats::top()
        };
        assert_eq!(
            enforcement_report(&cav, SandboxKind::MinimalRootfs).exec,
            Some(AxisEnforcement::Kernel),
            "minimal-rootfs closes exec by identity ⇒ kernel"
        );
        assert_eq!(
            enforcement_report(&cav, SandboxKind::Landlock).exec,
            Some(AxisEnforcement::Interceptor),
            "a Landlock-only boundary run stays exec→interceptor (ADR 0011)"
        );
    }

    /// ADR 0013 D3 (#111): the Tier-2 micro-VM confines every OS axis in the
    /// kernel — fs + exec by the guest boundary (identity by existence), and net
    /// because the guest has no network device (egress impossible). The strongest
    /// tier: `fence_strength` is therefore `Kernel` even with all axes restricted.
    #[test]
    fn micro_vm_marks_all_axes_kernel() {
        let r = enforcement_report(&fully_restricted(), SandboxKind::MicroVm);
        assert_eq!(r.fs_read, Some(AxisEnforcement::Kernel));
        assert_eq!(r.fs_write, Some(AxisEnforcement::Kernel));
        assert_eq!(r.exec, Some(AxisEnforcement::Kernel));
        assert_eq!(r.net, Some(AxisEnforcement::Kernel));
        assert_eq!(fence_strength(&r), Some(AxisEnforcement::Kernel));
    }

    /// AppContainer (#51): fs ACL narrowing is wired in the launcher. Both fs axes
    /// are now Kernel (per-path DACL grants + container default deny-user-dirs).
    /// exec stays Interceptor for non-deny-all (only deny-all → Kernel via #123).
    /// net stays Advisory for a general remote-host allowlist (no egress proxy yet,
    /// #133).
    #[test]
    fn appcontainer_marks_fs_kernel_exec_interceptor_net_advisory_for_allowlist() {
        // `fully_restricted()` uses net: Only(["example.com"]) — a non-empty
        // allowlist the launcher cannot kernel-express → Advisory.
        let r = enforcement_report(&fully_restricted(), SandboxKind::AppContainer);
        assert_eq!(r.fs_read, Some(AxisEnforcement::Kernel));
        assert_eq!(r.fs_write, Some(AxisEnforcement::Kernel));
        assert_eq!(r.exec, Some(AxisEnforcement::Interceptor));
        assert_eq!(r.net, Some(AxisEnforcement::Advisory));
    }

    /// net → Kernel only when the scope is empty (deny-all): the AppContainer
    /// capability model withholds all network SIDs → kernel-denied egress.
    #[test]
    fn appcontainer_marks_net_kernel_for_deny_all() {
        let net_deny_all = Caveats {
            net: Scope::none(),
            ..Caveats::top()
        };
        let r = enforcement_report(&net_deny_all, SandboxKind::AppContainer);
        assert_eq!(r.net, Some(AxisEnforcement::Kernel));
        // fs/exec are unrestricted (top) — not in the report.
        assert_eq!(r.fs_read, None);
        assert_eq!(r.fs_write, None);
        assert_eq!(r.exec, None);
    }

    /// Scope-fidelity for AppContainer loopback (Kernel *strength* ≠ least
    /// *authority*): the container's loopback exemption permits the whole
    /// interface (127.0.0.1 AND ::1), so it is an EXACT witness only when the
    /// Caveat denotes the full interface (`localhost`, or both addresses). A
    /// single-address grant is a widening → reported below Kernel so CONFINED
    /// refuses.
    #[test]
    fn appcontainer_loopback_is_kernel_only_for_the_full_interface() {
        let full = |hosts: &[&str]| Caveats {
            net: Scope::only(hosts.iter().map(|h| h.to_string())),
            ..Caveats::top()
        };
        for exact in [vec!["localhost"], vec!["127.0.0.1", "::1"]] {
            assert_eq!(
                enforcement_report(&full(&exact), SandboxKind::AppContainer).net,
                Some(AxisEnforcement::Kernel),
                "the full loopback interface {exact:?} is an exact Kernel witness"
            );
        }
        for widening in ["127.0.0.1", "::1"] {
            assert_eq!(
                enforcement_report(&full(&[widening]), SandboxKind::AppContainer).net,
                Some(AxisEnforcement::Advisory),
                "single-address loopback {widening} widens to the interface → not exact Kernel"
            );
        }
    }

    /// exec → Kernel for AppContainer only when the scope is empty (deny-all):
    /// `PROCESS_CREATION_CHILD_PROCESS_RESTRICTED` blocks any child-process
    /// creation at the kernel level (#123, ADR 0013 D7).
    #[test]
    fn appcontainer_marks_exec_kernel_for_deny_all() {
        let exec_deny_all = Caveats {
            exec: Scope::none(),
            ..Caveats::top()
        };
        let r = enforcement_report(&exec_deny_all, SandboxKind::AppContainer);
        assert_eq!(r.exec, Some(AxisEnforcement::Kernel));
        // fs/net are unrestricted (top) — not in the report.
        assert_eq!(r.fs_read, None);
        assert_eq!(r.fs_write, None);
        assert_eq!(r.net, None);
    }

    /// exec with a non-empty allow-list stays Interceptor: only the deny-all
    /// case can be kernel-enforced (no WDAC policy in the AppContainer launcher).
    #[test]
    fn appcontainer_exec_allowlist_stays_interceptor() {
        let exec_allowlist = Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        };
        let r = enforcement_report(&exec_allowlist, SandboxKind::AppContainer);
        assert_eq!(r.exec, Some(AxisEnforcement::Interceptor));
    }

    /// Seatbelt's direct SBPL socket rules remain scope-shaped, but none is a
    /// complete authority witness while ambient Mach/XPC deputies are unbounded.
    /// Every restricted net shape is therefore Advisory.
    #[test]
    fn seatbelt_net_is_advisory_for_every_restricted_scope() {
        let net_report = |net| {
            enforcement_report(
                &Caveats {
                    net,
                    ..Caveats::top()
                },
                SandboxKind::Seatbelt,
            )
            .net
        };

        for (label, scope) in [
            ("deny-all", Scope::none()),
            ("loopback token", Scope::only(["localhost".to_string()])),
            (
                "both loopback addresses",
                Scope::only(["127.0.0.1".to_string(), "::1".to_string()]),
            ),
            (
                "single loopback address",
                Scope::only(["127.0.0.1".to_string()]),
            ),
            ("remote allowlist", Scope::only(["example.com".to_string()])),
            (
                "mixed loopback and remote",
                Scope::only(["localhost".to_string(), "example.com".to_string()]),
            ),
        ] {
            assert_eq!(
                net_report(scope),
                Some(AxisEnforcement::Advisory),
                "Seatbelt {label} must remain Advisory until ambient deputies are bounded"
            );
        }
    }

    /// Under CONFINED (`net: Kernel`), every restricted Seatbelt net scope is
    /// refused before spawn because every such report is Advisory. Direct socket
    /// denial is not enough to discharge the ambient-deputy authority obligation.
    #[test]
    fn restricted_seatbelt_net_cannot_be_admitted_under_confined() {
        for (label, net) in [
            ("deny-all", Scope::none()),
            ("full loopback", Scope::only(["localhost".to_string()])),
            ("single loopback", Scope::only(["127.0.0.1".to_string()])),
            ("remote", Scope::only(["example.com".to_string()])),
        ] {
            let caveats = Caveats {
                net,
                ..Caveats::top()
            };
            let unmet =
                unenforceable_axis(&caveats, SandboxKind::Seatbelt, EnforcementFloor::CONFINED)
                    .unwrap_or_else(|| panic!("Seatbelt {label} must refuse under CONFINED"));
            assert_eq!(unmet.axis, ConfinedAxis::Net);
            assert_eq!(unmet.required, AxisEnforcement::Kernel);
            assert_eq!(unmet.actual, Some(AxisEnforcement::Advisory));
        }
    }

    /// Model B (exec scope-fidelity): Seatbelt exec is an exact Kernel witness
    /// only when the profile permits exactly the granted programs. A granted `sh`
    /// pulls Apple's `/bin/bash` launcher variant into the profile (a program the
    /// Caveat did not name), so the axis is Interceptor. A no-variant allow-list
    /// and `exec:none` (deny-all) stay exact Kernel. Under CONFINED (exec floor =
    /// Interceptor) the `sh` grant is still ADMITTED — Interceptor meets the floor.
    #[test]
    fn seatbelt_exec_is_interceptor_when_the_launcher_closure_widens() {
        let exec = |p: &str| Caveats {
            exec: Scope::only([p.to_string()]),
            ..Caveats::top()
        };
        for widening in ["sh", "/bin/sh"] {
            assert_eq!(
                enforcement_report(&exec(widening), SandboxKind::Seatbelt).exec,
                Some(AxisEnforcement::Interceptor),
                "a granted {widening} pulls /bin/bash into the profile → not exact Kernel"
            );
        }
        assert_eq!(
            enforcement_report(&exec("echo"), SandboxKind::Seatbelt).exec,
            Some(AxisEnforcement::Kernel),
            "a no-variant allow-list is an exact identity witness → Kernel"
        );
        let deny_all = Caveats {
            exec: Scope::none(),
            ..Caveats::top()
        };
        assert_eq!(
            enforcement_report(&deny_all, SandboxKind::Seatbelt).exec,
            Some(AxisEnforcement::Kernel),
            "deny-all exec is exact → Kernel"
        );
        // The widened `sh` grant is admitted under CONFINED (exec floor Interceptor).
        assert!(
            unenforceable_axis(
                &exec("sh"),
                SandboxKind::Seatbelt,
                EnforcementFloor::CONFINED
            )
            .is_none(),
            "Interceptor exec meets the CONFINED exec floor → admitted"
        );
    }

    /// The honesty oracle for a Noop host: NO restricted axis is ever `kernel`.
    #[test]
    fn noop_host_never_reports_kernel() {
        let r = enforcement_report(&fully_restricted(), SandboxKind::None);
        assert_eq!(r.fs_read, Some(AxisEnforcement::Interceptor));
        assert_eq!(r.fs_write, Some(AxisEnforcement::Interceptor));
        assert_eq!(r.exec, Some(AxisEnforcement::Interceptor));
        assert_eq!(r.net, Some(AxisEnforcement::Advisory));
        for axis in [r.fs_read, r.fs_write, r.exec, r.net] {
            assert_ne!(
                axis,
                Some(AxisEnforcement::Kernel),
                "Noop must never claim kernel"
            );
        }
    }

    /// Unrestricted axes (`All`) are omitted — there is nothing to confine.
    #[test]
    fn unrestricted_axes_are_omitted() {
        let top = Caveats::top(); // every axis is All
        let r = enforcement_report(&top, SandboxKind::Landlock);
        assert!(
            r.is_empty(),
            "all-`All` caveats produce an empty report: {r:?}"
        );
        assert_eq!(r.fs_write, None);
    }

    /// A mix: only `fs_write` restricted under Landlock → that one axis kernel,
    /// the rest absent.
    #[test]
    fn only_restricted_axes_appear() {
        let caveats = Caveats {
            fs_write: Scope::only(["/w".to_string()]),
            ..Caveats::top()
        };
        let r = enforcement_report(&caveats, SandboxKind::Landlock);
        assert_eq!(r.fs_write, Some(AxisEnforcement::Kernel));
        assert_eq!(r.fs_read, None);
        assert_eq!(r.exec, None);
        assert_eq!(r.net, None);
    }

    #[test]
    fn axis_enforcement_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(AxisEnforcement::Kernel).unwrap(),
            serde_json::json!("kernel")
        );
        assert_eq!(
            serde_json::to_value(AxisEnforcement::Interceptor).unwrap(),
            serde_json::json!("interceptor")
        );
        assert_eq!(
            serde_json::to_value(AxisEnforcement::Advisory).unwrap(),
            serde_json::json!("advisory")
        );
    }

    /// ADR 0012 D2 regression: the order is **ascending** `Advisory < Interceptor
    /// < Kernel`, NOT the descending declaration order. A naive `#[derive(Ord)]`
    /// would invert this — making `Kernel < Advisory` — and silently fail
    /// `fence_strength` OPEN (picking the strongest axis as the floor).
    #[test]
    fn axis_enforcement_orders_ascending_advisory_to_kernel() {
        use AxisEnforcement::{Advisory, Interceptor, Kernel};
        assert!(Advisory < Interceptor);
        assert!(Interceptor < Kernel);
        assert!(
            Advisory < Kernel,
            "the fail-open footgun: Advisory must be < Kernel"
        );
        // The strongest claim is the MAX; the weakest (the GLB the fence takes) is
        // the MIN.
        assert_eq!(
            [Interceptor, Kernel, Advisory].into_iter().max(),
            Some(Kernel)
        );
        assert_eq!(
            [Interceptor, Kernel, Advisory].into_iter().min(),
            Some(Advisory)
        );
    }

    /// A fence is only as strong as its weakest restricted axis: fully restricted
    /// under Landlock is fs=Kernel, exec=Interceptor, net=Advisory ⇒ `Advisory`.
    #[test]
    fn fence_strength_is_the_weakest_restricted_axis() {
        let r = enforcement_report(&fully_restricted(), SandboxKind::Landlock);
        assert_eq!(fence_strength(&r), Some(AxisEnforcement::Advisory));
    }

    /// Only the fs axes restricted under Landlock ⇒ both `Kernel`, nothing weaker
    /// present ⇒ the fence is `Kernel`.
    #[test]
    fn fence_strength_all_kernel_when_only_fs_restricted() {
        let caveats = Caveats {
            fs_read: Scope::only(["/r".to_string()]),
            fs_write: Scope::only(["/w".to_string()]),
            ..Caveats::top()
        };
        let r = enforcement_report(&caveats, SandboxKind::Landlock);
        assert_eq!(fence_strength(&r), Some(AxisEnforcement::Kernel));
    }

    /// An empty report (top grant, nothing restricted) has no strength — there is
    /// nothing to confine (ADR 0012 D1: a vacuous top ⇒ `None`, never a hole).
    #[test]
    fn fence_strength_empty_report_is_none() {
        let r = enforcement_report(&Caveats::top(), SandboxKind::Landlock);
        assert!(r.is_empty());
        assert_eq!(fence_strength(&r), None);
    }

    /// One restricted axis with no kernel backend ⇒ the fence is that axis's
    /// (interceptor) strength.
    #[test]
    fn fence_strength_single_axis_no_backend() {
        let caveats = Caveats {
            fs_write: Scope::only(["/w".to_string()]),
            ..Caveats::top()
        };
        let r = enforcement_report(&caveats, SandboxKind::None);
        assert_eq!(fence_strength(&r), Some(AxisEnforcement::Interceptor));
    }

    /// #114 / ADR 0013 D6 report guard: `exec → kernel` is **identity, not
    /// behavior**. A granted *interpreter* that does not pull an implementation
    /// variant (`python3`) still earns `exec → kernel` under Seatbelt — only
    /// un-granted *programs* are excluded — which must not be misread as
    /// constraining the interpreter's interior (that is governed only by the
    /// fs/net axes, absent here because they are unrestricted).
    ///
    /// (Apple's `/bin/sh` is the exception: it re-execs `/bin/bash`, widening the
    /// closure beyond the Caveat, so a granted `sh` is Interceptor — model B, see
    /// `seatbelt_exec_is_interceptor_when_the_launcher_closure_widens`.)
    #[test]
    fn exec_kernel_is_identity_not_interpreter_behavior() {
        let interp = Caveats {
            exec: Scope::only(["python3".to_string()]),
            ..Caveats::top()
        };
        let r = enforcement_report(&interp, SandboxKind::Seatbelt);
        assert_eq!(
            r.exec,
            Some(AxisEnforcement::Kernel),
            "a granted non-launcher interpreter still earns exec→kernel (identity, not behavior)"
        );
        // exec→kernel does NOT imply the interior is constrained: fs/net are All
        // (unrestricted) here, so they are absent from the report.
        assert_eq!(r.fs_read, None);
        assert_eq!(r.fs_write, None);
        assert_eq!(r.net, None);
    }

    // ── Per-axis strength floor (EnforcementFloor / unenforceable_axis) ──────
    //
    // These fail on the OLD scalar behavior and pass on the per-axis one; every
    // `actual` enforcement below is grounded in a real `enforcement_report`
    // output for the constructed (Caveats, SandboxKind) pair (pure, no IO).

    fn only_fs_write() -> Caveats {
        Caveats {
            fs_write: Scope::only(["/w".to_string()]),
            ..Caveats::top()
        }
    }
    fn only_net_host() -> Caveats {
        Caveats {
            net: Scope::only(["example.com".to_string()]),
            ..Caveats::top()
        }
    }
    fn only_net_deny_all() -> Caveats {
        Caveats {
            net: Scope::none(),
            ..Caveats::top()
        }
    }
    fn only_exec() -> Caveats {
        // A non-launcher program: `echo` does not pull an implementation variant,
        // so Seatbelt's process-exec* fence is an exact identity witness (Kernel).
        // (A granted `sh` would widen to `/bin/bash` — model B, tested separately.)
        Caveats {
            exec: Scope::only(["echo".to_string()]),
            ..Caveats::top()
        }
    }

    /// (1) A restricted filesystem axis with no kernel fs backend (`None`) must
    /// REFUSE under the confined floor — its actual enforcement is interceptor.
    #[test]
    fn restricted_fs_without_kernel_backend_refuses() {
        let unmet = unenforceable_axis(
            &only_fs_write(),
            SandboxKind::None,
            EnforcementFloor::CONFINED,
        );
        let unmet = unmet.expect("restricted fs at interceptor must be refused under CONFINED");
        assert_eq!(unmet.axis, ConfinedAxis::FsWrite);
        assert_eq!(unmet.required, AxisEnforcement::Kernel);
        assert_eq!(unmet.actual, Some(AxisEnforcement::Interceptor));
    }

    /// (2) A restricted network axis with no kernel net backend (`None`) must
    /// REFUSE under the confined floor — its actual enforcement is advisory.
    #[test]
    fn restricted_net_without_kernel_backend_refuses() {
        let unmet = unenforceable_axis(
            &only_net_host(),
            SandboxKind::None,
            EnforcementFloor::CONFINED,
        )
        .expect("restricted net at advisory must be refused under CONFINED");
        assert_eq!(unmet.axis, ConfinedAxis::Net);
        assert_eq!(unmet.required, AxisEnforcement::Kernel);
        assert_eq!(unmet.actual, Some(AxisEnforcement::Advisory));
    }

    /// (3) A restricted exec axis at the interceptor tier, with the confined
    /// floor requiring only Interceptor for exec, is ADMITTED (not refused). This
    /// is exactly what a blanket scalar `Kernel` floor got wrong.
    #[test]
    fn restricted_exec_at_interceptor_is_admitted() {
        assert!(
            unenforceable_axis(&only_exec(), SandboxKind::None, EnforcementFloor::CONFINED)
                .is_none(),
            "exec at interceptor meets the CONFINED exec floor (Interceptor)"
        );
        // ...but the old over-strict scalar Kernel floor rejects it:
        let scalar_kernel = EnforcementFloor::from_scalar(AxisEnforcement::Kernel);
        assert_eq!(
            unenforceable_axis(&only_exec(), SandboxKind::None, scalar_kernel)
                .expect("scalar Kernel wrongly rejects interceptor exec")
                .axis,
            ConfinedAxis::Exec,
        );
    }

    /// (4) An unrestricted axis manufactures no requirement — a top grant is
    /// admitted at any floor (there is nothing to confine).
    #[test]
    fn unrestricted_axis_manufactures_no_requirement() {
        assert!(unenforceable_axis(
            &Caveats::top(),
            SandboxKind::None,
            EnforcementFloor::CONFINED
        )
        .is_none());
    }

    /// (5) One axis cannot borrow another axis's strength: exec being satisfied
    /// does not cover an unmet net axis. With exec restricted (interceptor, meets
    /// its floor) AND net restricted (advisory, below Kernel), the net axis is
    /// still reported.
    #[test]
    fn one_axis_cannot_borrow_anothers_strength() {
        let caveats = Caveats {
            exec: Scope::only(["sh".to_string()]),
            net: Scope::only(["example.com".to_string()]),
            ..Caveats::top()
        };
        let unmet =
            unenforceable_axis(&caveats, SandboxKind::None, EnforcementFloor::CONFINED).unwrap();
        assert_eq!(
            unmet.axis,
            ConfinedAxis::Net,
            "net still fails though exec is satisfied"
        );
    }

    /// (6) A backend strong on fs but weak on net cannot satisfy `{fs:Kernel,
    /// net:Kernel}`. Landlock kernel-fences the filesystem but leaves a remote
    /// host net scope advisory, so a confined floor still refuses on net.
    #[test]
    fn backend_strong_on_fs_weak_on_net_cannot_satisfy_confined() {
        let caveats = Caveats {
            fs_write: Scope::only(["/w".to_string()]),
            net: Scope::only(["example.com".to_string()]),
            ..Caveats::top()
        };
        let unmet = unenforceable_axis(&caveats, SandboxKind::Landlock, EnforcementFloor::CONFINED)
            .expect("Landlock cannot kernel-confine a remote-host net scope");
        assert_eq!(unmet.axis, ConfinedAxis::Net);
        assert_eq!(unmet.actual, Some(AxisEnforcement::Advisory));
    }

    /// (7)/(9) A restricted axis the backend cannot enforce is reported — so the
    /// spawn site refuses rather than falling to a weaker (Advisory/Noop) backend.
    /// `SandboxKind::None` is the Noop backend; a restricted fs axis under it is
    /// unenforceable, so no hostile child would ever execute unconfined.
    #[test]
    fn noop_backend_never_admits_a_restricted_kernel_axis() {
        assert!(unenforceable_axis(
            &only_fs_write(),
            SandboxKind::None,
            EnforcementFloor::CONFINED
        )
        .is_some());
    }

    /// (8) Tightening the caveats can only ever refuse *more*, never admit more:
    /// a top grant is admitted, and adding a restricted fs axis the backend
    /// cannot kernel-confine flips it to a refusal.
    #[test]
    fn tightening_caveats_cannot_weaken_selection() {
        assert!(unenforceable_axis(
            &Caveats::top(),
            SandboxKind::None,
            EnforcementFloor::CONFINED
        )
        .is_none());
        assert!(
            unenforceable_axis(
                &only_fs_write(),
                SandboxKind::None,
                EnforcementFloor::CONFINED
            )
            .is_some(),
            "adding a restriction the backend can't enforce must not become admissible"
        );
    }

    /// A Seatbelt deny-all socket rule is not a complete network witness while
    /// ambient deputies remain unbounded: report Advisory and refuse CONFINED.
    #[test]
    fn seatbelt_deny_all_net_is_advisory_and_refused() {
        assert_eq!(
            enforcement_report(&only_net_deny_all(), SandboxKind::Seatbelt).net,
            Some(AxisEnforcement::Advisory),
        );
        assert!(
            unenforceable_axis(
                &only_net_deny_all(),
                SandboxKind::Seatbelt,
                EnforcementFloor::CONFINED
            )
            .is_some(),
            "Seatbelt restricted net cannot satisfy the CONFINED Kernel floor"
        );
    }

    fn floor_on(axis: ConfinedAxis, required: AxisEnforcement) -> EnforcementFloor {
        let mut f = EnforcementFloor::uniform(AxisEnforcement::Advisory);
        match axis {
            ConfinedAxis::FsRead => f.fs_read = required,
            ConfinedAxis::FsWrite => f.fs_write = required,
            ConfinedAxis::Net => f.net = required,
            ConfinedAxis::Exec => f.exec = required,
        }
        f
    }

    /// Table-driven lattice sweep: for each axis and each backend that yields a
    /// KNOWN actual enforcement, the axis is refused **iff** `actual < required`
    /// across every `required ∈ {Advisory, Interceptor, Kernel}`. Grounds the
    /// per-axis comparison in real `enforcement_report` outputs (no fiction).
    #[test]
    fn per_axis_refusal_iff_actual_below_required_matrix() {
        let strengths = [
            AxisEnforcement::Advisory,
            AxisEnforcement::Interceptor,
            AxisEnforcement::Kernel,
        ];
        // (caveats, kind, axis, actual enforcement produced by enforcement_report)
        let cases: &[(Caveats, SandboxKind, ConfinedAxis, AxisEnforcement)] = &[
            (
                only_fs_write(),
                SandboxKind::None,
                ConfinedAxis::FsWrite,
                AxisEnforcement::Interceptor,
            ),
            (
                only_fs_write(),
                SandboxKind::Landlock,
                ConfinedAxis::FsWrite,
                AxisEnforcement::Kernel,
            ),
            (
                only_net_host(),
                SandboxKind::None,
                ConfinedAxis::Net,
                AxisEnforcement::Advisory,
            ),
            (
                only_net_deny_all(),
                SandboxKind::Seatbelt,
                ConfinedAxis::Net,
                AxisEnforcement::Advisory,
            ),
            (
                only_exec(),
                SandboxKind::None,
                ConfinedAxis::Exec,
                AxisEnforcement::Interceptor,
            ),
            (
                only_exec(),
                SandboxKind::Seatbelt,
                ConfinedAxis::Exec,
                AxisEnforcement::Kernel,
            ),
        ];
        for (caveats, kind, axis, actual) in cases {
            // Sanity: the actual enforcement really is what we claim.
            let report = enforcement_report(caveats, *kind);
            let reported = report
                .fs_read
                .or(report.fs_write)
                .or(report.net)
                .or(report.exec);
            assert_eq!(
                reported,
                Some(*actual),
                "actual mismatch for {axis:?} under {kind:?}"
            );
            for &required in &strengths {
                let unmet = unenforceable_axis(caveats, *kind, floor_on(*axis, required));
                let should_refuse = *actual < required;
                assert_eq!(
                    unmet.is_some(),
                    should_refuse,
                    "{axis:?} actual={actual:?} required={required:?}: refuse should be {should_refuse}"
                );
                if let Some(u) = unmet {
                    assert_eq!(u.axis, *axis);
                    assert_eq!(u.actual, Some(*actual));
                    assert_eq!(u.required, required);
                }
            }
        }
    }

    /// The scalar back-compat bridge exactly reproduces the historic behavior:
    /// `from_scalar(f)` = filesystem Kernel (always), exec/net = `f`.
    #[test]
    fn from_scalar_preserves_historic_semantics() {
        assert_eq!(
            EnforcementFloor::from_scalar(AxisEnforcement::Advisory),
            EnforcementFloor::DEFAULT
        );
        let k = EnforcementFloor::from_scalar(AxisEnforcement::Kernel);
        assert_eq!(k.fs_read, AxisEnforcement::Kernel);
        assert_eq!(k.fs_write, AxisEnforcement::Kernel);
        assert_eq!(k.net, AxisEnforcement::Kernel);
        assert_eq!(k.exec, AxisEnforcement::Kernel);
    }

    // ── Blocker 1: `fs_read`/`fs_write < Kernel` is unrepresentable via serde ──

    /// A crafted JSON body that tries to smuggle a weak filesystem floor is
    /// REJECTED (not silently normalized): the fs axes are not wire fields, and
    /// `deny_unknown_fields` turns the attempt into a hard error. This is the exact
    /// forge the task calls out.
    #[test]
    fn serde_rejects_a_forged_weak_filesystem_floor() {
        for forged in [
            r#"{"fs_read":"advisory","fs_write":"advisory","exec":"advisory","net":"advisory"}"#,
            r#"{"fs_write":"advisory","exec":"kernel","net":"kernel"}"#,
            r#"{"fs_read":"kernel","exec":"kernel","net":"kernel"}"#, // even a Kernel fs field is unknown
        ] {
            assert!(
                serde_json::from_str::<EnforcementFloor>(forged).is_err(),
                "a payload carrying an fs_read/fs_write field must be rejected: {forged}",
            );
        }
    }

    /// The valid wire form is only `{exec, net}`; deserialization always
    /// reconstructs `fs_read = fs_write = Kernel`, so no weak fs floor can emerge
    /// from any accepted payload.
    #[test]
    fn serde_wire_form_reconstructs_kernel_filesystem() {
        let floor: EnforcementFloor =
            serde_json::from_str(r#"{"exec":"advisory","net":"advisory"}"#).unwrap();
        assert_eq!(floor.fs_read(), AxisEnforcement::Kernel);
        assert_eq!(floor.fs_write(), AxisEnforcement::Kernel);
        assert_eq!(floor.exec(), AxisEnforcement::Advisory);
        assert_eq!(floor.net(), AxisEnforcement::Advisory);
    }

    /// Round-trip through serde is lossless (fs stays Kernel because it is
    /// invariantly Kernel — dropping it from the wire loses nothing), and the
    /// serialized form contains no filesystem key.
    #[test]
    fn serde_roundtrip_is_lossless_and_omits_filesystem() {
        for floor in [
            EnforcementFloor::DEFAULT,
            EnforcementFloor::CONFINED,
            EnforcementFloor::from_scalar(AxisEnforcement::Kernel),
        ] {
            let json = serde_json::to_string(&floor).unwrap();
            assert!(
                !json.contains("fs_read") && !json.contains("fs_write"),
                "the wire form must not expose a filesystem field: {json}",
            );
            let back: EnforcementFloor = serde_json::from_str(&json).unwrap();
            assert_eq!(back, floor, "round-trip must be lossless");
            assert_eq!(back.fs_read(), AxisEnforcement::Kernel);
            assert_eq!(back.fs_write(), AxisEnforcement::Kernel);
        }
    }

    /// Every PUBLIC constructor pins both filesystem axes to Kernel — no external
    /// path (struct literal is impossible via `pub(crate)`; `uniform` is test-only)
    /// can yield a sub-Kernel fs floor.
    #[test]
    fn every_public_floor_constructor_pins_filesystem_to_kernel() {
        for f in [
            EnforcementFloor::DEFAULT,
            EnforcementFloor::CONFINED,
            EnforcementFloor::from_scalar(AxisEnforcement::Advisory),
            EnforcementFloor::from_scalar(AxisEnforcement::Interceptor),
            EnforcementFloor::from_scalar(AxisEnforcement::Kernel),
        ] {
            assert_eq!(f.fs_read(), AxisEnforcement::Kernel);
            assert_eq!(f.fs_write(), AxisEnforcement::Kernel);
        }
    }

    /// The **v0.8 Newt `CONFINED` acceptance contract** (release theorem, unit
    /// layer): effective authority `{fs_read/fs_write restricted, net:none, exec
    /// allowlist}` under floor `CONFINED {fs Kernel, net Kernel, exec Interceptor}`
    /// is ADMITTED only where the actual mechanism supplies each restricted axis's
    /// required strength — and every admission is an *exact* witness (no native
    /// scope exceeds the Caveat while labelled a strong witness). macOS restricted
    /// net remains unsupported-and-refused until native authority evidence can
    /// bound ambient deputies.
    #[test]
    fn v0_8_newt_confined_contract_admits_only_with_required_mechanisms() {
        let contract = Caveats {
            fs_read: Scope::only(["/ws".to_string()]),
            fs_write: Scope::only(["/ws".to_string()]),
            net: Scope::none(),
            exec: Scope::only(["echo".to_string()]), // no launcher variant → exact
            ..Caveats::top()
        };
        let floor = EnforcementFloor::CONFINED;
        let admits = |m: ConfinementMechanism| unenforceable_axis(&contract, m, floor).is_none();
        let refuses_on = |m: ConfinementMechanism, axis: ConfinedAxis| {
            unenforceable_axis(&contract, m, floor).map(|u| u.axis) == Some(axis)
        };
        use ChildNetworkPolicy::{DenyDirect, LandlockOnly};
        let ll = |p| ConfinementMechanism::new(SandboxKind::Landlock, p);

        // Linux: admits ONLY with the real fs (Landlock) AND net (DenyDirect
        // seccomp) kernel mechanisms; the caveat-only Landlock net (LandlockOnly)
        // is an incomplete egress fence → refuse on net.
        assert!(
            admits(ll(DenyDirect)),
            "Landlock fs + DenyDirect net satisfies CONFINED"
        );
        assert!(
            refuses_on(ll(LandlockOnly), ConfinedAxis::Net),
            "LandlockOnly net:none is incomplete (UDP/DNS/raw ambient) → refuse on net"
        );

        // macOS: direct Seatbelt socket denial is not a complete ambient-authority
        // witness, so the contract refuses on net before spawn.
        assert!(refuses_on(
            ConfinementMechanism::backend(SandboxKind::Seatbelt),
            ConfinedAxis::Net
        ));

        // Windows: AppContainer independently kernel-denies egress (no net SIDs)
        // and fences fs → admits (exec allowlist is Interceptor, meets the floor).
        assert!(admits(ConfinementMechanism::backend(
            SandboxKind::AppContainer
        )));

        // A missing/weak backend refuses BEFORE any child runs: Noop cannot supply
        // a kernel fs fence → refuse on fs (the first consequential axis).
        assert!(
            refuses_on(
                ConfinementMechanism::backend(SandboxKind::None),
                ConfinedAxis::FsRead
            ),
            "Noop cannot satisfy the CONFINED contract → fail-closed refusal"
        );

        // Every other restricted Seatbelt net shape is refused too; a
        // single-address loopback additionally widens to the whole interface.
        let widened = Caveats {
            net: Scope::only(["127.0.0.1".to_string()]),
            ..contract.clone()
        };
        assert_eq!(
            unenforceable_axis(&widened, SandboxKind::Seatbelt, floor).map(|u| u.axis),
            Some(ConfinedAxis::Net),
            "a single-address loopback widens the fence → not an exact witness → refuse"
        );
    }

    /// (8) THE fail-closed guard: a restricted axis whose report carries **no
    /// witness** must REFUSE — missing report data is never vacuous success. We
    /// inject an inconsistent report (fs_write restricted in the caveats, but the
    /// report has no fs_write entry) via the report-taking core, since the real
    /// `enforcement_report` never produces that shape.
    #[test]
    fn restricted_axis_with_missing_report_witness_refuses() {
        let effective = only_fs_write(); // fs_write is Only(_) → restricted
        let report = EnforcementReport::default(); // fs_write witness = None
        let unmet = unenforceable_axis_in_report(&effective, &report, EnforcementFloor::CONFINED)
            .expect("a restricted axis with no witness must refuse");
        assert_eq!(unmet.axis, ConfinedAxis::FsWrite);
        assert_eq!(unmet.required, AxisEnforcement::Kernel);
        assert_eq!(
            unmet.actual, None,
            "the refusal records the MISSING witness"
        );
        assert!(unmet.to_string().contains("no enforcement witness"));
    }

    /// A witness present but below the floor still refuses (the ordinary case),
    /// distinct from the missing-witness case above.
    #[test]
    fn restricted_axis_witness_below_floor_refuses_with_actual() {
        let effective = only_fs_write();
        let report = EnforcementReport {
            fs_write: Some(AxisEnforcement::Interceptor),
            ..EnforcementReport::default()
        };
        let unmet = unenforceable_axis_in_report(&effective, &report, EnforcementFloor::CONFINED)
            .expect("interceptor fs under a Kernel floor must refuse");
        assert_eq!(unmet.actual, Some(AxisEnforcement::Interceptor));
    }

    /// (3) fs Kernel cannot be satisfied by net Kernel: a strong net witness does
    /// not cover a weak fs axis (the mirror of the fs-strong/net-weak case).
    #[test]
    fn fs_floor_cannot_be_satisfied_by_a_net_witness() {
        // fs_write restricted (interceptor under None), net deny-all (kernel under
        // Seatbelt). But we need ONE backend; use None where fs=interceptor and
        // net (deny-all) would be advisory — so fs fails regardless of net. To
        // isolate "net kernel doesn't help fs", assert the reported axis is fs.
        let effective = Caveats {
            fs_write: Scope::only(["/w".to_string()]),
            net: Scope::none(),
            ..Caveats::top()
        };
        // Use a synthetic report with net Kernel but fs only Interceptor to isolate
        // the cross-axis rule: fs must still be the refused axis.
        let report = EnforcementReport {
            fs_write: Some(AxisEnforcement::Interceptor),
            net: Some(AxisEnforcement::Kernel),
            ..EnforcementReport::default()
        };
        let unmet = unenforceable_axis_in_report(&effective, &report, EnforcementFloor::CONFINED)
            .expect("fs below floor refuses even though net is Kernel");
        assert_eq!(
            unmet.axis,
            ConfinedAxis::FsWrite,
            "a net Kernel witness cannot satisfy fs"
        );
    }

    /// (9) Tightening the floor on an axis can only ever turn an admission into a
    /// refusal, never the reverse: for a fixed report, raising `required` is
    /// monotonic in refusal.
    #[test]
    fn raising_a_floor_never_turns_refusal_into_admission() {
        let effective = only_exec(); // exec restricted, Interceptor under None
                                     // Interceptor floor admits; raising to Kernel refuses. Never the reverse.
        assert!(unenforceable_axis(
            &effective,
            SandboxKind::None,
            floor_on(ConfinedAxis::Exec, AxisEnforcement::Interceptor)
        )
        .is_none());
        assert!(
            unenforceable_axis(
                &effective,
                SandboxKind::None,
                floor_on(ConfinedAxis::Exec, AxisEnforcement::Kernel)
            )
            .is_some(),
            "raising the exec floor to Kernel must refuse the interceptor witness"
        );
    }

    /// (10) Weakening the Caveats (unrestricting an axis) removes its obligation —
    /// it can only ever REDUCE refusals, never manufacture stronger authority.
    /// An `All` axis is ignored even if the backend would have under-enforced it.
    #[test]
    fn unrestricting_an_axis_only_removes_obligations() {
        // net restricted + advisory under None → refuse under CONFINED.
        let restricted = only_net_host();
        assert!(
            unenforceable_axis(&restricted, SandboxKind::None, EnforcementFloor::CONFINED)
                .is_some()
        );
        // Unrestrict net (All) → the obligation vanishes; nothing else changed.
        let unrestricted = Caveats::top();
        assert!(
            unenforceable_axis(&unrestricted, SandboxKind::None, EnforcementFloor::CONFINED)
                .is_none()
        );
    }

    /// THE NEWT COMPATIBILITY RATCHET (release §9): the exact contract Newt
    /// depends on — `{fs_read: workspace, fs_write: workspace, net: deny-all,
    /// exec: allowlist}` with floor `{fs_read: Kernel, fs_write: Kernel, net:
    /// Kernel, exec: Interceptor}` (= [`EnforcementFloor::CONFINED`]) —
    /// admits ONLY when the backend's per-axis report actually satisfies every
    /// restricted axis, and refuses (before any spawn) otherwise. Pure over
    /// `SandboxKind`; the real-resource spawn refusal is proven separately by the
    /// spawn/confinement tests. This is the ratchet that lets Newt depend on
    /// v0.8 instead of reimplementing the policy.
    fn newt_confined_caveats() -> Caveats {
        Caveats {
            fs_read: Scope::only(["/ws".to_string()]),
            fs_write: Scope::only(["/ws".to_string()]),
            net: Scope::none(), // deny-all egress
            exec: Scope::only(["cargo".to_string(), "sh".to_string()]),
            ..Caveats::top()
        }
    }

    #[test]
    fn newt_contract_admits_only_when_the_backend_actually_satisfies_it() {
        let caveats = newt_confined_caveats();
        let floor = EnforcementFloor::CONFINED;

        // Seatbelt: fs/exec meet their floors, but every restricted net shape is
        // Advisory until ambient deputies are bounded ⇒ REFUSE on net.
        assert_eq!(
            unenforceable_axis(&caveats, SandboxKind::Seatbelt, floor).map(|u| u.axis),
            Some(ConfinedAxis::Net),
            "Seatbelt restricted net must fail closed before spawn"
        );

        // Landlock: fs=Kernel, exec=Interceptor(meets floor). net(deny-all) is
        // Kernel only where the kernel mechanism supplies it; on a Landlock report
        // that classifies deny-all net as Kernel this ADMITS, else it REFUSES on
        // net — either way it never falls through to host execution. Assert the
        // decision matches the report's own net classification (no cross-axis
        // borrowing, no fallback).
        let report_landlock = enforcement_report(&caveats, SandboxKind::Landlock);
        let net_meets = report_landlock
            .net
            .is_some_and(|n| n >= AxisEnforcement::Kernel);
        let admitted = unenforceable_axis(&caveats, SandboxKind::Landlock, floor).is_none();
        assert_eq!(
            admitted, net_meets,
            "Landlock admits iff its net witness is Kernel"
        );

        // Missing backend (Noop / None): fs=Interceptor < Kernel ⇒ REFUSE before
        // execution. A Noop can NEVER satisfy this contract.
        let unmet = unenforceable_axis(&caveats, SandboxKind::None, floor)
            .expect("no kernel backend ⇒ refuse before spawn");
        assert_eq!(unmet.axis, ConfinedAxis::FsRead); // first restricted axis below floor
        assert!(
            unenforceable_axis(&caveats, SandboxKind::None, floor).is_some(),
            "replacing the backend with Noop cannot satisfy the Newt contract"
        );
    }
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
