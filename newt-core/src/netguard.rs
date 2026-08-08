//! Kernel egress default-deny for confined children — the network half of the
//! b1 floor that Landlock cannot provide.
//!
//! agent-bridle's Landlock net fence kernel-denies **TCP only** (ABI-v4
//! `LANDLOCK_ACCESS_NET_CONNECT_TCP`). UDP, DNS (UDP/TCP :53), ICMP, and raw
//! packet egress are NOT covered — a hostile child under `net: none` can still
//! resolve names and exfiltrate over UDP. This module closes that with a
//! seccomp-BPF filter that denies the `socket()` syscall for the off-box address
//! families (`AF_INET` / `AF_INET6` / `AF_PACKET`), so **no** network socket can
//! be created regardless of protocol.
//!
//! It needs no privilege, user namespace, or root: an unprivileged process may
//! install a seccomp filter once `PR_SET_NO_NEW_PRIVS` is set (which
//! [`seccompiler::apply_filter`] does). This is the enforcement path chosen
//! because unprivileged network namespaces are unavailable on hosts that set
//! `kernel.apparmor_restrict_unprivileged_userns=1` (Ubuntu ≥ 23.10) — seccomp
//! is host-policy-independent.
//!
//! `AF_UNIX` (local IPC) is deliberately allowed: a *path*-named unix socket is
//! already governed by the Landlock fs fence. Abstract-namespace unix sockets
//! (which a netns would isolate) remain a bounded residual, tracked with the b1
//! entry.

/// Whether the seccomp egress-deny floor can be built and installed on this
/// platform. Linux-only; elsewhere the confined executor fails closed before a
/// child would run, so there is nothing to filter.
#[must_use]
pub fn egress_deny_supported() -> bool {
    cfg!(target_os = "linux")
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;

    use seccompiler::{
        apply_filter, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
        SeccompFilter, SeccompRule, TargetArch,
    };

    /// The address families that reach off the box. Denying `socket()` for these
    /// is a total egress deny for the child: TCP, UDP, DNS, ICMP, and raw.
    fn egress_families() -> [u64; 3] {
        [
            libc::AF_INET as u64,
            libc::AF_INET6 as u64,
            libc::AF_PACKET as u64,
        ]
    }

    /// Build the deny-egress BPF program without installing it (so it can be
    /// constructed in a parent and applied in a post-`fork` child, and so it is
    /// unit-testable). One rule per off-box family, matched on `socket()`'s
    /// `domain` argument (arg 0).
    pub fn egress_deny_program() -> Result<BpfProgram, String> {
        let rules: Vec<SeccompRule> = egress_families()
            .into_iter()
            .map(|fam| {
                let cond = SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, fam)
                    .map_err(|e| e.to_string())?;
                SeccompRule::new(vec![cond]).map_err(|e| e.to_string())
            })
            .collect::<Result<_, String>>()?;

        let mut per_syscall: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
        per_syscall.insert(libc::SYS_socket, rules);

        let filter = SeccompFilter::new(
            per_syscall,
            // Default for every other syscall — and for `socket()` with a
            // non-matched family (e.g. AF_UNIX): allow.
            SeccompAction::Allow,
            // A matched off-box `socket()`: fail with EACCES (a clean, catchable
            // "permission denied" the child sees as an unreachable network).
            SeccompAction::Errno(libc::EACCES as u32),
            TargetArch::try_from(std::env::consts::ARCH).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;

        BpfProgram::try_from(filter).map_err(|e| e.to_string())
    }

    /// Install the egress-deny filter on the CURRENT thread; it is inherited
    /// across `execve` and by every descendant. Irreversible — call immediately
    /// before handing off to the confined program. `apply_filter` sets
    /// `PR_SET_NO_NEW_PRIVS` first, so no privilege is required.
    pub fn install_egress_deny() -> Result<(), String> {
        let prog = egress_deny_program()?;
        apply_filter(&prog).map_err(|e| e.to_string())
    }
}

#[cfg(target_os = "linux")]
pub use linux::{egress_deny_program, install_egress_deny};

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn egress_deny_program_builds_a_nonempty_bpf_filter() {
        // Mocked tier: the BPF program compiles for this arch and is non-empty
        // (an empty program would make `apply_filter` refuse). The real-resource
        // `netguard_egress_deny` test grounds that the kernel HONORS it.
        let prog = egress_deny_program().expect("egress-deny BPF builds");
        assert!(
            !prog.is_empty(),
            "an empty BPF program would be rejected by apply_filter"
        );
    }

    #[test]
    fn egress_deny_supported_on_linux() {
        assert!(egress_deny_supported());
    }
}
