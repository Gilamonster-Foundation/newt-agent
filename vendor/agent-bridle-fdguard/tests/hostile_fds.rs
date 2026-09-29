//! Hostile descriptor-inheritance tests for the confined-spawn guard
//! (agent-bridle#319 / #352), through the **public** API only.
//!
//! Each test builds an ambient descriptor of a different shape, proves with an
//! *unguarded* positive control that the descriptor really is inherited across
//! `exec`, then proves the guarded spawn closes it.
//!
//! ## The oracle is the child's own descriptor table — never a shell
//!
//! The child is a re-exec of this very test binary (`fd_probe_helper`), which
//! answers with `fcntl(F_GETFD)` / `F_GETPATH` on the descriptor numbers under
//! test and then *attempts a write* through each one. Nothing is inferred from
//! a shell.
//!
//! That is not fastidiousness. The earlier version of this suite asked
//! `/bin/bash -c 'echo probe >&N'` whether descriptor `N` was usable, and on
//! macOS 26.5.2 (bash 3.2.57, and `/bin/sh`, which is a *different* binary of
//! the same family) that oracle is unsound: with `N = 10` and the descriptor
//! **provably closed** — verified in a standalone C replica where the child's
//! inherited table was exactly `{0, 1, 2}` and the parent's file stayed empty —
//! the shell still resolved `>&10` to a descriptor of its own connected to
//! stdout, printed `probe`, and exited 0. Read as "the descriptor is usable",
//! that is a false alarm on a working guard; by the same mechanism a shell can
//! report "usable" for a descriptor the guard *did* leak-check, i.e. a false
//! pass. An oracle whose answer depends on the shell's own free-descriptor
//! layout cannot certify a descriptor boundary. Shells reserve descriptor
//! numbers from 10 upward for their internal save/restore slots; do not probe
//! there, and do not use a shell here at all.
//!
//! The two failure modes are one root cause. Commit 38ca0d2 had already moved
//! these probes from `/bin/sh` to `/bin/bash` because dash rejects a
//! multi-digit descriptor redirection at *parse* time — the false-FAIL face of
//! the same unsound oracle. Switching shells treated the symptom; the
//! false-PASS face (a shell answering `>&N` from a descriptor of its own)
//! survived the switch, because the defect was never the choice of shell.
//!
//! ## The red-before is deterministic, and it is NOT a race
//!
//! The old oracle fails on demand, single-threaded, as a function of the
//! descriptor NUMBER. On Darwin at 38ca0d2 (pre-rework), one command:
//!
//! ```text
//! bash -c 'exec 3</dev/null 4</dev/null 5</dev/null 6</dev/null \
//!          7</dev/null 8</dev/null 9</dev/null; \
//!          exec "$0" --test-threads=1' <fdguard-test-binary>
//! ```
//!
//! Pre-opening fds 3..9 forces the probe onto fd 10 and the old test fails
//! every time; forcing it onto 8, 9 or 11 passes every time. **There is no
//! concurrency, no timing and no race in this failure, and it must never be
//! cited as evidence of one.** It says nothing about how the sweep behaves
//! under concurrent descriptor creation — that question belongs to
//! `concurrent_descriptor_creation_never_reaches_the_confined_child` below, and
//! to agent-bridle#358 on its own native probe evidence.
//!
//! What it does show is why the defect stayed hidden: the trigger is a function
//! of **suite size**, not of the guard. Three CHECKOUTS — not three successive
//! states of one branch; `38ca0d2` is on the #319/#352 line while `b04ca39` and
//! `80f2213` are on the #351/#354 line — each run 100 times naturally, with
//! isolated target dirs and identical guard code throughout:
//!
//! | checkout | line | crate tests | natural failures / 100 |
//! |---|---|---|---|
//! | `38ca0d2` | #319/#352 | 2 | 0 — probe fd was 3 in 95 runs, 4 in the other 5 |
//! | `b04ca39` | #351/#354 | 7 | 0 — fd 10 never came up |
//! | `80f2213` | #351/#354 | 16 | 10 — every one at fd 10 |
//!
//! Do not read that as a trend line: this rework reports 12/13 lib tests, which
//! is not a fourth row and not a regression. The axis is how many descriptors
//! the suite already holds when the probe is allocated, and nothing else moved.
//! A green baseline was luck, not soundness: nothing competed for descriptors,
//! so the probe never reached the number where the shell answers for itself.
//!
//! Every ambient descriptor under test is also placed at a number this suite
//! *owns* (above everything currently open), so a test never probes a
//! descriptor belonging to the harness or to a concurrent test.
//!
//! ## Inspect any descriptor; WRITE only through the one you own
//!
//! The child reports on every descriptor it is asked about, but it attempts a
//! `write` through exactly one — the descriptor the calling test created
//! (`PROBE_WRITE_ENV`). Report-only is the default.
//!
//! This is not tidiness. An earlier revision wrote `probe` through *every* open
//! descriptor in the probed band, which in the unguarded control includes
//! descriptors the harness owns — among them duplicates of the inherited
//! stdout. Pushing this branch over HTTPS, `git` drives `remote-curl` as a
//! remote helper over its stdout pipe, and the pre-push hook inherits that same
//! descriptor; three probe children wrote into it and git rejected the
//! resulting command
//! (`unknown command 'probeprobeprobepush HEAD:refs/heads/...'`). A
//! descriptor-hygiene suite corrupted a protocol channel by writing through a
//! descriptor it did not own — exactly the class of bug this crate exists to
//! prevent. A `wrote=-1` in the report means "not attempted", not "failed".
//!
//! On Linux these exercise the `close_range(CLOSE_RANGE_CLOEXEC)` leg; on macOS
//! the planned `fcntl(F_SETFD, FD_CLOEXEC)` sweep. The contract is identical, so
//! the same suite is the acceptance evidence for both.
//!
//! **Running this suite from two checkouts:** give each one its own
//! `CARGO_TARGET_DIR`. A globally shared `target-dir` makes two worktrees
//! compile this crate to the same path with the same hash, so one silently
//! executes the other's binary; the only symptom is a wrong test count, which
//! is exactly how a stale binary once looked like a flaky guard. Treat the test
//! count as a first-class wrong-binary signal — but **derive it from the
//! checkout you are about to build** (`cargo test -p agent-bridle-fdguard --
//! --list`), never from a number quoted in prose. This crate is a stacking
//! point, so any absolute written down here is wrong by the next merge.
//!
//! **State the mechanism, derive the arithmetic.** What is durable is *which
//! tests are platform-gated and why*; the resulting count difference is a
//! consequence that changes whenever a gated test is added on either side. On
//! this branch the lib target has exactly one gated test —
//! `sweep::tests::the_planned_bound_tracks_the_kernel_ceiling_rather_than_a_constant`,
//! macOS-only because it reads `kern.maxfilesperproc`, which Linux does not
//! have — so macOS lists one more lib test than Linux *here*. That arithmetic
//! does not survive the #351/#354 stack merge, which adds a Linux-only
//! `beneath` test and removes a Linux-only test this rework superseded: the two
//! gates then cancel and both platforms list the same number. Enumerate the
//! gates in the checkout in front of you and derive the delta from them; a
//! transported number turns a correct run into a false alarm.
//!
//! This file is platform-independent and must list the same number on both.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::collections::BTreeMap;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Descriptor numbers the re-exec'd child must report on (comma-separated).
const PROBE_ENV: &str = "BRIDLE_FDGUARD_PROBE";

