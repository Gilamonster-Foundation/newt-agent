//! dec1-build-grant (F30): a lifecycle build's `Build` permission request must
//! be `[s]ession allow`-able (it was refused, forcing a prompt on every
//! build/test/check in a session — up to 7 times observed in one run), and one
//! session grant must cover the lifecycle build, routed `build_exec`, and
//! `run_command` lanes alike.

use super::danger::{DangerTable, DangerTier};
use super::permission_prompt_tests::scripted_gate;
use super::{
    permission_policy, session_grant_covers, Audience, PermissionPromptState, PromptChoice,
};
use newt_core::caveats::{CountBound, Scope};
use newt_core::{Caveats, CaveatsExt as _, DenialKind, PermissionGate as _, PermissionRequest};
use std::cell::Cell;
use std::rc::Rc;

fn build_request(workspace: &str) -> PermissionRequest {
    PermissionRequest {
        tool: "lifecycle".into(),
        kind: DenialKind::Build,
        target: workspace.into(),
        reason: "Run this resolved lifecycle command: cargo test".into(),
    }
}

/// Would fail before the fix: `Build` was hardcoded to `DangerTier::High`
/// and `permission_policy` only offers `[s]ession allow` for `Low` tier — the
/// form never listed the action at all.
#[test]
fn permission_policy_offers_session_allow_for_build() {
    let danger = DangerTable::builtin().with_fs_root("/ws");
    let req = build_request("/ws");
    assert_eq!(danger.classify(req.kind, &req.target), DangerTier::High);

    let (actions, _note) = permission_policy(&req, &danger, Audience::Terminal);
    assert!(
        actions
            .iter()
            .any(|a| a.action == PromptChoice::AllowSession),
        "Build must offer session allow despite its High blast-radius tier"
    );
}

/// Would fail before the fix: `session_grant_covers` only matched the exact
/// `(kind, target)` pair, so a `run_command`/routed `cargo test` — kind
/// `Exec`, target `"cargo"` — still prompted even after the operator already
/// granted the workspace's `Build` authority for the session.
#[test]
fn a_session_build_grant_covers_exec_of_the_same_build_tool() {
    let mut grants = std::collections::BTreeSet::new();
    grants.insert((DenialKind::Build, "/ws".to_string()));

    let cargo_test = PermissionRequest {
        tool: "run_command".into(),
        kind: DenialKind::Exec,
        target: "cargo".into(),
        reason: "cargo test".into(),
    };
    assert!(
        session_grant_covers(&grants, &cargo_test),
        "a session Build grant must cover exec of the workspace's build tool"
    );

    // The reverse direction stays refused: an `exec:cargo` grant alone must
    // NOT silently satisfy a `Build` request (lifecycle_build_request's
    // explicit design invariant — a shell grant never acquires build
    // authority).
    let mut exec_only = std::collections::BTreeSet::new();
    exec_only.insert((DenialKind::Exec, "cargo".to_string()));
    let build = build_request("/ws");
    assert!(
        !session_grant_covers(&exec_only, &build),
        "an exec:cargo grant must never silently satisfy a Build request"
    );

    // An unrelated command is still not covered.
    let rm = PermissionRequest {
        tool: "run_command".into(),
        kind: DenialKind::Exec,
        target: "rm".into(),
        reason: "cleanup".into(),
    };
    assert!(!session_grant_covers(&grants, &rm));
}

/// Architect review round 1, #1: `session_grant_covers` decides WHETHER a
/// Build-covered exec is allowed, but not what fence it runs under. Would
/// fail before the fix: the covered exec ran under whatever the shell lane's
/// CURRENT `baseline` allowed (here, a net host a prior, unrelated grant
/// added), not the calibrated `build_tool_caveats` fence — `widen_caveats`
/// only adds to `baseline`, it never narrows a wider net/exec/write axis
/// back down. A session Build grant must clamp the covered exec to the build
/// fence regardless of what else the shell lane has been granted.
#[test]
fn a_build_covered_exec_runs_under_the_build_fence_not_a_wider_shell_baseline() {
    let ws = "/ws";
    let mut state = PermissionPromptState::default();
    // The operator already granted Build authority for this workspace.
    state
        .session_grants
        .insert((DenialKind::Build, ws.to_string()));

    // The shell lane's CURRENT baseline is wider than the build fence: it
    // carries a net grant some earlier, unrelated permission decision added.
    let wide_baseline = Caveats {
        fs_read: Scope::only([ws.to_string()]),
        fs_write: Scope::only([ws.to_string()]),
        exec: Scope::only(["cargo".to_string()]),
        net: Scope::only(["crates.io".to_string()]),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    };

    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        wide_baseline.clone(),
        None,
        None,
        vec![],
        prompts.clone(),
    );
    let cargo_test = PermissionRequest {
        tool: "run_command".into(),
        kind: DenialKind::Exec,
        target: "cargo".into(),
        reason: "cargo test".into(),
    };
    let decision = gate.ask_with_caveats(&wide_baseline, std::slice::from_ref(&cargo_test));
    let newt_core::PermissionDecision::Allow(caveats) = decision else {
        panic!(
            "a session Build grant must cover exec of the workspace's build tool without prompting"
        );
    };
    assert!(
        !caveats.permits_net("crates.io"),
        "a Build-covered exec must run under the calibrated build fence \
         (net denied), never the shell lane's wider current baseline"
    );
    assert_eq!(
        prompts.get(),
        0,
        "covered by the session Build grant — no prompt expected"
    );
}
