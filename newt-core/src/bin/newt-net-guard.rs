//! `newt-net-guard` — the child-side network-confinement wrapper for the
//! confined executor's `NetGrant::DenyAll` path.
//!
//! The parent's confined spawn has already established the Landlock fs fence and
//! the child's process group by the time this runs. This process then closes any
//! inherited descriptor, installs the seccomp egress-deny floor (which Landlock
//! cannot provide — UDP/DNS/raw) and `execvp`s the requested program, so the real
//! child inherits BOTH the fs fence and the net floor and — because the seccomp
//! filter set `no_new_privs` — cannot remove either. Descendants inherit the
//! seccomp filter across fork/exec.
//!
//! The whole sequence lives in [`newt_core::netguard::run_guard_and_exec`] so it
//! is shared verbatim with the `newt __net-guard` self-exec path (which is the
//! production route — a released `newt` carries the guard without shipping this
//! separate binary). This bin remains for the crate's own real-resource tests,
//! which invoke it via `CARGO_BIN_EXE_newt-net-guard`.
//!
//! Usage:
//!   newt-net-guard [--cgroup-procs PATH] --probe-egress      # self-test
//!   newt-net-guard [--cgroup-procs PATH] -- PROGRAM [ARGS...] # install floor + exec

fn main() -> ! {
    newt_core::netguard::run_guard_and_exec(std::env::args_os().skip(1))
}