/// The single descriptor the child may WRITE through — the one the calling test
/// created and owns. Unset means report-only.
const PROBE_WRITE_ENV: &str = "BRIDLE_FDGUARD_PROBE_WRITE";

/// `cargo test` runs a test binary's tests as threads of one process, so
/// descriptor-table surgery has to be serialized: two tests must not choose the
/// same free descriptor number, and an unguarded control must not observe an
/// ambient descriptor another test is in the middle of closing.
static FD_LOCK: Mutex<()> = Mutex::new(());

fn fd_lock() -> MutexGuard<'static, ()> {
    FD_LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// NOT A TEST — the child-process entrypoint the probes `exec`.
///
/// It asserts nothing: when `BRIDLE_FDGUARD_PROBE` is set it reports on its OWN
/// descriptor table with `fcntl` and a real write attempt, and otherwise
/// returns. `#[ignore]` keeps it out of the passed count, because a `#[test]`
/// that always succeeds inflates static enumeration and the runtime `--list`
/// **identically** — so the count cross-check protocol, which exists to catch
/// vacuous evidence, is structurally blind to it. The probes pass
/// `--include-ignored` to invoke it deliberately.
///
/// Lines carry a `FDPROBE` marker — `FDPROBE <n> open cloexec=<0|1> wrote=<n>`
/// or `FDPROBE <n> closed` — because the harness's own output and the probe
/// write itself can share a line; the parser locates the marker rather than
/// assuming a line start. `PROBE_DONE` terminates the report, and its absence
/// means the child did not run, which the assertions treat as a failure rather
/// than a pass.
#[test]
#[ignore = "child-process entrypoint invoked by the probes, not coverage"]
fn harness_child_entrypoint_not_a_test() {
    let Ok(targets) = std::env::var(PROBE_ENV) else {
        return; // ordinary test run: nothing to do
    };
    // The ONE descriptor the calling test owns and may be written through. Any
    // other probed descriptor is inspected but never written to: a probe that
    // writes into a descriptor it does not own corrupts whatever that
    // descriptor is attached to — see the module docs.
    let owned: Option<RawFd> = std::env::var(PROBE_WRITE_ENV)
        .ok()
        .and_then(|fd| fd.parse().ok());

    for target in targets.split(',').filter(|t| !t.is_empty()) {
        let fd: RawFd = target.parse().expect("probe target");
        // SAFETY: `fcntl`/`write` on a descriptor number, both defined for any
        // integer (they report `EBADF` for a closed one). The write happens
        // only for the descriptor this test owns, and is what a leaked
        // capability would let this child do with it.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags == -1 {
                println!("\nFDPROBE {fd} closed");
                continue;
            }
            let wrote = if owned == Some(fd) {
                libc::write(fd, c"probe".as_ptr().cast(), 5)
            } else {
                -1
            };
            let cloexec = i32::from(flags & libc::FD_CLOEXEC != 0);
            println!("\nFDPROBE {fd} open cloexec={cloexec} wrote={wrote}");
        }
    }
    println!("\nPROBE_DONE");
}

