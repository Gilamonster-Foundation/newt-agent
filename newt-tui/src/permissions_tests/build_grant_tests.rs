//! dec1-build-grant (F30): a lifecycle build's `Build` permission request must
//! be `[s]ession allow`-able (it was refused, forcing a prompt on every
//! build/test/check in a session — up to 7 times observed in one run), and one
//! session grant must cover the lifecycle build, routed `build_exec`, and
//! `run_command` lanes alike.

use super::danger::{DangerTable, DangerTier};
use super::{permission_policy, session_grant_covers, Audience, PromptChoice};
use newt_core::{DenialKind, PermissionRequest};

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
