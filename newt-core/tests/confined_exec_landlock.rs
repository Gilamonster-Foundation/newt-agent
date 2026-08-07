//! step-4.2 — real-resource adversarial proof for
//! [`newt_core::confined_exec::ConstrainedExecutor`] (P4/P5, targets #7–#10).
//!
//! Real-resource tier (see CLAUDE.md "Testing strategy"): the invariant IS the
//! Linux kernel's Landlock enforcement on a real child process — no mock can
//! stand in for a syscall the kernel refuses. These tests are the ground truth
//! that the confined executor actually contains a hostile child; they ground the
//! mocked `confined_exec` unit tests (which only prove the *fence* and the
//! *strength floor*, not that the kernel honours them).
//!
//! Linux-only, `#[serial]` (real subprocess + real fs contend under parallel
//! load — CLAUDE.md). Each test is written to pass in BOTH environments:
//!
//! - where Landlock IS available, a fence-escaping child is **denied** by the
//!   kernel (the command fails / the object is untouched), and a legitimate
//!   in-workspace child **succeeds** under `SandboxKind::Landlock`;
//! - where Landlock is NOT available, the `AgentInfluenced` spawn is **refused**
//!   (`ExecRefused::ConfinementUnenforceable`) — never run unconfined.
//!
//! Either branch proves the security property: an attacker-influenced child can
//! neither read nor write outside the workspace nor reach the network — by
//! confinement where the kernel can enforce it, by refusal where it cannot.

#![cfg(target_os = "linux")]

use std::path::Path;

use newt_core::confined_exec::{
    workspace_confined_caveats, ConfinedOutput, ConstrainedExecutor, ExecOrigin, ExecRefused,
    ExecRequest,
};
use serial_test::serial;
use tempfile::tempdir;

/// Run `script` through `sh -c`, confined to `ws` as attacker-influenced code.
fn confined_sh(ws: &Path, script: &str) -> Result<ConfinedOutput, ExecRefused> {
    let req = ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        "sh",
        ["-c", script],
        ws,
        workspace_confined_caveats(ws),
    )
    // `sh`/coreutils need HOME/PATH-independent resolution; grant the bare
    // minimum so the interpreter runs (the fence still governs what it may
    // touch). Nothing credential-bearing is granted.
    .env("PATH", "/usr/bin:/bin");
    ConstrainedExecutor::run(&req)
}

fn landlock_available() -> bool {
    agent_bridle::landlock_is_supported()
}

/// Positive control: a legitimate in-workspace write runs, under a real kernel
/// sandbox where one is available (else it fails closed — never unconfined).
#[test]
#[serial]
fn legitimate_in_workspace_write_runs_confined() {
    let ws = tempdir().unwrap();
    let r = confined_sh(ws.path(), "echo hello > inside.txt");
    if landlock_available() {
        let out = r.expect("a workspace-contained write must run under Landlock");
        assert!(out.success, "in-fence write should succeed: {out:?}");
        assert_eq!(
            std::fs::read_to_string(ws.path().join("inside.txt")).unwrap(),
            "hello\n"
        );
        assert_eq!(
            out.sandbox_kind,
            agent_bridle::SandboxKind::Landlock,
            "the child must actually be Landlock-confined, not advisory"
        );
    } else {
        // No kernel fs backend → the AgentInfluenced spawn is refused, not run.
        assert!(
            matches!(r, Err(ExecRefused::ConfinementUnenforceable(_))),
            "without Landlock the confined spawn must fail closed, got {r:?}"
        );
    }
}

/// #7/#8: a child cannot WRITE outside the workspace fence — the classic
/// hostile-build-check escape. Denied by Landlock, or the spawn is refused.
#[test]
#[serial]
fn hostile_child_cannot_write_outside_the_workspace() {
    let ws = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let target = outside.path().join("escaped.txt");
    let script = format!("echo pwned > '{}'", target.display());

    match confined_sh(ws.path(), &script) {
        Ok(out) => assert!(
            !out.success,
            "a write outside the fence must fail under confinement: {out:?}"
        ),
        Err(ExecRefused::ConfinementUnenforceable(_)) => {}
        Err(e) => panic!("unexpected refusal: {e}"),
    }
    assert!(
        !target.exists(),
        "no file may be written outside the workspace fence"
    );
}

