//! Parent-side planning for the macOS descriptor sweep (agent-bridle#319/#352).
//!
//! macOS has no `close_range(2)`, so the confined-spawn guard has to walk the
//! descriptor table itself with `fcntl(F_SETFD, FD_CLOEXEC)`. A walk needs a
//! bound, and a bound is a *security* object: any descriptor above it survives
//! `exec` unmarked. This module derives that bound in the parent (where
//! non-async-signal-safe calls are allowed) and — the point of #352 — **refuses
//! the spawn** when no trustworthy bound can be established, instead of clamping
//! a required range down to something cheap to scan.
//!
//! Compiled on macOS (the enforcement leg) and, under `cfg(test)`, on Linux, so
//! the fail-closed decision table and both `pre_exec` installers are exercised
//! by the Linux CI tier as well as by the macOS job.

use std::process::Command;

/// Largest descriptor universe the child-side sweep is willing to visit.
///
/// This is a *cost* cap on the sweep, and it is deliberately **not** allowed to
/// act as a security boundary: a required bound above it does not get clamped,
/// it refuses the spawn ([`Refusal::RangeExceedsSweepCap`]). 2^20 `fcntl` calls
/// is the most async-signal-safe work we are willing to do between `fork` and
/// `exec`; a process whose descriptor universe is genuinely wider than that
/// cannot be confined by this mechanism, and saying so is the honest answer.
pub(crate) const SWEEP_CAP: u64 = 1 << 20;

/// Why a confined spawn must be refused rather than swept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// No trustworthy ceiling on the descriptors this process may still
    /// allocate: `getrlimit(RLIMIT_NOFILE)` failed or is `RLIM_INFINITY`, *and*
    /// `kern.maxfilesperproc` could not be read either.
    UnknownAllocationLimit,
    /// The descriptors already open could not be enumerated: `/dev/fd` was
    /// unreadable, an entry failed to read, or an entry was not a descriptor
    /// number. A partial enumeration is treated as no enumeration.
    UnknownOpenDescriptors,
    /// The required range is wider than [`SWEEP_CAP`], so the sweep cannot cover
    /// the descriptor universe. Refused, never truncated.
    RangeExceedsSweepCap,
}

impl Refusal {
    /// `errno` the refused spawn reports.
    ///
    /// A `pre_exec` failure reaches the parent as a bare `errno` — std writes
    /// `raw_os_error()` down its exec-status pipe and reconstructs the error
    /// there — so the *reason* has to ride in the code itself. Building a
    /// message instead would allocate, which is forbidden after `fork` in a
    /// multi-threaded parent.
    pub(crate) fn errno(self) -> libc::c_int {
        match self {
            Refusal::UnknownAllocationLimit => libc::ENOTSUP,
            Refusal::UnknownOpenDescriptors => libc::EIO,
            Refusal::RangeExceedsSweepCap => libc::EMFILE,
        }
    }
}

/// Decide the exclusive sweep bound, or refuse.
///
/// The descriptor universe a forked child can inherit is bounded by the union
/// of two sets, so the sweep bound must cover both:
///
/// - **Descriptors already open** when the guard is installed. Enumerated from
///   `/dev/fd` (`highest_open`); one past the highest of them covers the set.
///   These can sit *above* the current soft limit if the limit was lowered
///   after they were opened, which is why the allocation limit alone is not
///   sufficient.
/// - **Descriptors created between installation and `fork`** — by this thread
///   (`Command::spawn` itself creates stdio pipes and its exec-status pipe) or
///   by any other thread. Every such descriptor is allocated below the
///   allocation limit in force at creation time (`alloc_limit`): XNU's
///   `fdalloc` bounds an allocation by `min(RLIMIT_NOFILE.rlim_cur,
///   kern.maxfilesperproc)`, and `dup2`/`F_DUPFD` reject a target at or above
///   that same limit.
///
/// Hence `bound = max(alloc_limit, highest_open + 1)`, and a missing input is
/// not a bound at all: `None` on either leg refuses. If the resulting required
/// range exceeds what the child-side sweep can inspect ([`SWEEP_CAP`]), the
/// spawn is refused — `required <= supported` is checked, never assumed.
pub(crate) fn plan_sweep_bound(
    alloc_limit: Option<u64>,
    highest_open: Option<u64>,
) -> Result<u64, Refusal> {
    let alloc_limit = alloc_limit.ok_or(Refusal::UnknownAllocationLimit)?;
    let highest_open = highest_open.ok_or(Refusal::UnknownOpenDescriptors)?;
    let required = alloc_limit.max(highest_open.saturating_add(1));
    if required > SWEEP_CAP {
        return Err(Refusal::RangeExceedsSweepCap);
    }
    Ok(required)
}

