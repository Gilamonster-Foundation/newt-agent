//! Regression: the **Autonomous** posture — the fourth corner of the ADR 0018
//! two-axis lattice (unbridled × human-gate-off, D10).
//!
//! In a **dedicated test binary** because it flips BOTH process-global markers
//! (`set_unbridled` + `set_human_gate(HumanGate::None)`), which a `OnceLock` cannot
//! un-set — so it must not share a process with the Supervised-free proofs in
//! `unbridle.rs` (which assert the human gate stays `passkey`).
//!
//! ## Where each ADR 0018 security invariant is guarded (the regression map)
//!
//! 1. **Fail-closed by omission (all three tokens)** — `agent-bridle-mcp`
//!    `caveats_source.rs`: `default_is_fail_closed_deny_all`,
//!    `unbridle_without_matching_ack_fails_closed` (token matrix
//!    None/""/"1"/"true"/stale → DENY-ALL), `neither_key_alone_unbridles`,
//!    `missing_home_falls_through_to_fail_closed_default`.
//! 2. **Unbridle keeps L2 + step-up** — `unbridle.rs`
//!    `unbridled_still_owes_step_up_supervised_free` (step-up) +
//!    `unbridled_runs_native_discloses_and_still_gates_the_grant` (L2);
//!    the L2-under-Autonomous half is proven **here**.
//! 3. **Honesty parity** (`sandbox_kind=none`, never `kernel`, identical
//!    Linux/macOS) — `unbridle.rs` `unbridled_never_overclaims_kernel_on_any_os`.
//! 4. **R6 rejection** (`AGENT_BRIDLE_NO_STEPUP` while bridled → hard refusal) —
//!    `caveats_source.rs` `no_step_up_ack_while_bridled_is_rejected`; the legal
//!    `step_up=none` (no ceremony) — `step_up_absent_while_bridled_is_legal_no_ceremony`.
//! 5. **Config precedence** (file/env/API, both axes) — `agent-bridle-config`
//!    `precedence_is_defaults_then_file_then_env_then_api`,
//!    `both_mode_axes_settable_across_file_env_api_with_precedence`,
//!    `step_up_floor_none_is_the_legal_no_ceremony_case`.
//!
//! This file closes the one missing **integration** corner: Autonomous end-to-end
//! through the real `ShellTool` — the human gate is disclosed OFF, yet the L2
//! capability grant is STILL enforced.
#![cfg(feature = "shell")]

use agent_bridle_core::{
    set_human_gate, set_unbridled, Caveats, Gate, HumanGate, Scope, Tool, ToolContext,
};
use agent_bridle_tool_shell::ShellTool;

#[cfg(not(windows))]
const GRANTED_PROGRAM: &str = "echo";
#[cfg(windows)]
const GRANTED_PROGRAM: &str = "cmd";

#[cfg(not(windows))]
fn granted_args() -> serde_json::Value {
    serde_json::json!(["hi"])
}

#[cfg(windows)]
fn granted_args() -> serde_json::Value {
    serde_json::json!(["/c", "echo hi"])
}

fn ctx(granted: Caveats) -> ToolContext {
    Gate::new(0)
        .authorize(&ShellTool::new(), &granted)
        .expect("authorize")
}

/// ADR 0018 D10 — the **Autonomous** posture: unbridled AND the human step-up gate
/// removed (via the distinct second ack). Two things must hold end-to-end through
/// the real `ShellTool`:
///
/// 1. **Disclosure** — every envelope carries `unbridled=true` and
///    `human_gate="none"` (vs Supervised-free's `"passkey"`), on the run path AND
///    the denied path; the granted program runs native (`sandbox_kind=none`).
/// 2. **Capability independence** — dropping the *human* leash must NOT widen the
///    *capability* axis. The L2 OCAP grant is still enforced, so an out-of-scope
///    program is denied even in the loudest posture. (The two axes are orthogonal:
///    ADR 0018 D1/D9.)
#[tokio::test]
async fn autonomous_discloses_human_gate_off_but_keeps_the_l2_grant() {
    set_unbridled();
    set_human_gate(HumanGate::None); // the distinct second ack → Autonomous

    // A restricted grant: only the platform's echo route is permitted, even in
    // the loudest posture. Windows `echo` is a cmd builtin, not an executable.
    let granted = Caveats {
        exec: Scope::only([GRANTED_PROGRAM.to_string()]),
        ..Caveats::top()
    };

    // The granted program runs native and discloses the Autonomous posture.
    let out = ShellTool::new()
        .invoke(
            serde_json::json!({"program": GRANTED_PROGRAM, "args": granted_args()}),
            &ctx(granted.clone()),
        )
        .await
        .expect("invoke");
    assert_eq!(out["exit_code"], 0, "granted program runs: {out}");
    assert_eq!(out["stdout"].as_str().map(str::trim), Some("hi"), "{out}");
    assert_eq!(
        out["sandbox_kind"], "none",
        "unbridled ⇒ no OS sandbox: {out}"
    );
    assert_eq!(out["disclosure"]["unbridled"], true, "{out}");
    assert_eq!(
        out["disclosure"]["human_gate"], "none",
        "Autonomous must disclose the human gate OFF (vs Supervised-free 'passkey'): {out}"
    );

    // Capability independence: Autonomous drops the human leash but NOT the L2
    // grant — an out-of-scope exec is still denied even here.
    let denied = ShellTool::new()
        .invoke(
            serde_json::json!({"program": "rm", "args": ["-rf", "/tmp/nope"]}),
            &ctx(granted),
        )
        .await
        .expect("invoke");
    assert_eq!(
        denied["denied"], true,
        "Autonomous must keep the L2 grant — out-of-scope exec still denied: {denied}"
    );
    // The denied envelope discloses the Autonomous posture too (honest on refusal).
    assert_eq!(denied["disclosure"]["unbridled"], true, "{denied}");
    assert_eq!(denied["disclosure"]["human_gate"], "none", "{denied}");
}