/// What the child reported for each probed descriptor: `Some(wrote)` if the
/// descriptor was open in the child, `None` if it was closed.
type ProbeReport = BTreeMap<RawFd, Option<isize>>;

/// Run the probe child over `targets`, with or without the guard installed.
fn probe_child(targets: &[RawFd], owned: Option<RawFd>, guarded: bool) -> ProbeReport {
    let list = targets
        .iter()
        .map(RawFd::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut cmd = Command::new(std::env::current_exe().expect("current_exe"));
    cmd.args([
        "--exact",
        "harness_child_entrypoint_not_a_test",
        "--include-ignored",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(PROBE_ENV, list)
    .env_remove(PROBE_WRITE_ENV)
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    if let Some(fd) = owned {
        cmd.env(PROBE_WRITE_ENV, fd.to_string());
    }
    if guarded {
        agent_bridle_fdguard::deny_inherited_fds(&mut cmd);
    }
    let out = cmd.output().expect("spawn the probe child");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("PROBE_DONE"),
        "the probe child did not run to completion — a vacuous result, not a pass:\n{text}"
    );

    let mut report = ProbeReport::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace().skip_while(|t| *t != "FDPROBE");
        if parts.next() != Some("FDPROBE") {
            continue;
        }
        let Some(fd) = parts.next().and_then(|f| f.parse::<RawFd>().ok()) else {
            continue;
        };
        match parts.next() {
            Some("closed") => {
                report.insert(fd, None);
            }
            Some("open") => {
                let wrote = parts
                    .find_map(|f| f.strip_prefix("wrote="))
                    .and_then(|w| w.parse::<isize>().ok())
                    .expect("wrote= field");
                report.insert(fd, Some(wrote));
            }
            _ => {}
        }
    }
    report
}

/// Prove the descriptor is inherited without the guard (positive control) and
/// closed with it — asked of the child's own descriptor table, twice.
fn assert_guard_closes(fd: RawFd, what: &str) {
    let control = probe_child(&[fd], Some(fd), false);
    assert!(
        matches!(control.get(&fd), Some(Some(_))),
        "positive control: an unguarded child must inherit the {what} at fd {fd}: {control:?}"
    );

    let guarded = probe_child(&[fd], Some(fd), true);
    assert_eq!(
        guarded.get(&fd),
        Some(&None),
        "the confined child inherited an ambient descriptor ({what} at fd {fd}): {guarded:?}"
    );
}

/// One past the highest descriptor currently open in this process.
fn highest_open_fd() -> RawFd {
    let mut highest = 0;
    for entry in std::fs::read_dir("/dev/fd").expect("/dev/fd") {
        if let Some(fd) = entry
            .expect("dir entry")
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<RawFd>().ok())
        {
            highest = highest.max(fd);
        }
    }
    highest
}

