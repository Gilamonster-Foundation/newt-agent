//! `newt-net-guard` — the child-side network-confinement wrapper for the
//! confined executor's `NetGrant::None` path.
//!
//! The parent's confined spawn has already established the Landlock fs fence and
//! the child's process group by the time this runs. This process then installs
//! the seccomp egress-deny floor (which Landlock cannot provide — UDP/DNS/raw)
//! and `execvp`s the requested program, so the real child inherits BOTH the fs
//! fence and the net floor and — because `apply_filter` set `no_new_privs` —
//! cannot remove either. Descendants inherit the seccomp filter across fork/exec.
//!
//! Usage:
//!   newt-net-guard --probe-egress        # self-test: exit 0 iff the floor holds
//!   newt-net-guard -- PROGRAM [ARGS...]   # install floor, then exec PROGRAM

#[cfg(target_os = "linux")]
fn main() -> ! {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let mut args = std::env::args_os().skip(1).peekable();

    // Optional leading `--cgroup-procs PATH`: join the cgroup-v2 subtree the
    // executor created, BEFORE anything forks, so every descendant (including a
    // setsid / double-fork daemon) is a member and the executor's `cgroup.kill`
    // reaches the whole tree. Best-effort: a failure leaves the killpg fallback.
    if args.peek().is_some_and(|a| a == "--cgroup-procs") {
        args.next();
        if let Some(path) = args.next() {
            let pid = std::process::id().to_string();
            if let Err(e) = std::fs::write(&path, &pid) {
                eprintln!("newt-net-guard: cgroup join failed (killpg fallback applies): {e}");
            }
        }
    }

    // Close any file descriptor the parent left open (>= 3): an inherited fd is
    // a capability that bypasses pathname confinement. Do this before the real
    // program can observe it.
    newt_core::netguard::close_inherited_fds();

    // Install the seccomp egress-deny floor on this soon-to-exec process.
    if let Err(e) = newt_core::netguard::install_egress_deny() {
        eprintln!("newt-net-guard: seccomp install failed (fail-closed): {e}");
        std::process::exit(120);
    }

    let first = args.next();

    // Self-test: prove the floor is active on this process, exit with its code.
    if first.as_deref().is_some_and(|s| s == "--probe-egress") {
        match newt_core::netguard::probe_egress_denied() {
            Ok(()) => std::process::exit(0),
            Err(code) => std::process::exit(code),
        }
    }

    // Exec mode: `newt-net-guard -- PROGRAM ARGS...`
    if first.as_deref().map(|s| s != "--").unwrap_or(true) {
        eprintln!("newt-net-guard: usage: newt-net-guard [--probe-egress | -- PROGRAM ARGS...]");
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

    // SAFETY: `c_prog`/`ptrs` are valid, NUL-terminated, and outlive the call;
    // `execvp` only returns on error.
    unsafe {
        libc::execvp(c_prog.as_ptr(), ptrs.as_ptr());
    }
    let e = std::io::Error::last_os_error();
    eprintln!("newt-net-guard: exec {prog:?} failed: {e}");
    std::process::exit(122);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    // The confined executor fails closed on non-Linux before reaching a guard,
    // so this is unreachable in production; refuse loudly if invoked.
    eprintln!("newt-net-guard: unsupported platform — fail closed");
    std::process::exit(121);
}
