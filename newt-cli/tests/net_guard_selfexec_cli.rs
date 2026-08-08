//! Packaging proof: the released `newt` binary carries the child-side network
//! guard (`newt __net-guard`), so the confined executor's `NetGrant::DenyAll`
//! egress floor works from an installed artifact — not only from `cargo run`
//! with a sibling `newt-net-guard` helper.
//!
//! This is the regression test for the deployment gap that made the seccomp
//! egress floor fail closed in every installed layout: `release.yml` /
//! `cargo install` / the nfpm packages ship only `newt` (+ `newt-mcp-server`),
//! never the `newt-net-guard` helper the old sibling-file lookup required. The
//! self-exec path (`resolve_net_guard` → `current_exe __net-guard`) removes the
//! second binary, and this test spawns the ACTUAL `newt` binary to prove the
//! subcommand is wired in and enforces the floor.
//!
//! Real-resource (installs a seccomp filter in the spawned `newt` process),
//! Linux + `#[serial]`. It grounds the mocked `resolve_net_guard` unit logic in
//! `newt-core::confined_exec` against a real released-shape invocation.
#![cfg(target_os = "linux")]

use assert_cmd::Command;

/// `newt __net-guard --probe-egress` installs `no_new_privs` + the seccomp
/// socket()-family deny on the spawned `newt` process, then self-tests that
/// off-box sockets (AF_INET/AF_INET6/AF_PACKET) are kernel-denied while AF_UNIX
/// survives — exit 0 iff the whole floor holds. That the subcommand exists AND
/// installs the floor is the packaging guarantee: the guard rides in `newt`.
#[test]
fn newt_binary_carries_the_net_guard_and_the_floor_holds() {
    let assert = Command::cargo_bin("newt")
        .unwrap()
        .args(["__net-guard", "--probe-egress"])
        .assert();
    // Exit 0 = every off-box family denied with EACCES, AF_UNIX allowed. A
    // non-zero code would name the family the kernel failed to deny (see
    // `newt_core::netguard::probe_code`); anything but 0 is a real floor failure
    // on a kernel that should support unprivileged seccomp.
    assert.success();
}

/// The guard refuses a malformed invocation (no `--` / program) rather than
/// silently exec-ing something — fail-closed argument handling. Exit 2 is the
/// usage error; crucially it is NOT 0 (which would mean it ran a program).
#[test]
fn net_guard_rejects_a_malformed_invocation() {
    Command::cargo_bin("newt")
        .unwrap()
        .args(["__net-guard", "not-a-flag"])
        .assert()
        .failure()
        .code(2);
}
