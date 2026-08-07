//! Blocker-1 truth reconciliation for `p4-constrained-executor`.
//!
//! The register (`docs/security/ocap-deviations.md`) marks P4 CLOSED and the CI
//! spawn-inventory gate holds `agent-exec-todo-p4 == 0`, yet the runtime
//! `verify_constrained_executor()` used to hard-code `Absent` — so the three
//! sources of truth (register, gate, verifier) disagreed. These tests force them
//! to agree from executable evidence:
//!
//! - The **tie test** (unit tier, no real spawn) parses the committed
//!   `spawn-inventory.toml` and asserts (a) zero unmigrated attacker spawns and
//!   (b) the verifier's verdict matches this host's fence availability — so the
//!   gate and the verifier can never drift.
//! - The **env-clean test** (real-resource, `#[serial]`, Linux) grounds the
//!   EnvIsolation guarantee: a secret planted in THIS process's environment must
//!   never reach a child spawned through `ConstrainedExecutor` (the child is
//!   `env_clear`ed), OR — where the kernel fence is unavailable — the spawn fails
//!   closed, never an unconfined run.

use newt_core::confined_exec::kernel_fs_fence_available;
use newt_core::ocap::verify_constrained_executor;

/// The committed inventory, embedded at compile time (no runtime fs read).
const SPAWN_INVENTORY: &str = include_str!("../../docs/security/spawn-inventory.toml");

/// Sum the `count` of every entry classed `agent-exec-todo-p4` — the unmigrated
/// attacker-influenced spawns the P4 closure criterion must hold at 0.
fn unmigrated_attacker_spawn_count(toml_src: &str) -> i64 {
    let doc: toml::Value = toml::from_str(toml_src).expect("spawn-inventory.toml parses");
    let mut total = 0i64;
    // The data section is a flat table of `"path" = { count, class, note }`.
    for (_path, entry) in doc.as_table().expect("top-level table") {
        let Some(t) = entry.as_table() else { continue };
        if t.get("class").and_then(toml::Value::as_str) == Some("agent-exec-todo-p4") {
            total += t
                .get("count")
                .and_then(toml::Value::as_integer)
                .unwrap_or(0);
        }
    }
    total
}

#[test]
fn verifier_agrees_with_the_spawn_inventory_gate() {
    // (a) the gate: no unmigrated attacker-influenced spawn remains.
    assert_eq!(
        unmigrated_attacker_spawn_count(SPAWN_INVENTORY),
        0,
        "spawn-inventory.toml carries an unmigrated agent-exec-todo-p4 spawn — the P4 closure \
         criterion, and this verifier's Verified verdict, would be a lie"
    );
    // (b) the verifier tracks live enforcement: Verified exactly when the kernel
    // fence the executor requires is available; Absent (naming the deviation)
    // where it is not (the executor fails closed there rather than run unconfined).
    let v = verify_constrained_executor();
    if kernel_fs_fence_available() {
        assert!(
            v.is_verified(),
            "the gate is clean and the kernel fence is available, but the verifier is not \
             Verified — register/verifier drift (the exact contradiction this test guards)"
        );
        assert_eq!(v.deviation(), None);
    } else {
        assert!(
            !v.is_verified(),
            "no kernel fence here, so the executor fails closed — the verifier must not claim \
             Verified"
        );
        assert_eq!(v.deviation(), Some("p4-constrained-executor"));
    }
}

#[cfg(target_os = "linux")]
mod real_resource {
    use newt_core::confined_exec::{
        workspace_confined_caveats, ConstrainedExecutor, ExecOrigin, ExecRefused, ExecRequest,
    };
    use serial_test::serial;
    use tempfile::tempdir;

    /// Grounds `EnvIsolation`: `ConfinedCommand::spawn` `env_clear`s the child, so
    /// a secret present ONLY in newt's own environment must not appear in a
    /// confined child's environment. This is the executable proof behind the
    /// report's EnvIsolation = Enforced claim (it grounds the mocked
    /// `request_env_is_exactly_the_grants_nothing_inherited` unit test).
    #[test]
    #[serial]
    fn a_parent_only_secret_never_reaches_a_confined_child() {
        const CANARY_KEY: &str = "NEWT_OCAP_ENV_CANARY";
        const CANARY_VAL: &str = "s3cr3t-parent-only-do-not-leak-4242";
        // The secret lives ONLY in the parent (newt) process environment.
        std::env::set_var(CANARY_KEY, CANARY_VAL);

        let ws = tempdir().unwrap();
        // Dump the child's ENTIRE environment, confined as attacker-influenced.
        let req = ExecRequest::new(
            ExecOrigin::AgentInfluenced,
            "sh",
            ["-c", "env"],
            ws.path(),
            workspace_confined_caveats(ws.path()),
        )
        // The interpreter needs PATH to resolve `env`; nothing credential-bearing
        // is granted. This is the child's ENTIRE environment.
        .env("PATH", "/usr/bin:/bin");
        let result = ConstrainedExecutor::run(&req);
        std::env::remove_var(CANARY_KEY);

        match result {
            Ok(out) => {
                let dump = String::from_utf8_lossy(&out.stdout);
                assert!(
                    !dump.contains(CANARY_VAL),
                    "a confined child inherited a parent-only secret VALUE — EnvIsolation is a \
                     lie:\n{dump}"
                );
                assert!(
                    !dump.contains(CANARY_KEY),
                    "a confined child even saw the secret's NAME:\n{dump}"
                );
                // No PARENT variable is inherited. `sh` legitimately synthesizes
                // a few of its own (PWD/SHLVL/_), but nothing from newt's
                // environment crosses: HOME/USER/LOGNAME are present in the
                // parent yet must be absent in the env-cleared child.
                for leaked in ["HOME=", "USER=", "LOGNAME=", "MAIL="] {
                    assert!(
                        !dump.contains(leaked),
                        "the confined child inherited parent var {leaked:?} — not env-cleared:\n{dump}"
                    );
                }
            }
            Err(e) => {
                // Fence unavailable → fail-closed refusal is equally correct: it
                // never runs the child unconfined, so it cannot leak.
                assert!(
                    matches!(e, ExecRefused::ConfinementUnenforceable(_)),
                    "unexpected non-fail-closed error from the confined executor: {e}"
                );
            }
        }
    }
}