/// Move `source` to a descriptor number this test OWNS: above everything
/// currently open, so no harness or concurrent-test descriptor is ever probed.
/// `dup2` clears `FD_CLOEXEC` on the target, which is what makes it ambient.
fn place_ambient(source: RawFd) -> RawFd {
    let target = highest_open_fd() + 16;
    // The FIXTURE needs headroom the product code does not: it places a
    // descriptor 16 above the table on purpose. Under a lowered soft limit
    // (`ulimit -n 64` in a container or a hardened runner) that placement
    // fails, which would read as a guard defect and is not one.
    if let Some(soft) = soft_fd_limit() {
        assert!(
            u64::try_from(target).is_ok_and(|t| t < soft),
            "this fixture needs a free descriptor at {target}, but the soft \
             RLIMIT_NOFILE is {soft}; raise it (ulimit -n) to run this suite"
        );
    }
    // SAFETY: `dup2` onto a descriptor number proven free immediately below.
    unsafe {
        assert_eq!(
            libc::fcntl(target, libc::F_GETFD),
            -1,
            "fd {target} must be free for this test"
        );
        assert!(libc::dup2(source, target) >= 0, "dup2 to {target}");
        assert_eq!(
            libc::fcntl(target, libc::F_GETFD) & libc::FD_CLOEXEC,
            0,
            "dup2 must leave the new descriptor inheritable"
        );
    }
    target
}

/// Clear `FD_CLOEXEC`, turning a descriptor into an ambient (inheritable) one.
fn make_inheritable(fd: RawFd) {
    // SAFETY: test-only `fcntl` on a descriptor this test owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        assert_ne!(flags, -1, "F_GETFD on fd {fd}");
        assert_eq!(libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC), 0);
    }
}

/// Close a descriptor this suite created.
fn close_fd(fd: RawFd) {
    // SAFETY: closing a descriptor number this suite owns.
    unsafe {
        libc::close(fd);
    }
}

fn temp_file(tag: &str) -> (std::path::PathBuf, std::fs::File) {
    let path = std::env::temp_dir().join(format!("fdguard-{}-{tag}", std::process::id()));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .expect("temp file");
    (path, file)
}