/// One past the highest descriptor currently open, or `None` if the descriptor
/// table could not be enumerated **completely**.
///
/// `/dev/fd` is the per-process descriptor directory on macOS (devfs) and a
/// `/proc/self/fd` symlink on Linux. Any failure — the directory is unreadable,
/// an entry errors mid-iteration, an entry is not a number — yields `None`,
/// which refuses the spawn. A partially enumerated table is indistinguishable
/// from a table with a descriptor we did not see.
pub(crate) fn highest_open_fd() -> Option<u64> {
    let mut highest: u64 = 0;
    for entry in std::fs::read_dir("/dev/fd").ok()? {
        let fd = entry.ok()?.file_name().to_str()?.parse::<u64>().ok()?;
        highest = highest.max(fd);
    }
    Some(highest)
}

/// The ceiling the kernel enforces on descriptor *allocation* for this process,
/// or `None` if neither source can be read.
///
/// `min(RLIMIT_NOFILE.rlim_cur, kern.maxfilesperproc)` is the value XNU's
/// `fdalloc` compares against, so it is the tightest trustworthy bound on any
/// descriptor number this process can still obtain. `RLIM_INFINITY` is not a
/// bound; when the soft limit is unlimited the sysctl is the only ceiling, and
/// if that is unreadable too the answer is "unknown" (⇒ refuse), never a guess.
#[cfg(target_os = "macos")]
pub(crate) fn allocation_limit() -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` writes only into the stack struct above; no aliasing.
    let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0
        && lim.rlim_cur != libc::RLIM_INFINITY
    {
        Some(lim.rlim_cur)
    } else {
        None
    };

    match (soft, max_files_per_proc()) {
        (Some(soft), Some(sysctl)) => Some(soft.min(sysctl)),
        (Some(soft), None) => Some(soft),
        (None, Some(sysctl)) => Some(sysctl),
        (None, None) => None,
    }
}

/// `kern.maxfilesperproc` — the kernel's own per-process descriptor ceiling.
#[cfg(target_os = "macos")]
fn max_files_per_proc() -> Option<u64> {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: `sysctlbyname` writes at most `size` bytes into `value` and
    // updates `size`; the name is a NUL-terminated literal and the new-value
    // pointer is null (read-only query).
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            std::ptr::from_mut(&mut value).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size != std::mem::size_of::<libc::c_int>() || value <= 0 {
        return None;
    }
    u64::try_from(value).ok()
}

/// Install the parent-side plan on `cmd`: sweep the planned range, or refuse.
///
/// The dispatch lives here, and takes the plan as an argument, so a test can
/// drive the *production* path with a constructed plan — including the refusal
/// branches that a healthy machine cannot reach on its own (on macOS 26 with
/// `kern.maxfilesperproc` = 61440, no descriptor above the cap can exist, so
/// `RangeExceedsSweepCap` is unreachable by ordinary means).
pub(crate) fn install(cmd: &mut Command, plan: Result<libc::c_int, Refusal>) {
    match plan {
        Ok(bound) => install_sweep(cmd, bound),
        Err(refusal) => install_refusal(cmd, refusal),
    }
}

/// The parent-side plan for this spawn: a sweepable bound, or a refusal.
#[cfg(target_os = "macos")]
pub(crate) fn plan() -> Result<libc::c_int, Refusal> {
    plan_sweep_bound(allocation_limit(), highest_open_fd()).map(|bound| {
        // `plan_sweep_bound` refuses anything above SWEEP_CAP (2^20), so this
        // conversion cannot truncate; it runs in the parent, before `fork`.
        libc::c_int::try_from(bound).expect("sweep bound is capped at SWEEP_CAP")
    })
}

/// Install the child-side sweep over `[3, bound)`.
pub(crate) fn install_sweep(cmd: &mut Command, bound: libc::c_int) {
    use std::os::unix::process::CommandExt;

    // SAFETY: the closure runs in the forked child after `fork` and before
    // `exec`. It calls only `fcntl` — async-signal-safe per POSIX — and
    // `io::Error::last_os_error`/`raw_os_error` (an `errno` read; OS-errno
    // errors do not allocate); it allocates nothing, takes no locks, and is
    // bounded by `bound` (<= `SWEEP_CAP`), satisfying `pre_exec`'s
    // async-signal-safety contract. Rust's `Command` performs its stdio `dup2`
    // onto fds 0/1/2 *before* running pre-exec hooks, so sweeping `[3, bound)`
    // covers only ambient descriptors and never the child's delegated stdio.
    // Marking `FD_CLOEXEC` rather than closing immediately keeps std's own
    // `CLOEXEC` exec-status pipe alive to report an `exec` failure (it is
    // already close-on-exec, so re-marking it is a no-op); every marked fd
    // closes atomically at `exec`, so the confined child never inherits one.
    unsafe {
        cmd.pre_exec(move || {
            for fd in 3..bound {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags == -1 {
                    let err = std::io::Error::last_os_error();
                    // A hole in the descriptor table is not an error.
                    if err.raw_os_error() == Some(libc::EBADF) {
                        continue;
                    }
                    return Err(err);
                }
                if flags & libc::FD_CLOEXEC == 0
                    && libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// Install a hook that refuses the spawn: the descriptor universe cannot be
/// proven sweepable, so no confined child may run.
///
/// The refusal is decided in the parent but *reported* from `pre_exec` so that
/// `deny_inherited_fds` stays infallible for callers (no API churn at the three
/// confined-spawn sites) while `Command::spawn` still returns `Err` and `exec`
/// never happens.
pub(crate) fn install_refusal(cmd: &mut Command, refusal: Refusal) {
    use std::os::unix::process::CommandExt;

    let errno = refusal.errno();
    // SAFETY: the closure runs after `fork` and before `exec`; it only builds an
    // `io::Error` from a raw `errno` (a plain integer copy — no allocation, no
    // locks, no non-reentrant calls) and returns it, which makes std abandon the
    // exec and report the failure to the parent.
    unsafe {
        cmd.pre_exec(move || Err(std::io::Error::from_raw_os_error(errno)));
    }
}

/// Serializes the tests that place a descriptor at a *specific* number.
///
/// `cargo test` runs the unit tests of a crate as threads of one process, and
/// `dup2` onto a chosen descriptor number is a process-wide operation: without
/// this lock two tests can pick the same free slot, or one can close the other's
/// descriptor. Poisoning is ignored deliberately — a panicking test must not
/// cascade into unrelated failures.
#[cfg(test)]
pub(crate) static FD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::process::Stdio;

    fn fd_lock() -> std::sync::MutexGuard<'static, ()> {
        FD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Report a fixture that cannot run here — and FAIL instead when the
    /// caller demands it.
    ///
    /// A silently-skipping test is a passing test that checked nothing, which
    /// is the same vacuity this suite just finished removing from its harness
    /// entrypoints; libtest captures a passing test's output, so the message
    /// below is invisible without `--nocapture`. Set
    /// `BRIDLE_REQUIRE_FD_FIXTURES=1` (CI, and any run being used as evidence)
    /// to turn the skip into a hard failure, mirroring
    /// `BRIDLE_REQUIRE_SEATBELT` / `BRIDLE_REQUIRE_LANDLOCK` elsewhere in this
    /// repo. SKIP IS NOT PASS.
    fn skip_or_fail_fixture(test: &str, offset: u64) {
        let why = format!(
            "the soft RLIMIT_NOFILE leaves no free descriptor {offset} above the \
             open table, so the FIXTURE cannot place one (the guard itself is \
             unaffected — raise `ulimit -n` to run it)"
        );
        // The two paths must not read alike in a log: a FAILURE whose message
        // opens with "skipping" is unreadable at a glance.
        assert!(
            std::env::var_os("BRIDLE_REQUIRE_FD_FIXTURES").is_none(),
            "REQUIRED FIXTURE UNAVAILABLE: {test} could not run because {why}"
        );
        eprintln!("skipping {test}: {why}");
    }

    /// A free descriptor number `offset` above the current table, or `None`
    /// when the soft `RLIMIT_NOFILE` leaves no room for one.
    ///
    /// The FIXTURE needs headroom the product code does not: these tests place
    /// a descriptor high in the table on purpose. Under a lowered limit
    /// (`ulimit -n 64` in a container, a hardened runner, a launchd job) the
    /// placement itself fails, which reads as a guard defect and is not one.
    /// Callers report a named skip instead.
    fn owned_slot_above_table(offset: u64) -> Option<libc::c_int> {
        let base = highest_open_fd().expect("/dev/fd must be enumerable");
        let target = base + offset;
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `getrlimit` writes only into the stack struct above.
        let soft = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0 {
            lim.rlim_cur
        } else {
            u64::MAX
        };
        if target >= soft {
            return None;
        }
        libc::c_int::try_from(target).ok()
    }

    /// Descriptor number the re-exec'd child must report on.
    const PROBE_ENV: &str = "BRIDLE_FDGUARD_UNIT_PROBE";

    /// NOT A TEST — the child-process entrypoint the probes `exec`.
    ///
    /// It asserts nothing: when `PROBE_ENV` is set it reports whether the named
    /// descriptor is open in ITS OWN table (a shell cannot answer that soundly
    /// — see the module docs of `tests/hostile_fds.rs`), and otherwise returns.
    /// `#[ignore]` keeps it out of the passed count, because a `#[test]` that
    /// always succeeds inflates static enumeration and the runtime `--list`
    /// **identically** — so the count cross-check protocol, which exists to
    /// catch vacuous evidence, is structurally blind to it. The probes pass
    /// `--include-ignored` to invoke it deliberately.
    #[test]
    #[ignore = "child-process entrypoint invoked by the probes, not coverage"]
    fn harness_child_entrypoint_not_a_test() {
        let Ok(target) = std::env::var(PROBE_ENV) else {
            return; // ordinary test run: nothing to do
        };
        let fd: libc::c_int = target.parse().expect("probe target");
        // SAFETY: `fcntl(F_GETFD)` is defined for any integer — it reports
        // `EBADF` for a descriptor number that is not open.
        let open = unsafe { libc::fcntl(fd, libc::F_GETFD) } != -1;
        // Leading newline: the harness's own line and this report can otherwise
        // share a line, which would hide the marker from the parser.
        println!("\nFDPROBE {fd} {}", if open { "open" } else { "closed" });
        println!("PROBE_DONE");
    }

    /// Spawn the probe child with `plan` installed; `true` if the child found
    /// `fd` still open (i.e. the descriptor was inherited).
    fn probe_child(fd: libc::c_int, plan: Result<libc::c_int, Refusal>) -> bool {
        let mut cmd = Command::new(std::env::current_exe().expect("current_exe"));
        cmd.args([
            "--exact",
            "sweep::tests::harness_child_entrypoint_not_a_test",
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROBE_ENV, fd.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
        install(&mut cmd, plan);
        let out = cmd.output().expect("spawn the probe child");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            text.contains("PROBE_DONE"),
            "the probe child did not run to completion — a vacuous result:\n{text}"
        );
        text.contains(&format!("FDPROBE {fd} open"))
    }

    /// The bound covers the descriptors already open even when they sit above
    /// the current allocation limit (a soft limit lowered after they opened).
    #[test]
    fn the_sweep_bound_covers_both_the_allocation_limit_and_the_open_table() {
        assert_eq!(plan_sweep_bound(Some(256), Some(10)), Ok(256));
        assert_eq!(plan_sweep_bound(Some(64), Some(300)), Ok(301));
        assert_eq!(plan_sweep_bound(Some(0), Some(0)), Ok(1));
    }

    /// The defect this module exists to remove, stated as a test: the previous
    /// bound was `max(min(rlimit, CAP), min(highest + 1, CAP))`, so a descriptor
    /// above `CAP` produced a bound *below that descriptor* — and the spawn was
    /// still permitted. The replacement refuses instead. Regression for
    /// agent-bridle#352.
    #[test]
    fn the_old_clamped_bound_swept_below_a_live_descriptor_where_this_one_refuses() {
        let old_clamped_bound =
            |rlimit: u64, highest: u64| rlimit.min(SWEEP_CAP).max((highest + 1).min(SWEEP_CAP));
        let highest = SWEEP_CAP + 5;
        assert!(
            old_clamped_bound(SWEEP_CAP * 2, highest) <= highest,
            "the old bound silently skipped fd {highest} and spawned anyway"
        );
        assert_eq!(
            plan_sweep_bound(Some(SWEEP_CAP * 2), Some(highest)),
            Err(Refusal::RangeExceedsSweepCap),
            "a descriptor universe wider than the sweep must refuse the spawn"
        );
    }

    /// An unknown input is not a bound. Regression for agent-bridle#352: the
    /// old implementation substituted a constant when `getrlimit` failed and
    /// treated an unreadable `/dev/fd` as "no descriptors open".
    #[test]
    fn an_unknowable_descriptor_universe_refuses_instead_of_guessing() {
        assert_eq!(
            plan_sweep_bound(None, Some(10)),
            Err(Refusal::UnknownAllocationLimit)
        );
        assert_eq!(
            plan_sweep_bound(Some(256), None),
            Err(Refusal::UnknownOpenDescriptors)
        );
        assert_eq!(
            plan_sweep_bound(None, None),
            Err(Refusal::UnknownAllocationLimit)
        );
    }

    /// `required <= supported` decides; a wider requirement refuses rather than
    /// clamping. Regression for agent-bridle#352 (the cap used to be a silent
    /// `min`, which let a descriptor above it survive `exec` unmarked).
    #[test]
    fn a_required_range_wider_than_the_cap_refuses_instead_of_clamping() {
        assert_eq!(plan_sweep_bound(Some(SWEEP_CAP), Some(3)), Ok(SWEEP_CAP));
        assert_eq!(
            plan_sweep_bound(Some(SWEEP_CAP + 1), Some(3)),
            Err(Refusal::RangeExceedsSweepCap)
        );
        assert_eq!(
            plan_sweep_bound(Some(64), Some(SWEEP_CAP)),
            Err(Refusal::RangeExceedsSweepCap)
        );
        assert_eq!(
            plan_sweep_bound(Some(u64::MAX), Some(u64::MAX)),
            Err(Refusal::RangeExceedsSweepCap)
        );
    }

    /// Each refusal reason reaches the parent as its own `errno`, so a
    /// fail-closed spawn is diagnosable without allocating after `fork`.
    #[test]
    fn every_refusal_reason_has_a_distinct_errno() {
        let codes = [
            Refusal::UnknownAllocationLimit.errno(),
            Refusal::UnknownOpenDescriptors.errno(),
            Refusal::RangeExceedsSweepCap.errno(),
        ];
        for (i, a) in codes.iter().enumerate() {
            for b in &codes[i + 1..] {
                assert_ne!(a, b, "refusal errnos must be distinguishable");
            }
        }
    }

    /// The enumeration leg actually sees a descriptor placed near the top of the
    /// table — it is not silently reporting only the low, dense range.
    #[test]
    fn the_enumeration_leg_sees_a_descriptor_high_in_the_table() {
        let _guard = fd_lock();
        let Some(target) = owned_slot_above_table(64) else {
            skip_or_fail_fixture(
                "the_enumeration_leg_sees_a_descriptor_high_in_the_table",
                64,
            );
            return;
        };
        let file = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: test-only `dup2` onto a descriptor number proven free below.
        unsafe {
            assert_eq!(
                libc::fcntl(target, libc::F_GETFD),
                -1,
                "target must be free"
            );
            assert!(libc::dup2(file.as_raw_fd(), target) >= 0, "dup2 failed");
        }
        let seen = highest_open_fd().expect("/dev/fd must be enumerable");
        // SAFETY: closing the descriptor this test created.
        unsafe {
            libc::close(target);
        }
        assert!(
            seen >= u64::try_from(target).unwrap(),
            "enumeration must see fd {target}, saw a maximum of {seen}"
        );
    }

    /// A refused plan fails the spawn closed: `Command::spawn` errors with the
    /// refusal's `errno` and the program never runs (no marker file appears).
    /// Driven through [`install`] — the same dispatch `deny_inherited_fds` uses
    /// — because on a healthy machine the refusal branches are unreachable from
    /// the OS probes alone. Regression for agent-bridle#352: an unsupported
    /// descriptor universe must not fall back to "spawn anyway".
    #[test]
    fn a_refused_plan_fails_the_spawn_and_the_program_never_runs() {
        let marker = std::env::temp_dir().join(format!(
            "fdguard-refusal-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_file(&marker);

        for refusal in [
            Refusal::UnknownAllocationLimit,
            Refusal::UnknownOpenDescriptors,
            Refusal::RangeExceedsSweepCap,
        ] {
            let mut cmd = Command::new("/bin/bash");
            cmd.arg("-c")
                .arg(format!("touch {}", marker.display()))
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            install(&mut cmd, Err(refusal));
            let err = cmd
                .spawn()
                .expect_err("a refused confined spawn must not succeed");
            assert_eq!(
                err.raw_os_error(),
                Some(refusal.errno()),
                "the refusal reason must survive to the parent"
            );
            assert!(
                !marker.exists(),
                "the confined program must never run when the guard refuses"
            );
        }
    }

    /// Why the cap may not be a silent clamp, demonstrated: a sweep bound that
    /// stops below a live ambient descriptor LEAKS it into the child, while the
    /// honest bound closes it. Same command, same descriptor, two bounds.
    /// Regression for agent-bridle#352 (this is the property the old
    /// `min(bound, 1 << 20)` traded away silently).
    ///
    /// The child is a re-exec of this test binary reporting on its own
    /// descriptor table (`probe_child`) — never a shell. See the module docs of
    /// `tests/hostile_fds.rs` for why a shell cannot be trusted with this
    /// question on Darwin.
    #[test]
    fn a_truncated_sweep_bound_leaks_a_descriptor_the_honest_bound_closes() {
        let _guard = fd_lock();
        let Some(target) = owned_slot_above_table(32) else {
            skip_or_fail_fixture(
                "a_truncated_sweep_bound_leaks_a_descriptor_the_honest_bound_closes",
                32,
            );
            return;
        };
        let file = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: test-only `dup2` onto a descriptor number proven free; `dup2`
        // clears `FD_CLOEXEC` on the new descriptor, which is exactly the
        // ambient (inheritable) descriptor being modelled.
        unsafe {
            assert_eq!(
                libc::fcntl(target, libc::F_GETFD),
                -1,
                "target must be free"
            );
            assert!(libc::dup2(file.as_raw_fd(), target) >= 0, "dup2 failed");
        }

        let leaked = probe_child(target, Ok(4));
        let closed = !probe_child(target, Ok(target + 1));

        // SAFETY: closing the descriptor this test created.
        unsafe {
            libc::close(target);
        }
        assert!(
            leaked,
            "a bound truncated below fd {target} must leak it — otherwise this \
             test proves nothing about clamping"
        );
        assert!(
            closed,
            "the honest bound must close fd {target} in the child"
        );
    }

    /// End-to-end fail-closed for the case the brief names explicitly: the
    /// required range exceeds the supported range. Planned by
    /// [`plan_sweep_bound`], dispatched by [`install`] (the production path),
    /// and the spawn must fail rather than sweep what fits.
    #[test]
    fn a_required_range_wider_than_the_supported_range_refuses_the_spawn() {
        let plan = plan_sweep_bound(Some(SWEEP_CAP + 1), highest_open_fd())
            .map(|bound| libc::c_int::try_from(bound).unwrap());
        assert_eq!(plan, Err(Refusal::RangeExceedsSweepCap));

        let mut cmd = Command::new("/bin/bash");
        cmd.arg("-c")
            .arg("exit 0")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        install(&mut cmd, plan);
        let err = cmd
            .spawn()
            .expect_err("required > supported must refuse the confined spawn");
        assert_eq!(err.raw_os_error(), Some(libc::EMFILE));
    }

    /// A soft limit *lowered* after descriptors were opened must not shrink the
    /// bound below the live table — the enumeration leg is what covers that
    /// case, and dropping it would silently leak every descriptor above the new
    /// limit. Uses the real `/dev/fd` enumeration with a synthetic (lowered)
    /// allocation limit, so no process-wide `setrlimit` is needed.
    #[test]
    fn a_lowered_allocation_limit_never_shrinks_the_bound_below_the_open_table() {
        let _guard = fd_lock();
        let Some(target) = owned_slot_above_table(16) else {
            skip_or_fail_fixture(
                "a_lowered_allocation_limit_never_shrinks_the_bound_below_the_open_table",
                16,
            );
            return;
        };
        let file = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: test-only `dup2` onto a descriptor number proven free below.
        unsafe {
            assert_eq!(
                libc::fcntl(target, libc::F_GETFD),
                -1,
                "target must be free"
            );
            assert!(libc::dup2(file.as_raw_fd(), target) >= 0, "dup2 failed");
        }
        let bound = plan_sweep_bound(Some(4), highest_open_fd());
        // SAFETY: closing the descriptor this test created.
        unsafe {
            libc::close(target);
        }
        assert_eq!(
            bound,
            Ok(u64::try_from(target).unwrap() + 1),
            "the bound must still cover fd {target}, which predates the lowered limit"
        );
    }

    /// The bound is *derived from the kernel's own per-process ceiling*, not a
    /// constant. On the reference machine (macOS 26.5.2, arm64, xnu-12377):
    /// soft `RLIMIT_NOFILE` = 1048576 — numerically identical to `SWEEP_CAP` —
    /// while `kern.maxfilesperproc` = 61440 and `dup2` to 61440 fails `EBADF`.
    /// The pre-#352 implementation planned 1048576 there; this assertion fails
    /// on that value and passes on 61440, so it discriminates the fix from the
    /// clamp on exactly the configuration that would otherwise pass vacuously.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_planned_bound_tracks_the_kernel_ceiling_rather_than_a_constant() {
        let _guard = fd_lock();
        let maxfiles = max_files_per_proc().expect("kern.maxfilesperproc must be readable");
        let highest = highest_open_fd().expect("/dev/fd must be enumerable");
        let planned = u64::try_from(plan().expect("macOS must plan a sweepable bound")).unwrap();

        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `getrlimit` writes only into the stack struct above.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) },
            0,
            "getrlimit"
        );
        let soft = if lim.rlim_cur == libc::RLIM_INFINITY {
            None
        } else {
            Some(lim.rlim_cur)
        };
        let expected_alloc = soft.map_or(maxfiles, |soft| soft.min(maxfiles));

        // The allocation ceiling dominates the open table by orders of
        // magnitude on any real machine (61440 vs a few dozen), so this
        // equality is stable even though the two probes are not atomic.
        //
        // Note honestly what this proves: the expected value is RE-DERIVED from
        // the same formula the implementation uses, so it establishes that the
        // bound tracks the kernel's ceilings rather than a constant — the #352
        // regression — and it CANNOT catch a shared misconception in which the
        // formula itself is wrong in code and test alike. On the reference
        // machine it discriminates for real: min(1048576, 61440) = 61440, while
        // the pre-#352 clamp produced 1048576, which fails this assertion.
        assert_eq!(
            planned,
            expected_alloc.max(highest + 1),
            "the planned bound must be min(RLIMIT_NOFILE, kern.maxfilesperproc) \
             widened to cover the open table — nothing else"
        );
    }
}
