//! Blocker-2 real-resource proof for the seccomp egress-deny floor
//! (`newt_core::netguard`) — the network half of b1 that Landlock (TCP-only)
//! cannot provide.
//!
//! The invariant IS the kernel refusing `socket()` for off-box families after a
//! seccomp filter is installed — no mock can stand in for the syscall the kernel
//! denies. This grounds the mocked `netguard` unit tests (which only prove the
//! BPF program *builds*).
//!
//! Method: build the deny program in the parent, `fork`, install it in the child
//! (post-fork the child runs only raw syscalls + `_exit`, so it is
//! async-signal-safe), and have the child probe each family. The parent asserts
//! the child's exit code, which encodes exactly which sockets the kernel allowed
//! vs denied:
//!
//! - AF_INET / AF_INET6 (TCP + UDP) — an unprivileged process CAN normally open
//!   these, so their denial is a clean proof the seccomp filter is doing the work
//!   (this is the UDP/DNS gap Landlock leaves open);
//! - AF_PACKET (raw) — denied;
//! - AF_UNIX (local IPC) — still ALLOWED (governed by the fs fence, not seccomp).

#![cfg(target_os = "linux")]

use newt_core::netguard::egress_deny_program;
use serial_test::serial;

// Child exit codes (0 = the whole floor behaved correctly).
const OK: i32 = 0;
const INET_NOT_DENIED: i32 = 2; // seccomp did not stop a TCP socket → floor broken
const INET_WRONG_ERRNO: i32 = 3; // denied, but not our EACCES
const INET6_NOT_DENIED: i32 = 4;
const PACKET_NOT_DENIED: i32 = 5;
const UNIX_DENIED: i32 = 6; // over-broad: local IPC must survive
const APPLY_FAILED: i32 = 10;

#[test]
#[serial]
fn seccomp_denies_all_off_box_socket_families_but_allows_unix() {
    // Build the BPF program in the parent (no allocation happens in the child).
    let prog = egress_deny_program().expect("egress-deny BPF program builds");

    // SAFETY: post-fork the child touches only raw syscalls and `_exit` — no
    // allocator, no mutex, nothing another thread could hold — so forking a
    // multi-threaded test binary here is safe.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed");

    if pid == 0 {
        // ---- child ----
        if seccomp_apply(&prog).is_err() {
            unsafe { libc::_exit(APPLY_FAILED) };
        }
        // TCP over IPv4: normally allowed unprivileged → must now be denied.
        let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        if s >= 0 {
            unsafe { libc::_exit(INET_NOT_DENIED) };
        }
        if errno() != libc::EACCES {
            unsafe { libc::_exit(INET_WRONG_ERRNO) };
        }
        // UDP over IPv6 (this is the DNS / datagram egress Landlock misses).
        let s6 = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, 0) };
        if s6 >= 0 {
            unsafe { libc::_exit(INET6_NOT_DENIED) };
        }
        // Raw packet socket.
        let sp = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW, 0) };
        if sp >= 0 {
            unsafe { libc::_exit(PACKET_NOT_DENIED) };
        }
        // Local IPC must still work (fs-fence governs its path, not seccomp).
        let su = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        if su < 0 {
            unsafe { libc::_exit(UNIX_DENIED) };
        }
        unsafe { libc::_exit(OK) };
    }

    // ---- parent ----
    let mut status: libc::c_int = 0;
    let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
    assert_eq!(waited, pid, "waitpid failed");
    assert!(libc::WIFEXITED(status), "child did not exit normally");
    let code = libc::WEXITSTATUS(status);
    assert_eq!(
        code, OK,
        "seccomp egress floor failed (child exit {code}): 2=TCP-not-denied, \
         3=wrong-errno, 4=UDPv6-not-denied, 5=raw-not-denied, 6=unix-wrongly-denied, \
         10=apply-failed"
    );
}

/// Apply the prebuilt filter in the child. Kept as a tiny wrapper so the child
/// body reads as raw syscalls.
fn seccomp_apply(prog: &seccompiler::BpfProgram) -> Result<(), String> {
    seccompiler::apply_filter(prog).map_err(|e| e.to_string())
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}