/// An ordinary inherited file descriptor.
#[test]
fn an_ambient_file_descriptor_is_closed_in_the_confined_child() {
    let _guard = fd_lock();
    let (path, file) = temp_file("file");
    make_inheritable(file.as_raw_fd());
    let ambient = place_ambient(file.as_raw_fd());
    assert_guard_closes(ambient, "regular file");
    close_fd(ambient);
    drop(file);
    let _ = std::fs::remove_file(path);
}

/// A socket — a descriptor that is a *network* capability, not a file one.
#[test]
fn an_ambient_socket_descriptor_is_closed_in_the_confined_child() {
    let _guard = fd_lock();
    let (sock, _peer) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    make_inheritable(sock.as_raw_fd());
    let ambient = place_ambient(sock.as_raw_fd());
    assert_guard_closes(ambient, "unix socket");
    close_fd(ambient);
}

/// A pipe write end — the shape a leaked descriptor takes when a parent forgets
/// to close the other side of an IPC channel.
#[test]
fn an_ambient_pipe_descriptor_is_closed_in_the_confined_child() {
    let _guard = fd_lock();
    let mut fds: [libc::c_int; 2] = [-1, -1];
    // SAFETY: `pipe` writes two descriptors into the array above.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    make_inheritable(fds[1]);
    let ambient = place_ambient(fds[1]);
    assert_guard_closes(ambient, "pipe write end");
    close_fd(ambient);
    close_fd(fds[0]);
    close_fd(fds[1]);
}

/// A *duplicated* descriptor. `dup(2)` clears `FD_CLOEXEC` on the new
/// descriptor, so duplication silently manufactures an ambient capability from
/// a well-behaved close-on-exec one — the guard must catch the copy, not just
/// the original.
#[test]
fn a_duplicated_descriptor_is_closed_in_the_confined_child() {
    let _guard = fd_lock();
    let (path, file) = temp_file("dup");
    // The ORIGINAL keeps CLOEXEC; only the duplicate is ambient.
    // SAFETY: test-only `dup` of a descriptor this test owns.
    let copy = unsafe { libc::dup(file.as_raw_fd()) };
    assert!(copy >= 0, "dup");
    // SAFETY: reading the flags of the descriptor just created.
    let flags = unsafe { libc::fcntl(copy, libc::F_GETFD) };
    assert_eq!(
        flags & libc::FD_CLOEXEC,
        0,
        "dup(2) is specified to clear FD_CLOEXEC on the new descriptor"
    );
    let ambient = place_ambient(copy);
    assert_guard_closes(ambient, "duplicated descriptor");
    close_fd(ambient);
    close_fd(copy);
    drop(file);
    let _ = std::fs::remove_file(path);
}

/// The soft `RLIMIT_NOFILE` limit, or `None` if it is unlimited / not an fd.
fn soft_fd_limit() -> Option<u64> {
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
    (lim.rlim_cur != libc::RLIM_INFINITY).then_some(lim.rlim_cur)
}

/// `kern.maxfilesperproc` — the kernel's own per-process descriptor ceiling,
/// measured here independently of the crate's own reader. On the reference
/// machine (macOS 26.5.2, arm64) this is 61440 while the soft `RLIMIT_NOFILE`
/// is 1048576: the sysctl, not the rlimit, is where descriptors actually stop.
#[cfg(target_os = "macos")]
fn max_files_per_proc() -> Option<u64> {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    // SAFETY: read-only `sysctlbyname` into a stack integer; the name is a
    // NUL-terminated literal and the new-value pointer is null.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            std::ptr::from_mut(&mut value).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && value > 0).then(|| u64::try_from(value).unwrap())
}

/// The exclusive ceiling on descriptor numbers this process can hold: the value
/// the kernel itself enforces, with no cost cap folded in.
fn kernel_fd_ceiling() -> u64 {
    let rlimit = soft_fd_limit().unwrap_or(u64::MAX);
    #[cfg(target_os = "macos")]
    let ceiling = rlimit.min(max_files_per_proc().unwrap_or(u64::MAX));
    // Linux has no per-process sysctl ceiling; the soft limit is the ceiling.
    #[cfg(not(target_os = "macos"))]
    let ceiling = rlimit;
    ceiling
}

