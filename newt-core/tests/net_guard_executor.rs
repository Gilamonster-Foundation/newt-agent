//! Blocker-2 slice-2 real-resource proof: the seccomp egress floor is applied to
//! LIVE children by the confined executor via the `newt-net-guard` wrapper.
//!
//! This exercises the whole production path — `ConstrainedExecutor::run` with
//! `NetGrant::DenyAll` spawns the guard under the Landlock fs fence, the guard
//! installs `no_new_privs` + seccomp, then `exec`s the requested program — and
//! proves egress is denied for the child AND its descendants (seccomp is
//! inherited across fork/exec), while a legitimate confined child still runs.
//!
//! Linux, `#[serial]`. Where Landlock is unavailable the `AgentInfluenced` spawn
//! fails closed (nothing runs unconfined); each test treats that as a pass.

#![cfg(target_os = "linux")]

use std::path::Path;

use newt_core::confined_exec::{
    build_tool_caveats, workspace_confined_caveats, ConfinedOutput, ConstrainedExecutor,
    ExecOrigin, ExecRefused, ExecRequest, NetGrant,
};
use serial_test::serial;
use tempfile::tempdir;

/// The Cargo-built guard binary (Cargo exports this for the crate's own bins).
const GUARD_BIN: &str = env!("CARGO_BIN_EXE_newt-net-guard");

/// A confined `sh -c <script>` under the full egress-deny floor.
fn deny_all_sh(ws: &Path, script: &str) -> ExecRequest {
    ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        "sh",
        ["-c", script],
        ws,
        workspace_confined_caveats(ws),
    )
    .env("PATH", "/usr/bin:/bin")
    .net_grant(NetGrant::DenyAll)
    .net_guard_bin(GUARD_BIN)
}

fn run(req: &ExecRequest) -> Option<ConfinedOutput> {
    match ConstrainedExecutor::run(req) {
        Ok(out) => Some(out),
        // No Landlock → fail-closed refusal is correct (nothing ran unconfined).
        Err(ExecRefused::ConfinementUnenforceable(_)) => None,
        Err(e) => panic!("unexpected confined-exec error: {e}"),
    }
}

#[test]
#[serial]
fn guard_denies_egress_for_the_live_child_via_the_executor() {
    // The guard's own probe, run THROUGH the executor: exit 0 iff seccomp denied
    // TCP/UDP/raw while AF_UNIX survived — proving the floor reached the child.
    let ws = tempdir().unwrap();
    let req = ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        GUARD_BIN,
        ["--probe-egress"],
        ws.path(),
        workspace_confined_caveats(ws.path()),
    )
    .net_grant(NetGrant::DenyAll)
    .net_guard_bin(GUARD_BIN);
    let Some(out) = run(&req) else { return };
    assert!(
        out.success,
        "the guarded child's egress probe failed (code {:?}): the seccomp floor did not reach \
         the live child. stderr: {}",
        out.code,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[serial]
fn a_nested_shell_and_descendant_cannot_open_a_socket() {
    // A NESTED sh spawns a descendant that runs the guard's probe. seccomp is
    // inherited across every fork/exec, so the descendant's off-box sockets are
    // denied too — proving egress denial is not just for the direct child.
    let ws = tempdir().unwrap();
    let script = format!("sh -c '{GUARD_BIN} --probe-egress'; echo nested_exit=$?");
    let req = deny_all_sh(ws.path(), &script);
    let Some(out) = run(&req) else { return };
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("nested_exit=0"),
        "a nested-shell descendant's egress was NOT denied (want nested_exit=0):\n{stdout}\n\
         stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[serial]
fn build_tool_caveats_under_deny_all_deny_a_build_steps_egress() {
    // The exact contract run_build_check + crew now apply (b1 slice 1b):
    // build_tool_caveats (net: none) + NetGrant::DenyAll. Run the guard's egress
    // probe under that contract — exit 0 iff seccomp denied TCP/UDP/raw while
    // AF_UNIX survived, proving a hostile `build.rs` / test cannot resolve a name
    // or exfiltrate over UDP (the leg the net:none Landlock TCP-deny alone misses).
    let ws = tempdir().unwrap();
    let req = ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        GUARD_BIN,
        ["--probe-egress"],
        ws.path(),
        build_tool_caveats(ws.path()),
    )
    .env("PATH", "/usr/bin:/bin")
    .net_grant(NetGrant::DenyAll)
    .net_guard_bin(GUARD_BIN);
    let Some(out) = run(&req) else { return };
    assert!(
        out.success,
        "a build step under build_tool_caveats + DenyAll could still reach the network \
         (probe code {:?}): the seccomp floor did not reach the build child. stderr: {}",
        out.code,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[serial]
fn a_legitimate_in_workspace_command_still_runs_under_the_floor() {
    // Positive control: the egress floor must not break ordinary confined work.
    let ws = tempdir().unwrap();
    let req = deny_all_sh(ws.path(), "echo hello-from-guarded-child");
    let Some(out) = run(&req) else { return };
    assert!(out.success, "a legitimate guarded command failed to run");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("hello-from-guarded-child"),
        "guarded child did not produce expected output"
    );
}

#[test]
#[serial]
fn deny_all_refuses_when_the_guard_binary_is_missing() {
    // Fail-closed: if the egress floor cannot be established (no guard binary),
    // the executor refuses rather than running with a weaker net floor.
    let ws = tempdir().unwrap();
    let req = ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        "sh",
        ["-c", "true"],
        ws.path(),
        workspace_confined_caveats(ws.path()),
    )
    .env("PATH", "/usr/bin:/bin")
    .net_grant(NetGrant::DenyAll)
    .net_guard_bin("/nonexistent/newt-net-guard");
    match ConstrainedExecutor::run(&req) {
        Err(ExecRefused::ConfinementUnenforceable(_)) => {}
        other => panic!("expected fail-closed refusal when the guard is missing, got {other:?}"),
    }
}