/// #7: a child cannot READ outside the workspace fence — no exfiltration of a
/// secret placed outside the workspace. Denied by Landlock, or spawn refused.
#[test]
#[serial]
fn hostile_child_cannot_read_outside_the_workspace() {
    let ws = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "TOPSECRET-9f3a").unwrap();
    let script = format!("cat '{}'", secret.display());

    match confined_sh(ws.path(), &script) {
        Ok(out) => {
            let seen = String::from_utf8_lossy(&out.stdout);
            assert!(
                !seen.contains("TOPSECRET-9f3a"),
                "a read outside the fence must not surface the secret: {seen:?}"
            );
        }
        Err(ExecRefused::ConfinementUnenforceable(_)) => {}
        Err(e) => panic!("unexpected refusal: {e}"),
    }
}

/// #8: the child's environment is EMPTY plus only the explicit grants — an
/// ambient credential/authority switch set in the parent is NOT inherited.
#[test]
#[serial]
fn hostile_child_does_not_inherit_parent_credentials() {
    let ws = tempdir().unwrap();
    // Set a credential-shaped var in THIS process; the confined child must not
    // see it (ConfinedCommand starts env-empty; we granted only PATH).
    std::env::set_var("NEWT_TEST_SECRET_TOKEN", "leak-me-42");
    let r = confined_sh(
        ws.path(),
        "printf '%s' \"${NEWT_TEST_SECRET_TOKEN:-<unset>}\" > seen.txt",
    );
    std::env::remove_var("NEWT_TEST_SECRET_TOKEN");

    if landlock_available() {
        let out = r.expect("in-workspace write runs under Landlock");
        assert!(out.success, "{out:?}");
        let seen = std::fs::read_to_string(ws.path().join("seen.txt")).unwrap();
        assert_eq!(
            seen, "<unset>",
            "the child must NOT inherit the parent's credential env var"
        );
    } else {
        assert!(matches!(r, Err(ExecRefused::ConfinementUnenforceable(_))));
    }
}

/// #9: a child has NO network without an explicit grant — an empty `net` fence
/// becomes a kernel deny-all. Proven live where a connector + kernel net-ABI
/// are available; otherwise the structural fence + a refusal-or-confinement is
/// asserted (and the live-connect assertion is honestly skipped).
#[test]
#[serial]
fn hostile_child_cannot_open_a_network_connection() {
    let ws = tempdir().unwrap();
    // A local listener the child will try (and must fail) to reach — no real
    // internet needed; Landlock net governs even loopback TCP connect().
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let python = ["/usr/bin/python3", "/usr/local/bin/python3"]
        .into_iter()
        .find(|p| Path::new(p).exists());

    let Some(py) = python else {
        // No connector available: assert the structural fence still holds — the
        // spawn is confined under a real sandbox (or refused), never advisory.
        eprintln!(
            "confined_exec net test: no python3 connector on this host — \
             asserting the structural fence only (live-connect assertion skipped)"
        );
        let r = confined_sh(ws.path(), "true");
        if landlock_available() {
            let out = r.expect("confined run under Landlock");
            assert_eq!(out.sandbox_kind, agent_bridle::SandboxKind::Landlock);
        } else {
            assert!(matches!(r, Err(ExecRefused::ConfinementUnenforceable(_))));
        }
        return;
    };

    // The child tries to connect to the local listener; under an empty net fence
    // the kernel must deny the connect (nonzero exit), or the spawn is refused.
    let script = format!(
        "{py} -c 'import socket,sys; \
         s=socket.socket(); s.settimeout(2); \
         sys.exit(0 if s.connect_ex((\"127.0.0.1\",{}))==0 else 7)'",
        addr.port()
    );
    let req = ExecRequest::new(
        ExecOrigin::AgentInfluenced,
        "sh",
        ["-c", script.as_str()],
        ws.path(),
        workspace_confined_caveats(ws.path()),
    )
    .env("PATH", "/usr/bin:/bin");

    match ConstrainedExecutor::run(&req) {
        Ok(out) => assert!(
            !out.success,
            "an outbound connect must be denied under an empty net fence: {out:?}"
        ),
        Err(ExecRefused::ConfinementUnenforceable(_)) => {}
        Err(e) => panic!("unexpected refusal: {e}"),
    }
    drop(listener);
}