/// Where the high-descriptor test places its ambient descriptor: the kernel
/// ceiling, capped so a host with a 2^20 soft limit does not make `bash` walk a
/// million numbers (61439 on the reference macOS box — the true top there).
fn probe_fd_ceiling() -> u64 {
    kernel_fd_ceiling().min(65_536)
}

/// A descriptor placed as high in the table as the kernel will allow — the case
/// a truncated sweep bound would miss (agent-bridle#352). On the reference
/// macOS box this puts the descriptor at 61439, one below
/// `kern.maxfilesperproc`, the highest number `dup2` accepts there.
#[test]
fn a_descriptor_near_the_top_of_the_table_is_closed_in_the_confined_child() {
    let _guard = fd_lock();
    let (path, file) = temp_file("high");

    let target = RawFd::try_from(probe_fd_ceiling().saturating_sub(1)).expect("fd fits in c_int");
    // SAFETY: `dup2` onto a descriptor number proven free immediately below;
    // `dup2` clears `FD_CLOEXEC`, producing the ambient descriptor under test.
    unsafe {
        assert_eq!(
            libc::fcntl(target, libc::F_GETFD),
            -1,
            "fd {target} must be free for this test"
        );
        assert!(
            libc::dup2(file.as_raw_fd(), target) >= 0,
            "dup2 to {target}"
        );
    }

    assert_guard_closes(target, "descriptor at the top of the table");

    // SAFETY: closing the descriptor this test created.
    unsafe {
        libc::close(target);
    }
    drop(file);
    let _ = std::fs::remove_file(path);
}

/// The premise the sweep bound rests on, checked at runtime on the actual
/// platform: the kernel refuses to place a descriptor at or above the ceiling
/// the guard derives — by `dup2` *or* by `F_DUPFD` allocation — while accepting
/// one immediately below it. If that ever failed,
/// `max(allocation_limit, highest_open + 1)` would not cover the table and the
/// sweep would be unsound rather than merely slow.
///
/// On the reference macOS box the ceiling is `kern.maxfilesperproc` = 61440
/// (dup2 to 61439 succeeds, 61440 fails `EBADF`), *not* the soft
/// `RLIMIT_NOFILE` of 1048576 — which is why the guard mins the two.
#[test]
fn the_kernel_refuses_a_descriptor_at_or_above_the_derived_ceiling() {
    let _guard = fd_lock();
    let Ok(ceiling) = RawFd::try_from(kernel_fd_ceiling()) else {
        // No finite ceiling to probe. The guard treats an unbounded allocation
        // limit as UNKNOWN and refuses the spawn, which is the point.
        return;
    };

    let file = std::fs::File::open("/dev/null").expect("open /dev/null");
    // SAFETY: `dup2`/`fcntl` on a descriptor this test owns, targeting numbers
    // at and just below the kernel ceiling; the accepted one is closed again.
    unsafe {
        let rc = libc::dup2(file.as_raw_fd(), ceiling);
        assert_eq!(
            rc, -1,
            "dup2 placed a descriptor at the ceiling {ceiling} — the sweep bound premise is false"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF),
            "dup2 at or above the ceiling must fail with EBADF"
        );
        assert_eq!(
            libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, ceiling),
            -1,
            "F_DUPFD allocated at or above the ceiling {ceiling}"
        );
        // …and the number one below it IS reachable, so the ceiling is exactly
        // where the guard believes it is (not merely somewhere above). Skipped
        // when the ceiling is enormous: materialising a descriptor table of 2^20
        // entries to prove it costs more than the assertion is worth.
        if ceiling <= 131_072 {
            let below = ceiling - 1;
            assert!(
                libc::dup2(file.as_raw_fd(), below) >= 0,
                "dup2 to {below}, one below the ceiling, must succeed"
            );
            libc::close(below);
        }
    }
}

