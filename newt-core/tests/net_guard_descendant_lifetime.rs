//! Blocker/mandate §3 real-resource proof: a confined child's descendant that
//! ESCAPES the process group (`setsid` / double-fork) is still terminated,
//! because the confined child and its whole subtree live in a cgroup-v2 subtree
//! the executor kills with `cgroup.kill`. `killpg` alone cannot reach a `setsid`
//! session — so a surviving escapee would prove cgroup containment is absent.
//!
//! Linux, `#[serial]`. Where Landlock is unavailable the guarded spawn fails
//! closed (nothing runs); where cgroup-v2 delegation is unavailable the executor
//! keeps only the killpg fallback and this test would (correctly) be unable to
//! prove the stronger property — but on the reference host both are present.

#![cfg(target_os = "linux")]

use std::time::Duration;

use newt_core::confined_exec::{
    workspace_confined_caveats, ConstrainedExecutor, ExecOrigin, ExecRefused, ExecRequest, NetGrant,
};
use serial_test::serial;
use tempfile::tempdir;

const GUARD_BIN: &str = env!("CARGO_BIN_EXE_newt-net-guard");

#[test]
#[serial]
fn a_setsid_escaped_descendant_is_killed_by_the_cgroup() {
    // The stronger containment (killing a setsid/double-fork escape) requires a
    // delegated cgroup-v2 subtree. Where that primitive is unavailable — e.g. an
    // unprivileged container/pod (some CI runners) — the executor falls back to
    // killpg, which cannot reach a setsid session (the documented b1 residual), so
    // there is nothing to assert here. Skip rather than fail, like the Landlock
    // tests skip where Landlock is absent. On a host with delegation (bare-metal
    // gnuc) this runs for real.
    if !newt_core::confined_exec::cgroup_subtree_kill_available() {
        return;
    }

    let ws = tempdir().unwrap();
    let marker = ws.path().join("escapee-ran");
    let marker_s = marker.to_string_lossy().into_owned();

    // The child spawns, via `setsid`, a descendant in a NEW session (escaping the
    // process group) that would create the marker after 3s; the parent exits
    // immediately. `killpg` cannot reach the setsid session — only the cgroup
    // subtree kill can — so if the marker never appears, cgroup containment held.
    let script =
        format!("setsid sh -c 'sleep 3; : > {marker_s}' </dev/null >/dev/null 2>&1 & echo started");
    let req = ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        "sh",
        ["-c", &script],
        ws.path(),
        workspace_confined_caveats(ws.path()),
    )
    .env("PATH", "/usr/bin:/bin")
    .net_grant(NetGrant::DenyAll) // opt-in guard + cgroup subtree
    .net_guard_bin(GUARD_BIN);

    match ConstrainedExecutor::run(&req) {
        Ok(out) => assert!(
            out.success,
            "the parent should have exited cleanly (echo started)"
        ),
        Err(ExecRefused::ConfinementUnenforceable(_)) => return, // no Landlock → nothing ran
        Err(e) => panic!("confined run errored: {e}"),
    }

    // Wait well past the escapee's 3s timer. If it survived the run's cgroup.kill,
    // it will have created the marker.
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        !marker.exists(),
        "a setsid-ESCAPED descendant survived the run and wrote {marker_s} — the cgroup subtree \
         kill did not contain the process tree (killpg alone cannot reach a setsid session)"
    );
}
