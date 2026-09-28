//! Close *ambient* file descriptors on a confined spawn (agent-bridle#319).
//!
//! An already-open descriptor is itself an object capability. A confined child
//! must obtain resources only by inheriting the descriptors it was *delegated*
//! (its stdio), never by inheriting descriptors the parent happened to leave
//! open. `std::process::Command` relies on the platform CLOEXEC convention, but a
//! descriptor the parent opened with `CLOEXEC` cleared **is inherited** across
//! `exec` — a real capability leak (proven by
//! `agent-bridle-tool-shell/tests/real_spawn.rs::real_ambient_fd_is_not_inherited`).
//!
//! This crate is the single `unsafe`-permitting seam that lets
//! `agent-bridle-core` keep `#![forbid(unsafe_code)]` (ADR 0026, slice-1
//! decision): it installs an async-signal-safe `pre_exec` hook that marks every
//! descriptor `>= 3` **close-on-exec**, so the kernel closes them atomically at
//! `exec`, preserving stdio (`0`/`1`/`2`).
//!
//! Marking-then-exec — rather than closing the descriptors *immediately* in the
//! child — is deliberate: `std::process::Command` keeps its own `CLOEXEC` pipe
//! (also a descriptor `>= 3`) open across the `pre_exec → exec` window to report
//! an `exec` failure back to the parent. Closing that pipe early makes a failed
//! spawn (a missing or non-executable program) surface as a bogus `Ok` — the
//! child dies by `SIGABRT` and the parent reads the early EOF as success —
//! instead of the correct `io::Error`. Close-on-exec marking leaves the pipe to
//! close itself at `exec`, so exec failures are reported faithfully while
//! ambient descriptors still never reach the confined child. The `unsafe`
//! blocks are documented and encapsulated here, so callers invoke a **safe**
//! function.
//!
//! ## Platform scope
//!
//! - **Linux**: `close_range(2)` with `CLOSE_RANGE_CLOEXEC` (kernel >= 5.11) —
//!   one race-free syscall marks the whole range `[3, u32::MAX]`. No bound is
//!   involved, so nothing here is range-dependent.
//! - **macOS**: no `close_range` exists, so the pre-exec hook sweeps
//!   `fcntl(fd, F_SETFD, FD_CLOEXEC)` over `[3, bound)`. `fcntl` is
//!   async-signal-safe; the sweep allocates nothing. The exclusive `bound` is
//!   computed **in the parent** (where ordinary calls are allowed) and must
//!   provably cover every descriptor the forked child could inherit — the
//!   derivation and its proof obligations live on `sweep::plan_sweep_bound`.
//!   When no trustworthy bound within the sweepable cap can be established, the
//!   guard **refuses the confined spawn** instead of sweeping a truncated
//!   range: a cost cap is never silently promoted to a security boundary
//!   (agent-bridle#352).
//! - **Everywhere else** [`deny_inherited_fds`] is a no-op and the
//!   CLOEXEC-convention residual remains. On Windows the *confined* path does
//!   not go through `std::process::Command` at all: `agent-bridle-aclaunch`
//!   spawns with an explicit `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` of only the
//!   delegated handles, which is the Windows equivalent of this guard.
//!
//! ## Fail-closed
//!
//! For any confined spawn, every ambient descriptor that could survive `exec`
//! is either delegated stdio (fds `0`/`1`/`2`, intentionally inherited), marked
//! `FD_CLOEXEC` by the guard (closed atomically at `exec`), or the spawn is
//! refused. Concretely:
//!
//! - If the marking step fails at spawn time (`close_range` on Linux, any
//!   non-`EBADF` `fcntl` failure on macOS), the pre-exec hook returns an error,
//!   so `Command::spawn` fails and the child never runs.
//! - On macOS, if the parent cannot establish a trustworthy sweep bound, the
//!   installed hook refuses the spawn outright, reporting a distinct `errno`
//!   per reason: `ENOTSUP` (no trustworthy allocation ceiling — `getrlimit`
//!   failed or is `RLIM_INFINITY` *and* `kern.maxfilesperproc` is unreadable),
//!   `EIO` (the `/dev/fd` table could not be enumerated completely), `EMFILE`
//!   (the required range exceeds the sweepable cap `SWEEP_CAP` = 2^20). There
//!   is no silent-clamp path: an unsweepable descriptor universe means no
//!   confined child runs.
//!
//! ## Concurrency (macOS sweep)
//!
//! `fork` snapshots the descriptor table and the forked child is
//! single-threaded, so nothing races the sweep itself; the only question is
//! whether the bound captured in the parent still covers the child's table at
//! `fork` time.
//!
//! - Descriptors already open at capture time are covered by the `/dev/fd`
//!   enumeration leg (enumeration failure => refuse).
//! - Descriptors any thread creates between capture and `fork` — via `open`,
//!   `socket`, `pipe`, `dup`, `fcntl(F_DUPFD)`, `accept`, … — are allocated
//!   strictly below `min(RLIMIT_NOFILE.rlim_cur, kern.maxfilesperproc)` as it
//!   stands at creation time, and `dup2`/`F_DUPFD` reject a target at or above
//!   it (`EBADF`). That is exactly the captured `alloc_limit`, so every such
//!   descriptor is below the bound and gets swept. Concurrent creation is
//!   therefore **covered**, not merely narrow — proven natively by
//!   `tests/hostile_fds.rs::{concurrent_descriptor_creation_never_reaches_the_confined_child,
//!   the_kernel_refuses_a_descriptor_at_or_above_the_derived_ceiling}`.
//! - A thread *clearing* `FD_CLOEXEC` on an existing descriptor concurrently
//!   changes nothing: the sweep marks every in-range descriptor unconditionally
//!   after `fork`, when no other thread exists in the child.
//! - **Named residual (macos-rlimit-raise-race).** A thread that *raises*
//!   `rlim_cur` after the bound is captured and then places a descriptor above
//!   the captured bound, all before `fork`, escapes the sweep. It has two
//!   regimes: when `rlim_cur >= kern.maxfilesperproc` the bound *is*
//!   `maxfilesperproc`, a system-wide ceiling unprivileged code cannot raise,
//!   so the race is **closed** (the reference machine, macOS 26.5.2/arm64, is
//!   in this regime: `rlim_cur` = 1048576, `maxfilesperproc` = 61440). When
//!   `rlim_cur < maxfilesperproc` (the stock macOS `ulimit -n 256`) the race is
//!   open in principle. Deliberately not bought off by sweeping to
//!   `maxfilesperproc` unconditionally: that costs ~240x more `fcntl` calls per
//!   spawn on the stock configuration to defend against a thread *inside the
//!   trusted parent*, which could equally spawn its own unguarded child.
//!   agent-bridle itself never calls `setrlimit` (grep-verified) and every
//!   in-repo call site installs the guard immediately before `spawn`.
//!
//! ## Why a sweep and not `POSIX_SPAWN_CLOEXEC_DEFAULT`
//!
//! Apple's `POSIX_SPAWN_CLOEXEC_DEFAULT` (`<spawn.h>`, 0x4000) closes every
//! descriptor not named by an explicit file action, in the kernel, as part of
//! the spawn. It was measured on the reference machine to work as documented:
//! range-independent (a descriptor at 61439 is closed), immune to a concurrent
//! thread opening descriptors during the spawn, and it closes stdio too unless
//! explicit `dup2` file actions name it. It is strictly the better primitive
//! for this property, and it is **not adopted here** for a structural reason,
//! not a doubt about the mechanism:
//!
//! - `std::process::Command` exposes no `posix_spawnattr` seam, so the flag
//!   cannot be requested through std at all; and installing anything via
//!   `pre_exec` forces std onto its `fork`/`exec` path regardless. Adopting the
//!   primitive means calling `posix_spawn` directly and giving up
//!   `std::process::Child`, i.e. replacing the macOS spawn path used by
//!   `ConfinedCommand`, `OsSpawner` and the carried-coreutils dispatch —
//!   including a separate exec-failure reporting path (`posix_spawn` returns
//!   the error itself rather than through std's `CLOEXEC` status pipe) and
//!   explicit stdio file actions, or the child silently loses stdout/stderr.
//! - The blast radius is wider than the three spawn sites: `std::process` also
//!   carries the timeout/reap semantics the confined path depends on (the
//!   kill-the-process-group behaviour covered by agent-bridle#269 / AB-006) and
//!   the trusted-worker control channel that rides in as the child's stdin.
//!   Those come back into scope the moment `Child` is hand-rolled.
//! - That is an architectural slice, not a mechanism swap, and it is tracked as
//!   **agent-bridle#358**. This crate's contract (a safe function that takes a
//!   `&mut Command`) is what the three confined-spawn sites are built on today.
//!
//! The sweep is what ships, so the concurrency answer above is the one this
//! crate stands behind; the primitive would replace it with a kernel-side
//! guarantee and delete the bound question entirely — dissolving the
//! `macos-rlimit-raise-race` residual rather than mitigating it, since it
//! consults no bound at all (agent-bridle#358).