/// stdio is *delegated*, not ambient: fds 0/1/2 must survive the guard, in both
/// directions. This is the intentional-inheritance half of the invariant.
#[test]
fn delegated_stdio_survives_the_confined_spawn() {
    // The shell here is only an echo: it round-trips bytes through fds 0/1/2.
    // It is never asked whether a descriptor exists — that question goes to
    // `fd_probe_helper`, for the reasons in the module docs.
    let mut cmd = Command::new("/bin/bash");
    cmd.arg("-c")
        .arg("read line; echo out:$line; echo err:$line >&2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    agent_bridle_fdguard::deny_inherited_fds(&mut cmd);
    let mut child = cmd.spawn().expect("spawn guarded child");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(b"delegated\n")
        .expect("write to the child's delegated stdin");
    let out = child.wait_with_output().expect("child output");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "out:delegated",
        "the child's delegated stdin and stdout must survive the guard"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "err:delegated",
        "the child's delegated stderr must survive the guard"
    );
}

/// Descriptors created by *another thread* while the spawn transition is being
/// prepared must not reach the child either. The churn thread duplicates an
/// ambient descriptor in a tight loop (`dup(2)` clears `FD_CLOEXEC`, so every
/// copy is inheritable) across many guarded spawns, and the child reports on
/// the whole band of descriptor numbers the churn can reach.
#[test]
fn concurrent_descriptor_creation_never_reaches_the_confined_child() {
    let _guard = fd_lock();
    let (path, file) = temp_file("churn");
    make_inheritable(file.as_raw_fd());
    let held = place_ambient(file.as_raw_fd());
    // The churn allocates at the lowest free numbers, so probe the whole band
    // from the first ambient number up past the descriptor this test holds.
    let targets: Vec<RawFd> = (3..=held + 4).collect();
    let raw = file.as_raw_fd();

    let stop = Arc::new(AtomicBool::new(false));
    let churn = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // SAFETY: duplicating a descriptor this test owns and closing
                // only the duplicate it just created.
                unsafe {
                    let copy = libc::dup(raw);
                    if copy >= 0 {
                        std::thread::yield_now();
                        libc::close(copy);
                    }
                }
            }
        })
    };

    // Positive control WHILE the churn runs: the held ambient descriptor alone
    // guarantees an inherited descriptor, so the control is deterministic.
    let control = probe_child(&targets, None, false);

    let mut leaks = Vec::new();
    for _ in 0..8 {
        let guarded = probe_child(&targets, None, true);
        let open: Vec<_> = guarded
            .iter()
            .filter(|(_, state)| state.is_some())
            .map(|(fd, state)| (*fd, *state))
            .collect();
        if !open.is_empty() {
            leaks.push(format!("{open:?}"));
        }
    }

    stop.store(true, Ordering::Relaxed);
    churn.join().expect("churn thread");
    close_fd(held);
    drop(file);
    let _ = std::fs::remove_file(path);

    assert!(
        matches!(control.get(&held), Some(Some(_))),
        "positive control: an unguarded child must inherit the ambient fd {held}: {control:?}"
    );
    assert!(
        leaks.is_empty(),
        "a descriptor reached the confined child while another thread churned the \
         descriptor table:\n{}",
        leaks.join("\n")
    );
}

/// The exec-failure reporting property, through the public API: a confined
/// spawn of a missing program must surface as `NotFound`, never a bogus `Ok`.
/// Marking (rather than closing) descriptors is what keeps std's exec-status
/// pipe alive to carry that error, on both legs.
#[test]
fn a_missing_confined_program_still_reports_not_found() {
    let mut cmd = Command::new("/nonexistent/agent-bridle-fdguard-hostile-xyzzy");
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    agent_bridle_fdguard::deny_inherited_fds(&mut cmd);
    let err = cmd
        .spawn()
        .expect_err("spawning a missing program must fail, not succeed");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}
