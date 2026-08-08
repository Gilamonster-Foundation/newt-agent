//! Blocker-4 (mandate §4) real-resource proof: a confined child cannot use a
//! file descriptor the parent left open. An inherited fd is a capability that
//! BYPASSES pathname confinement — Landlock governs `open`, not an already-open
//! description — so `newt-net-guard` closes every inherited fd `>= 3` before exec.
//!
//! The child reads DIRECTLY from the inherited fd (`cat <&N`), not via
//! `/proc/self/fd/N` (which would re-`open` and hit Landlock); that is the true
//! fd-capability bypass. The control (no guard) proves the fd really is inherited
//! and readable, so the guarded case's denial is the fd closure at work.
//!
//! Linux, `#[serial]`. Where Landlock is unavailable the guarded spawn fails
//! closed (nothing runs) and the test is a no-op pass.

#![cfg(target_os = "linux")]

use std::os::unix::io::AsRawFd;
use std::path::Path;

use newt_core::confined_exec::{
    workspace_confined_caveats, ConstrainedExecutor, ExecOrigin, ExecRefused, ExecRequest, NetGrant,
};
use serial_test::serial;
use tempfile::tempdir;

const GUARD_BIN: &str = env!("CARGO_BIN_EXE_newt-net-guard");
const SECRET: &str = "FD-SENTINEL-SECRET-9973";

/// Open an out-of-workspace sentinel and return a NON-CLOEXEC (inheritable) dup
/// of its fd, plus the `File` (kept alive to hold the open description).
fn inheritable_sentinel_fd() -> (tempfile::TempDir, std::fs::File, i32) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sentinel");
    std::fs::write(&path, SECRET).unwrap();
    let f = std::fs::File::open(&path).unwrap(); // std sets CLOEXEC on this one
                                                 // `dup` produces a fd WITHOUT CLOEXEC → inherited across fork/exec.
    let fd = unsafe { libc::dup(f.as_raw_fd()) };
    assert!(fd >= 3, "dup did not yield an fd >= 3");
    (dir, f, fd)
}

fn read_inherited_fd(ws: &Path, fd: i32, net: NetGrant) -> Result<String, ExecRefused> {
    // Read straight from the inherited fd (bypasses Landlock's open-time checks).
    let script = format!("cat <&{fd} 2>/dev/null; echo END");
    let req = ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        "sh",
        ["-c", &script],
        ws,
        workspace_confined_caveats(ws),
    )
    .env("PATH", "/usr/bin:/bin")
    .net_grant(net)
    .net_guard_bin(GUARD_BIN);
    ConstrainedExecutor::run(&req).map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

#[test]
#[serial]
fn a_confined_guarded_child_cannot_read_an_inherited_out_of_workspace_fd() {
    let (_sentinel, f, fd) = inheritable_sentinel_fd();
    let ws = tempdir().unwrap();

    // CONTROL — no guard (Unrestricted): the fd IS inherited and readable, which
    // proves the bypass is real (and that Landlock alone does not stop it).
    match read_inherited_fd(ws.path(), fd, NetGrant::Unrestricted) {
        Ok(out) => assert!(
            out.contains(SECRET),
            "control: the inherited fd should be readable without the guard (else the test \
             proves nothing) — got: {out}"
        ),
        Err(ExecRefused::ConfinementUnenforceable(_)) => {
            // No Landlock at all → both branches fail closed; nothing to prove.
            unsafe { libc::close(fd) };
            drop(f);
            return;
        }
        Err(e) => panic!("control run errored: {e}"),
    }

    // GUARDED — DenyAll routes through newt-net-guard, which closes inherited fds.
    let guarded = read_inherited_fd(ws.path(), fd, NetGrant::DenyAll);
    unsafe { libc::close(fd) };
    drop(f);
    match guarded {
        Ok(out) => assert!(
            !out.contains(SECRET),
            "a guarded confined child READ an inherited out-of-workspace fd — fd hygiene failed:\n{out}"
        ),
        Err(ExecRefused::ConfinementUnenforceable(_)) => {} // failed closed — also fine
        Err(e) => panic!("guarded run errored: {e}"),
    }
}
