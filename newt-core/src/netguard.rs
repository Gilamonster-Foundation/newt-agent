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
//! `AF_UNIX` (local IPC) is deliberately allowed. **Correction (closure-proof,
//! `af_unix_deputy.rs`):** contrary to an earlier note here, Landlock does NOT
//! govern unix-socket `connect` — its `AccessFs` rights have no such right — so a
//! confined child can `connect()` to a host AF_UNIX deputy at BOTH a pathname
//! (outside the fs fence) AND an abstract name. So allowing `AF_UNIX` leaves an
//! INDIRECT-egress residual (a network-relaying local deputy), tracked as the
//! ACTIVE `local-deputy-egress` deviation and closed only by the deferred netns /
//! mediated-egress floor (#1599). The direct-socket floor here still stands: no
//! `AF_INET`/`AF_INET6`/`AF_PACKET` socket can be created.

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

    /// Close every inherited file descriptor `>= 3` so a confined child cannot
    /// use a descriptor the parent left open. An already-open fd is a capability
    /// that BYPASSES pathname confinement (Landlock governs `open`, not an
    /// inherited fd), so an unintended inherited fd would let a hostile child read
    /// or write an out-of-fence object. stdio (0/1/2 — the confined pipes) is
    /// preserved. Call child-side just before exec.
    pub fn close_inherited_fds() {
        // `close_range(3, ~0, 0)` closes the whole upper range in one syscall
        // (Linux 5.9+). SAFETY: no memory effects; closing an already-closed fd is
        // harmless.
        let rc = unsafe { libc::close_range(3, libc::c_uint::MAX, 0) };
        if rc == 0 {
            return;
        }
        // Fallback for a pre-5.9 kernel (ENOSYS): enumerate /proc/self/fd, collect
        // first (so the dir handle is dropped before we close), then close each.
        if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
            let fds: Vec<i32> = entries
                .flatten()
                .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()))
                .filter(|&fd| fd >= 3)
                .collect();
            for fd in fds {
                // SAFETY: closing a possibly-stale fd returns EBADF, ignored.
                unsafe { libc::close(fd) };
            }
        }
    }

    /// Install the egress-deny filter on the CURRENT thread; it is inherited
    /// across `execve` and by every descendant. Irreversible — call immediately
    /// before handing off to the confined program. `apply_filter` sets
    /// `PR_SET_NO_NEW_PRIVS` first, so no privilege is required.
    pub fn install_egress_deny() -> Result<(), String> {
        let prog = egress_deny_program()?;
        apply_filter(&prog).map_err(|e| e.to_string())
    }

    /// Distinct exit/return codes the egress probe reports, so a caller (the
    /// `newt-net-guard --probe-egress` self-test, an executor probe) can tell
    /// exactly which family the kernel failed to deny.
    pub mod probe_code {
        pub const OK: i32 = 0;
        pub const TCP_NOT_DENIED: i32 = 2;
        pub const TCP_WRONG_ERRNO: i32 = 3;
        pub const UDP_NOT_DENIED: i32 = 4;
        pub const RAW_NOT_DENIED: i32 = 5;
        pub const UNIX_WRONGLY_DENIED: i32 = 6;
    }

    /// Probe, on the CURRENT thread (which must already have the filter
    /// installed), that off-box `socket()` is kernel-denied while `AF_UNIX`
    /// survives. Returns `Ok(())` when the floor holds, else the failing
    /// [`probe_code`]. Uses only raw syscalls — safe post-`fork`.
    ///
    /// # Safety
    /// Calls `libc::socket`/`close`; must run after `install_egress_deny`.
    pub fn probe_egress_denied() -> Result<(), i32> {
        // TCP over IPv4: normally openable unprivileged, so its denial proves
        // seccomp is doing the work.
        let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        if s >= 0 {
            unsafe { libc::close(s) };
            return Err(probe_code::TCP_NOT_DENIED);
        }
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno != libc::EACCES {
            return Err(probe_code::TCP_WRONG_ERRNO);
        }
        // UDP over IPv6 — the DNS / datagram egress Landlock misses.
        let s6 = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, 0) };
        if s6 >= 0 {
            unsafe { libc::close(s6) };
            return Err(probe_code::UDP_NOT_DENIED);
        }
        // Raw packet socket.
        let sp = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW, 0) };
        if sp >= 0 {
            unsafe { libc::close(sp) };
            return Err(probe_code::RAW_NOT_DENIED);
        }
        // Local IPC must still work (fs fence governs its path, not seccomp).
        let su = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        if su < 0 {
            return Err(probe_code::UNIX_WRONGLY_DENIED);
        }
        unsafe { libc::close(su) };
        Ok(())
    }

    /// The full child-side guard sequence — cgroup join, fd hygiene, seccomp
    /// egress deny — then `exec`. This is the body shared by BOTH the standalone
    /// `newt-net-guard` helper bin AND the `newt __net-guard` self-exec path
    /// (`current_exe`), so a released `newt` carries everything the confined
    /// executor needs without shipping a second binary.
    ///
    /// `args` is everything AFTER the guard selector:
    /// `[--cgroup-procs PATH]? (--probe-egress | -- PROGRAM [ARGS...])`.
    /// Never returns: it `exec`s the program or `exit`s with a diagnostic code.
    /// Fail-closed: if the seccomp floor cannot be installed it exits `120`
    /// rather than exec the program unconfined.
    pub fn run_guard_and_exec<I>(args: I) -> !
    where
        I: IntoIterator<Item = std::ffi::OsString>,
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let mut args = args.into_iter().peekable();

        // Optional leading `--cgroup-procs PATH`: join the cgroup-v2 subtree the
        // executor created, BEFORE anything forks, so every descendant (incl. a
        // setsid / double-fork daemon) is a member and the executor's
        // `cgroup.kill` reaches the whole tree. Best-effort: a failure leaves the
        // killpg fallback in place.
        if args.peek().is_some_and(|a| a == "--cgroup-procs") {
            args.next();
            if let Some(path) = args.next() {
                let pid = std::process::id().to_string();
                if let Err(e) = std::fs::write(&path, &pid) {
                    eprintln!("newt-net-guard: cgroup join failed (killpg fallback applies): {e}");
                }
            }
        }

        // Close any parent-left descriptor (>= 3): an inherited fd bypasses
        // pathname confinement, so drop it before the program can observe it.
        close_inherited_fds();

        // Install the seccomp egress-deny floor on this soon-to-exec process.
        if let Err(e) = install_egress_deny() {
            eprintln!("newt-net-guard: seccomp install failed (fail-closed): {e}");
            std::process::exit(120);
        }

        let first = args.next();

        // Self-test: prove the floor is active on this process, exit with its code.
        if first.as_deref().is_some_and(|s| s == "--probe-egress") {
            match probe_egress_denied() {
                Ok(()) => std::process::exit(0),
                Err(code) => std::process::exit(code),
            }
        }

        // Exec mode: `... -- PROGRAM ARGS...`
        if first.as_deref().map(|s| s != "--").unwrap_or(true) {
            eprintln!(
                "newt-net-guard: usage: [--cgroup-procs PATH] (--probe-egress | -- PROGRAM ARGS...)"
            );
            std::process::exit(2);
        }
        let Some(prog) = args.next() else {
            eprintln!("newt-net-guard: no program after --");
            std::process::exit(2);
        };

        let c_prog = CString::new(prog.as_bytes()).expect("program path has no interior NUL");
        let mut c_args: Vec<CString> = vec![c_prog.clone()];
        for a in args {
            c_args.push(CString::new(a.as_bytes()).expect("arg has no interior NUL"));
        }
        let mut ptrs: Vec<*const libc::c_char> = c_args.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());

        // SAFETY: `c_prog`/`ptrs` are valid, NUL-terminated, and outlive the
        // call; `execvp` only returns on error.
        unsafe {
            libc::execvp(c_prog.as_ptr(), ptrs.as_ptr());
        }
        let e = std::io::Error::last_os_error();
        eprintln!("newt-net-guard: exec {prog:?} failed: {e}");
        std::process::exit(122);
    }
}

#[cfg(target_os = "linux")]
pub use linux::{
    close_inherited_fds, egress_deny_program, install_egress_deny, probe_code, probe_egress_denied,
    run_guard_and_exec,
};

/// The child-side guard is a Linux-only mechanism; the confined executor fails
/// closed on other platforms before a child would run, so this path is
/// unreachable in production. Refuse loudly if invoked anyway.
#[cfg(not(target_os = "linux"))]
pub fn run_guard_and_exec<I: IntoIterator<Item = std::ffi::OsString>>(_args: I) -> ! {
    eprintln!("newt __net-guard: unsupported platform — fail closed");
    std::process::exit(121);
}

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