/// Install a pre-exec step that marks every inherited descriptor `>= 3`
/// close-on-exec, so the kernel closes them at `exec` in the child while
/// preserving stdio.
///
/// Safe to call: the installed closure invokes only `close_range`, which is
/// async-signal-safe, so this satisfies [`CommandExt::pre_exec`]'s contract on
/// the caller's behalf. See the crate docs for platform scope and fail-closed
/// behavior.
///
/// [`CommandExt::pre_exec`]: std::os::unix::process::CommandExt::pre_exec
#[cfg(target_os = "linux")]
pub fn deny_inherited_fds(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: the closure runs in the forked child after `fork` and before
    // `exec`. It calls only `close_range` — an async-signal-safe syscall — and
    // `io::Error::last_os_error` (a `read` of `errno`, itself async-signal-safe);
    // it allocates nothing, takes no locks, and returns promptly, satisfying
    // `pre_exec`'s async-signal-safety contract. Rust's `Command` performs its
    // stdio `dup2` onto fds 0/1/2 *before* running pre-exec hooks, so the range
    // `[3, U32::MAX]` covers only ambient descriptors and never the child's
    // delegated stdio. `CLOSE_RANGE_CLOEXEC` marks that range close-on-exec rather
    // than closing it immediately, so std's own `CLOEXEC` exec-status pipe (also a
    // descriptor `>= 3`) survives to report an `exec` failure; every marked fd
    // still closes atomically at `exec`, so the confined child never inherits one.
    unsafe {
        cmd.pre_exec(|| {
            // close_range(first=3, last=U32::MAX, CLOSE_RANGE_CLOEXEC): mark every
            // fd >= 3 close-on-exec; the kernel closes them atomically at `exec`.
            // Raw syscall, not `libc::close_range`: the libc crate has no wrapper
            // on musl, and `syscall(2)` is identical everywhere else. On a kernel
            // < 5.11 it returns -1/ENOSYS (or EINVAL for the flag) and the spawn
            // is refused — fail-closed, unchanged.
            const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
            let rc = libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                CLOSE_RANGE_CLOEXEC,
            );
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Install a pre-exec step that marks every inherited descriptor `>= 3`
/// close-on-exec, so the kernel closes them at `exec` in the child while
/// preserving stdio — **or**, when the descriptor universe cannot be proven
/// sweepable, a step that refuses the spawn.
///
/// Safe to call: the installed closure invokes only `fcntl` (or, for a refusal,
/// nothing but an `errno` copy), which is async-signal-safe, so this satisfies
/// [`CommandExt::pre_exec`]'s contract on the caller's behalf.
///
/// The sweep bound is planned in the parent by `sweep::plan_sweep_bound`; see
/// the crate docs for the derivation, the concurrency argument, and the
/// refusal `errno`s (`ENOTSUP` / `EIO` / `EMFILE`).
///
/// [`CommandExt::pre_exec`]: std::os::unix::process::CommandExt::pre_exec
#[cfg(target_os = "macos")]
pub fn deny_inherited_fds(cmd: &mut std::process::Command) {
    // Planned in the PARENT: the child-side sweep must not call anything that
    // is not async-signal-safe (`getrlimit`, `sysctlbyname` and `readdir` are
    // not on that list). A bound that cannot be justified refuses the spawn
    // rather than sweeping a truncated range (agent-bridle#352).
    sweep::install(cmd, sweep::plan());
}

/// Parent-side sweep planning (macOS enforcement leg; also compiled under
/// `cfg(test)` on Linux so its fail-closed decision table is exercised there).
#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
mod sweep;

#[cfg(unix)]
mod beneath;
#[cfg(unix)]
pub use beneath::{is_resolution_refusal, GrantedRoot, RootIdentity};

/// No-op on platforms without an enforcement leg: the CLOEXEC-convention
/// residual remains there (see the crate docs — on Windows the confined path
/// spawns via `agent-bridle-aclaunch`'s explicit handle allowlist instead).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn deny_inherited_fds(_cmd: &mut std::process::Command) {}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::process::{Command, Stdio};

    // Ambient-descriptor closure is proven in `tests/hostile_fds.rs` (file,
    // socket, pipe, `dup`, a descriptor at the top of the table, concurrent
    // creation, delegated stdio) and, for the sweep bound itself, in
    // `sweep::tests`. Those probes ask the CHILD'S OWN descriptor table via
    // `fcntl`; the shell-redirection probes that used to live here were unsound
    // on Darwin — see the module docs of `tests/hostile_fds.rs`.

    /// A confined spawn of a MISSING program must still report the failure as
    /// `io::ErrorKind::NotFound`, never a bogus `Ok`. Regression for the
    /// mark-don't-close choice on BOTH legs: closing fds `>= 3` *immediately*
    /// also closes std's own `CLOEXEC` exec-status pipe, so a failed `exec`
    /// cannot be reported — `spawn` returns `Ok` with a child killed by
    /// `SIGABRT`. Close-on-exec marking leaves that pipe to close at `exec`, so
    /// the error surfaces correctly. This test FAILS on an immediate-close
    /// variant and PASSES with close-on-exec marking.
    #[test]
    fn deny_inherited_fds_reports_a_missing_program_as_not_found() {
        let mut cmd = Command::new("/nonexistent/agent-bridle-fdguard-missing-xyzzy");
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        super::deny_inherited_fds(&mut cmd);
        let err = cmd
            .spawn()
            .expect_err("spawning a missing program must fail, not succeed");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::NotFound,
            "a missing confined program must surface as NotFound, not a bogus spawn success"
        );
    }
}

/// Duplicate an explicitly delegated Unix stream descriptor with close-on-exec.
///
/// This does not take ownership of `source`, open a path, or authenticate its
/// peer. The caller must authenticate its protocol and scope before using it.
/// Validation occurs on the duplicate so descriptor reuse cannot change the
/// resource between validation and ownership. Standard streams are refused.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn duplicate_control_socket(
    source: std::os::fd::RawFd,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    if source < 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a delegated control endpoint must not replace standard streams",
        ));
    }
    // SAFETY: fcntl accepts any integer descriptor and returns either a new,
    // exclusively owned descriptor or -1. We do not dereference caller memory
    // or assume that source remains open across the call.
    let duplicated = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicated < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: successful F_DUPFD_CLOEXEC returned this new owned descriptor.
    let owned = unsafe { OwnedFd::from_raw_fd(duplicated) };
    let mut kind: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&kind) as libc::socklen_t;
    // SAFETY: both pointers refer to valid writable locals of the stated sizes;
    // owned remains live for the complete call.
    let result = unsafe {
        libc::getsockopt(
            owned.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut size,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if kind != libc::SOCK_STREAM || size as usize != std::mem::size_of_val(&kind) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "delegated control endpoint is not a stream socket",
        ));
    }
    let socket = std::os::unix::net::UnixStream::from(owned);
    // std validates the socket address family and requires a connected peer.
    socket.peer_addr()?;
    Ok(socket)
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod delegated_control_tests {
    use super::duplicate_control_socket;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn duplicate_control_preserves_original_and_sets_cloexec() {
        let (original, mut peer) = UnixStream::pair().unwrap();
        let mut duplicate = duplicate_control_socket(original.as_raw_fd()).unwrap();
        // SAFETY: duplicate remains live, F_GETFD has no pointer arguments.
        let flags = unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        duplicate.write_all(b"a").unwrap();
        let mut byte = [0];
        peer.read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"a");
        drop(duplicate);
        (&original).write_all(b"b").unwrap();
        peer.read_exact(&mut byte).unwrap();
        assert_eq!(byte, *b"b");
    }

    #[test]
    fn duplicate_control_rejects_files_and_standard_streams() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(duplicate_control_socket(file.as_raw_fd()).is_err());
        assert!(duplicate_control_socket(0).is_err());
        assert!(duplicate_control_socket(-1).is_err());
        // Validation failed after duplication; the original remains owned.
        assert!(file.metadata().is_ok());
    }
}
