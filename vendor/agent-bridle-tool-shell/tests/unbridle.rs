//! End-to-end proof of the ADR 0018 unbridle escape hatch (I12 / #151).
//!
//! In a **dedicated test binary** so flipping the process-global unbridle marker
//! (`agent_bridle_core::set_unbridled`) cannot leak into the confinement proofs in
//! `real_spawn.rs` or the mocked unit tests (a `OnceLock` can't be un-set).
#![cfg(feature = "shell")]

use std::sync::Arc;

use agent_bridle_core::{
    set_unbridled, AttestRequirement, CallRequest, Caveats, Challenge, Discharge,
    DischargeProvider, DischargeVerifier, Registry, Rule, Scope, SessionId, StepUpPolicy,
    ToolError,
};
use agent_bridle_tool_shell::ShellTool;

#[cfg(any(unix, all(target_os = "windows", feature = "windows-appcontainer")))]
use agent_bridle_core::{Gate, Tool, ToolContext};

#[cfg(any(unix, all(target_os = "windows", feature = "windows-appcontainer")))]
fn ctx(granted: Caveats) -> ToolContext {
    Gate::new(0)
        .authorize(&ShellTool::new(), &granted)
        .expect("authorize")
}

/// A step-up provider whose ceremony always fails (no authenticator) — enough to
/// prove the gesture is still *demanded* under unbridle, without any crypto.
struct FailingProvider;
impl DischargeProvider for FailingProvider {
    fn obtain(
        &self,
        _s: &SessionId,
        _r: &CallRequest,
        _req: &AttestRequirement,
        _g: u64,
        _n: &[u8; 32],
    ) -> Result<Discharge, String> {
        Err("test: no authenticator present".into())
    }
}
struct StubVerifier;
impl DischargeVerifier for StubVerifier {
    fn verify(&self, _d: &Discharge, _r: &AttestRequirement, _e: &Challenge) -> Result<(), String> {
        Ok(()) // never reached — the provider refuses first
    }
}

/// Unbridled: a granted program runs **natively** (no OS sandbox → `sandbox_kind`
/// None), every envelope discloses `unbridled`, and — crucially — the L2 OCAP gate
/// **still denies** an out-of-scope program (authority is kept; only the mechanism
/// is dropped, ADR 0018 D1).
///
/// Unix-only: it spawns the `echo` **binary** to prove the native run. Windows has
/// no standalone `echo` (it is a `cmd` builtin), so the AppContainer host proves the
/// same honesty via the denial path in [`unbridled_never_overclaims_kernel_appcontainer`].
#[cfg(unix)]
#[tokio::test]
async fn unbridled_runs_native_discloses_and_still_gates_the_grant() {
    set_unbridled(); // this binary is dedicated to the unbridled posture

    // A restricted grant: only `echo` is permitted.
    let granted = Caveats {
        exec: Scope::only(["echo".to_string()]),
        ..Caveats::top()
    };

    // The granted program runs (native), reports None, and discloses unbridled.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "echo", "args": ["hi"]}),
            &ctx(granted.clone()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["exit_code"], 0);
    assert_eq!(out["stdout"], "hi\n");
    assert_eq!(
        out["sandbox_kind"], "none",
        "unbridled ⇒ no OS sandbox: {out}"
    );
    assert_eq!(
        out["disclosure"]["unbridled"], true,
        "every envelope must disclose unbridled: {out}"
    );
    // Default posture is Supervised-free — the human gate stays on (R5/D11).
    assert_eq!(
        out["disclosure"]["human_gate"], "passkey",
        "unbridled without the no-step-up ack is Supervised-free (human gate on): {out}"
    );

    // The L2 OCAP gate still holds: an out-of-scope exec is denied even unbridled.
    let denied = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "rm", "args": ["-rf", "/tmp/nope"]}),
            &ctx(granted),
        )
        .await
        .expect("invoke");
    assert_eq!(
        denied["denied"], true,
        "unbridle keeps the L2 grant gate — out-of-scope exec must be denied: {denied}"
    );
    assert_eq!(
        denied["disclosure"]["unbridled"], true,
        "a denied envelope discloses unbridled too: {denied}"
    );
}

/// R9 (ADR 0018 D2 — **per-OS honesty parity**): an unbridled run drops the L3
/// mechanism on *every* backend, so the honesty report is identical across OSes —
/// `sandbox_kind` None and every restricted axis at advisory/interceptor, with **no
/// axis ever claiming `kernel`**. This is the crucial property: even on macOS, where
/// Seatbelt would otherwise kernel-confine `fs`/`exec`, unbridle must **not**
/// overclaim. Because both the Linux and the macOS CI jobs run `cargo test
/// --workspace --all-features`, this one test is the Unix cross-OS matrix assertion.
///
/// Unix-only: it spawns the `echo` binary. The **Windows/AppContainer leg** is
/// [`unbridled_never_overclaims_kernel_appcontainer`] below — same honesty guard,
/// run on the real AppContainer backend via the denial path (no Unix `echo`).
#[cfg(unix)]
#[tokio::test]
async fn unbridled_never_overclaims_kernel_on_any_os() {
    set_unbridled(); // dedicated, isolated marker for this binary

    // Restrict every OS-confinement axis so all four appear in the report.
    let granted = Caveats {
        fs_read: Scope::only(["/etc".to_string()]),
        fs_write: Scope::only(["/tmp/x".to_string()]),
        exec: Scope::only(["echo".to_string()]),
        net: Scope::only(["example.com".to_string()]),
        ..Caveats::top()
    };
    // Whether the advisory L2 gate permits or denies this specific call, the
    // envelope carries the coarse kind + the per-axis report + the disclosure —
    // which is what honesty parity is about (not command success).
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "echo", "args": ["hi"]}),
            &ctx(granted),
        )
        .await
        .expect("invoke");

    assert_eq!(
        out["sandbox_kind"], "none",
        "unbridle ⇒ no OS sandbox: {out}"
    );
    let e = &out["enforcement"];
    // The honest Noop grades — identical on Linux and macOS under unbridle.
    assert_eq!(e["fs_read"], "interceptor", "{out}");
    assert_eq!(e["fs_write"], "interceptor", "{out}");
    assert_eq!(e["exec"], "interceptor", "{out}");
    assert_eq!(e["net"], "advisory", "{out}");
    // The overclaim guard: not one axis may report kernel when unbridled.
    for axis in ["fs_read", "fs_write", "exec", "net"] {
        assert_ne!(
            e[axis], "kernel",
            "unbridle must never claim kernel on {axis} (any OS): {out}"
        );
    }
    assert_eq!(out["disclosure"]["unbridled"], true, "{out}");
}

