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
    // gpu-runner) this runs for real.
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
    // The escapee records its own pid before anything else, and the parent does
    // not exit until that pid is on disk, so after `run()` the test holds the
    // exact process the cgroup kill had to reach.
    let pidfile = ws.path().join("escapee-pid");
    let pidfile_s = pidfile.to_string_lossy().into_owned();
    let script = format!(
        "setsid sh -c 'echo $$ > {pidfile_s}; sleep 3; : > {marker_s}' </dev/null >/dev/null 2>&1 & \
         until [ -s {pidfile_s} ]; do sleep 0.01; done; echo started"
    );
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

    // Event, not timer: wait for the escapee's pid to be gone (a zombie counts
    // as gone) under the shared hang guard; only then is the marker's absence a
    // fact. The former 5 s sleep cost 5 s per run and could pass a survivor
    // whose write had merely been delayed past the check.
    let pid = std::fs::read_to_string(&pidfile)
        .expect("the escapee wrote its pid before the parent exited")
        .trim()
        .to_owned();
    let alive = |pid: &str| {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .map(|status| !status.contains("State:\tZ"))
            .unwrap_or(false)
    };
    let deadline = std::time::Instant::now() + newt_core::test_guard::HANG_GUARD;
    while alive(&pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "escapee {pid} outlived the hang guard; the cgroup kill did not reach it"
        );
        std::thread::yield_now();
    }
    assert!(
        !marker.exists(),
        "a setsid-ESCAPED descendant survived the run and wrote {marker_s} — the cgroup subtree \
         kill did not contain the process tree (killpg alone cannot reach a setsid session)"
    );
}
