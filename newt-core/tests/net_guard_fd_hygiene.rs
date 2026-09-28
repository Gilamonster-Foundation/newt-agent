//! Blocker-4 (mandate §4) real-resource proof: a confined child cannot use a
//! file descriptor the parent left open. An inherited fd is a capability that
//! BYPASSES pathname confinement — Landlock governs `open`, not an already-open
//! description.
//!
//! The child reads DIRECTLY from the inherited fd (`cat <&N`), not via
//! `/proc/self/fd/N` (which would re-`open` and hit Landlock); that is the true
//! fd-capability bypass.
//!
//! **Updated for the agent-bridle 0.8 upgrade.** Pre-0.8, `newt-net-guard`
//! (`NetGrant::DenyAll`) was the ONLY thing that closed an inherited fd, so the
//! control (no guard, `NetGrant::Unrestricted`) proved the bypass was real by
//! showing the fd WAS readable there. As of agent-bridle 0.8,
//! `agent_bridle_fdguard::deny_inherited_fds` runs unconditionally in
//! `ConfinedCommand::spawn` (agent-bridle#319) — every confined spawn now
//! closes ambient descriptors, independent of `NetGrant`. So `Unrestricted`
//! through `ConstrainedExecutor` no longer demonstrates the bypass either; the
//! control has to step OUTSIDE `ConstrainedExecutor` entirely (a bare
//! `std::process::Command`, no agent-bridle at all) to show the fd really is
//! OS-level inheritable, and both in-fence `NetGrant` arms now close it — a
//! strict improvement over the pre-0.8 behavior this test used to pin.
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

/// Genuinely unconfined: a bare `std::process::Command`, no agent-bridle
/// anywhere in the path. Proves the OS really does inherit a non-CLOEXEC fd
/// across `exec` (the ground truth every other branch is measured against).
fn raw_unconfined_read(fd: i32) -> String {
    let script = format!("cat <&{fd} 2>/dev/null; echo END");
    let output = std::process::Command::new("sh")
        .args(["-c", &script])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("raw unconfined spawn must succeed");
    String::from_utf8_lossy(&output.stdout).into_owned()
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

    // GROUND TRUTH — no agent-bridle involved at all: the fd IS inherited and
    // readable, proving the OS-level bypass is real (and that Landlock alone,
    // even if present, does not stop it — this run has no sandbox whatsoever).
    let raw = raw_unconfined_read(fd);
    assert!(
        raw.contains(SECRET),
        "ground truth: a raw unconfined child should read the inherited fd (else the test \
         proves nothing) — got: {raw}"
    );

    // UNRESTRICTED through ConstrainedExecutor — agent-bridle 0.8's
    // `ConfinedCommand::spawn` now closes ambient descriptors unconditionally
    // (agent-bridle#319), so even the "no guard" net grant no longer leaks the
    // fd. Confinement-unenforceable (no Landlock) is a no-op pass, same as before.
    match read_inherited_fd(ws.path(), fd, NetGrant::Unrestricted) {
        Ok(out) => assert!(
            !out.contains(SECRET),
            "agent-bridle 0.8's base ConfinedCommand should close inherited fds even under \
             NetGrant::Unrestricted — fd hygiene regressed:\n{out}"
        ),
        Err(ExecRefused::ConfinementUnenforceable(_)) => {
            unsafe { libc::close(fd) };
            drop(f);
            return;
        }
        Err(e) => panic!("unrestricted run errored: {e}"),
    }

    // GUARDED — DenyAll routes through newt-net-guard, which ALSO closes
    // inherited fds (belt-and-suspenders with agent-bridle's own fdguard).
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