/// R3 (ADR 0018 D8/D9 — *Supervised-free*): unbridle drops the machine leash but
/// **not** the human one. Dispatching the real `ShellTool` through a step-up
/// Registry while the process is unbridled STILL owes the demanded gesture — a
/// refusal is fail-closed. The two axes are independent (the registry step-up
/// path never consults the unbridle marker).
#[tokio::test]
async fn unbridled_still_owes_step_up_supervised_free() {
    set_unbridled(); // this binary runs unbridled (dedicated, isolated marker)

    let policy = StepUpPolicy::new(
        vec![Rule {
            selector: "shell".to_string(),
            requirement: AttestRequirement::passkey_recorded(),
        }],
        AttestRequirement::NONE,
    );
    let registry = Registry::builder()
        .tool(Arc::new(ShellTool::new()))
        .step_up(
            SessionId::new([5u8; 32]),
            policy,
            Arc::new(FailingProvider),
            Arc::new(StubVerifier),
        )
        .build();

    let granted = Caveats {
        exec: Scope::only(["echo".to_string()]),
        ..Caveats::top()
    };
    let grant = registry.mint_grant(granted);
    // Unbridled + step-up: the passkey is demanded, the provider refuses ⇒ the
    // shell never runs. Unbridle did not launder the human gate.
    let err = registry
        .dispatch(
            "shell",
            serde_json::json!({"program": "echo", "args": ["hi"]}),
            &grant,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ToolError::Denied { .. }),
        "unbridled must still owe the step-up gesture (Supervised-free): {err:?}"
    );
}

/// R9 **Windows/AppContainer leg** (ADR 0018 D2 — the delegated per-OS honesty
/// proof). The Unix legs run on the Linux/macOS CI jobs; this one runs only on a
/// Windows host with the AppContainer backend, where the overclaim risk is the
/// *largest*: a **bridled** AppContainer kernel-confines exec deny-all (#123), net
/// deny-all/loopback (#133), and a restricted fs axis (#51) — four axes that could
/// each report `kernel`. Unbridle drops the mechanism, so the honest report must be
/// `sandbox_kind = none` with **no axis claiming `kernel`**, even though the
/// AppContainer backend is genuinely available on this host.
///
/// Uses the **denial path** (exec deny-all ⇒ any program is out of scope ⇒ refused
/// before any spawn), so it needs no Windows `echo`; the refused envelope still
/// carries the coarse kind + per-axis report + disclosure — which is what honesty
/// parity is about (ADR 0018 R9; the macOS test's own rationale).
#[cfg(all(target_os = "windows", feature = "windows-appcontainer"))]
#[tokio::test]
async fn unbridled_never_overclaims_kernel_appcontainer() {
    use agent_bridle_core::{best_available_sandbox, SandboxKind, SandboxPolicy};

    set_unbridled(); // dedicated, isolated marker for this binary

    // Precondition: the AppContainer backend really IS available here, so the guard
    // is meaningful — a bridled run on this host would kernel-confine the axes below.
    assert_eq!(
        best_available_sandbox(&Arc::new(SandboxPolicy::default())).kind(),
        SandboxKind::AppContainer,
        "this leg must run on the real AppContainer backend"
    );

    // A grant a BRIDLED AppContainer would kernel-confine on every axis: exec
    // deny-all → Kernel (#123), net deny-all → Kernel (#133), fs restricted → Kernel
    // (#51). exec deny-all also means any program is out of scope, so the call is
    // refused before any spawn.
    let granted = Caveats {
        fs_read: Scope::only(["C:/etc".to_string()]),
        fs_write: Scope::only(["C:/tmp/x".to_string()]),
        exec: Scope::only([] as [String; 0]),
        net: Scope::only([] as [String; 0]),
        ..Caveats::top()
    };
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "whatever.exe", "args": []}),
            &ctx(granted),
        )
        .await
        .expect("invoke");

    assert_eq!(
        out["denied"], true,
        "exec deny-all ⇒ refused before any spawn: {out}"
    );
    assert_eq!(
        out["sandbox_kind"], "none",
        "unbridle ⇒ no OS sandbox even though AppContainer is available: {out}"
    );
    // The overclaim guard: not one axis may report kernel when unbridled, even
    // though a bridled AppContainer would kernel-confine each of these.
    let e = &out["enforcement"];
    for axis in ["fs_read", "fs_write", "exec", "net"] {
        assert_ne!(
            e[axis], "kernel",
            "unbridle must never claim kernel on {axis} — AppContainer would, bridled: {out}"
        );
    }
    assert_eq!(out["disclosure"]["unbridled"], true, "{out}");
}
