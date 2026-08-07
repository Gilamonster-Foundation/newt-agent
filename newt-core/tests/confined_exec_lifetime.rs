//! Blocker-4 real-resource proof for child-lifetime containment in
//! [`newt_core::confined_exec::ConstrainedExecutor`] (#1598).
//!
//! Real-resource tier (CLAUDE.md): the invariant IS the OS killing a real
//! process group — no mock stands in for `killpg`. These ground the mocked
//! `ExecRequest::timeout` builder unit test: they prove the executor actually
//! (a) SIGKILLs a hung child at its deadline instead of hanging the harness, and
//! (b) sweeps a background descendant the child left in its process group, so a
//! hostile child cannot outlive the run.
//!
//! Linux, `#[serial]` (real subprocess + fs contend under parallel load).
//! Written to pass in BOTH environments: where Landlock is available the child
//! runs confined; where it is not the `AgentInfluenced` spawn fails closed — in
//! which case there is nothing to time out, and the test is a no-op assertion.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::time::{Duration, Instant};

use newt_core::confined_exec::{
    workspace_confined_caveats, ConfinedOutput, ConstrainedExecutor, ExecOrigin, ExecRefused,
    ExecRequest,
};
use serial_test::serial;
use tempfile::tempdir;

/// Build a confined `sh -c <script>` request bounded by `timeout`.
fn confined_sh_bounded(ws: &Path, script: &str, timeout: Duration) -> ExecRequest {
    ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        "sh",
        ["-c", script],
        ws,
        workspace_confined_caveats(ws),
    )
    .env("PATH", "/usr/bin:/bin")
    .timeout(timeout)
}

/// `Ok` means the fence enforced and the child ran; `Err(ConfinementUnenforceable)`
/// means Landlock was absent and the spawn failed closed — both are correct, and
/// only the `Ok` branch has a child to contain.
fn run(req: &ExecRequest) -> Option<ConfinedOutput> {
    match ConstrainedExecutor::run(req) {
        Ok(out) => Some(out),
        Err(ExecRefused::ConfinementUnenforceable(_)) => None,
        Err(e) => panic!("unexpected confined-exec error: {e}"),
    }
}

#[test]
#[serial]
fn a_hung_child_is_killed_at_its_timeout() {
    let ws = tempdir().unwrap();
    // The child would sleep for a minute; the executor must kill it in ~1s.
    let req = confined_sh_bounded(ws.path(), "sleep 60", Duration::from_secs(1));
    let start = Instant::now();
    let Some(out) = run(&req) else {
        return; // no Landlock → failed closed; nothing ran to time out.
    };
    let elapsed = start.elapsed();
    assert!(
        out.timed_out,
        "a child that outran its deadline must be reported timed_out"
    );
    assert!(!out.success, "a timed-out run is never a success");
    assert!(
        elapsed < Duration::from_secs(15),
        "the hung child was not killed promptly: took {elapsed:?} (harness would hang)"
    );
}

#[test]
#[serial]
fn a_background_descendant_does_not_survive_the_run() {
    let ws = tempdir().unwrap();
    let marker = ws.path().join("descendant-ran");
    let marker_str = marker.to_string_lossy().into_owned();
    // The parent exits immediately (echo), leaving a background descendant IN ITS
    // PROCESS GROUP that would create the marker after 3s. The executor's
    // process-group sweep must kill it first, so the marker never appears.
    let script = format!("(sleep 3 && : > '{marker_str}') & echo started");
    let req = confined_sh_bounded(ws.path(), &script, Duration::from_secs(20));
    let Some(out) = run(&req) else {
        return; // no Landlock → failed closed; no descendant was created.
    };
    assert!(out.success, "the parent itself should have exited cleanly");
    // Give the descendant well past its 3s to fire IF it survived the sweep.
    std::thread::sleep(Duration::from_secs(5));
    assert!(
        !marker.exists(),
        "a background descendant survived the run and created {marker_str} — the process-group \
         cleanup did not contain the child's tree"
    );
}
