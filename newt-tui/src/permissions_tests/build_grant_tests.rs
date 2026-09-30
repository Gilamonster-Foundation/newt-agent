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
        harness_bound: false,
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
        harness_bound: false,
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
        harness_bound: false,
    };
    assert!(!session_grant_covers(&grants, &rm));
}

/// Build-covered execution keeps the calibrated filesystem fence and the
/// invocation's existing network authority. Recalled grants cannot restore
/// attenuated networking, and an explicit Plan ceiling still wins.
#[test]
fn a_build_covered_exec_keeps_network_authority_and_the_build_fence() {
    let ws = "/ws";
    let cargo_test = PermissionRequest {
        tool: "run_command".into(),
        kind: DenialKind::Exec,
        target: "cargo".into(),
        reason: "cargo test".into(),
        harness_bound: false,
    };
    for network in [Scope::none(), Scope::only(["crates.io".into()]), Scope::All] {
        let mut state = PermissionPromptState::default();
        state.session_grants.extend([
            (DenialKind::Build, ws.to_string()),
            (DenialKind::Net, "unrelated.example".to_string()),
        ]);
        let baseline = Caveats {
            fs_read: Scope::All,
            fs_write: Scope::All,
            exec: Scope::none(),
            net: network.clone(),
            max_calls: CountBound::AtMost(7),
            valid_for_generation: Scope::All,
        };
        let prompts = Rc::new(Cell::new(0));
        let mut gate = scripted_gate(
            &mut state,
            Caveats::top(),
            None,
            None,
            vec![],
            prompts.clone(),
        );
        for plan_active in [false, true] {
            gate.preset_clamp = plan_active.then(newt_core::agentic::plan_phase_clamp);
            let decision = gate.ask_with_caveats(&baseline, std::slice::from_ref(&cargo_test));
            let newt_core::PermissionDecision::Allow(caveats) = decision else {
                panic!("existing Build grant must not require another prompt");
            };
            assert_eq!(
                caveats.net,
                if plan_active {
                    Scope::none()
                } else {
                    network.clone()
                },
                "build approval must preserve invocation attenuation and Plan ceilings"
            );
            assert_eq!(caveats.permits_exec("cargo"), !plan_active);
            assert_eq!(caveats.permits_fs_write(ws), !plan_active);
            assert!(!caveats.permits_fs_write("/outside"));
            assert!(!caveats.permits_fs_read("/outside"));
            assert_eq!(caveats.max_calls, baseline.max_calls);
        }
        assert_eq!(prompts.get(), 0, "the existing Build grant is reused");
    }
}

/// Architect review round 2 (Reviewer FIX-FIRST, PR #2579, BLOCKER, red test
/// (a)): an exec grant for a build tool must never widen the SESSION's
/// `fs_read` — that caveats value is what `read_file` and every other tool
/// call checks for the rest of the session, not just the confined child the
/// grant was asked for. Would fail before the fix: an earlier version of
/// `widen_caveats` added the toolchain read roots to `fs_read` directly, so
/// `recalled_caveats` (and its headless twin, the ocap/durable-grant fold)
/// would have let `read_file` read `$CARGO_HOME/credentials.toml` after
/// nothing more than a session `exec:cargo` grant.
#[test]
fn an_exec_grant_never_widens_recalled_fs_read_via_either_grant_store() {
    let ws = "/ws";
    let base = Caveats {
        fs_read: Scope::only([ws.to_string()]),
        fs_write: Scope::only([ws.to_string()]),
        exec: Scope::none(),
        net: Scope::none(),
        max_calls: CountBound::Unlimited,
        valid_for_generation: Scope::All,
    };
    let credentials = "/home/op/.cargo/credentials.toml";

    // The interactive session-grant store.
    let mut state = PermissionPromptState::default();
    state
        .session_grants
        .insert((DenialKind::Exec, "cargo".to_string()));
    let widened = state.recalled_caveats(&base, None);
    assert!(widened.permits_exec("cargo"));
    assert!(
        !widened.permits_fs_read(credentials),
        "a session exec:cargo grant must never widen fs_read to cargo's \
         toolchain home — read_file must not gain cargo-credential access \
         from an exec grant"
    );

    // The headless durable/ocap-approved fold (`fold_ocap_approvals` extends
    // this same `durable_grants` set) — same widen, different grant store.
    let mut durable_state = PermissionPromptState::default();
    durable_state
        .durable_grants
        .insert((DenialKind::Exec, "cargo".to_string()));
    let widened_durable = durable_state.recalled_caveats(&base, None);
    assert!(
        !widened_durable.permits_fs_read(credentials),
        "a durable/ocap-approved exec:cargo grant must never widen fs_read either"
    );
}

/// Shawn's call (round 2, item 3): session `Build` is offered on the web
/// surface too, same as terminal — pinned deliberately so a later change to
/// `permission_policy`'s audience handling doesn't silently drop it there.
#[test]
fn permission_policy_offers_session_allow_for_build_on_the_web_surface_too() {
    let danger = DangerTable::builtin().with_fs_root("/ws");
    let req = build_request("/ws");

    let (terminal_actions, _) = permission_policy(&req, &danger, Audience::Terminal);
    let (web_actions, _) = permission_policy(&req, &danger, Audience::Web);
    for (audience, actions) in [("terminal", &terminal_actions), ("web", &web_actions)] {
        assert!(
            actions
                .iter()
                .any(|a| a.action == PromptChoice::AllowSession),
            "{audience} must offer session allow for Build"
        );
    }
}

fn fetch_request(host: &str) -> PermissionRequest {
    PermissionRequest {
        tool: "lifecycle".into(),
        kind: DenialKind::Net,
        target: host.into(),
        reason: "Fetch the crates Cargo.lock pins: `cargo fetch --locked`".into(),
        harness_bound: false,
    }
}

/// The dependency-fetch prompt (#2595) describes a one-shot `cargo fetch`.
/// Before the fix it offered session/permanent allow, and a recorded grant
/// flowed into `recalled_caveats`, widening the SHELL WORKER's net to
/// crates.io for the rest of the session. It must offer allow-once/deny only.
#[test]
fn dependency_fetch_prompt_offers_no_session_or_permanent_allow() {
    let danger = DangerTable::builtin();
    for audience in [Audience::Terminal, Audience::Web] {
        let (actions, _) = permission_policy(&fetch_request("index.crates.io"), &danger, audience);
        for a in &actions {
            assert!(
                !matches!(
                    a.action,
                    PromptChoice::AllowSession | PromptChoice::AllowPermanent
                ),
                "fetch prompt must be one-shot, offered {:?}",
                a.label
            );
        }
        assert!(actions.iter().any(|a| a.action == PromptChoice::AllowOnce));
    }
    // An ordinary net request keeps its session scope.
    let (actions, _) = permission_policy(
        &PermissionRequest {
            tool: "run_command".into(),
            ..fetch_request("example.com")
        },
        &danger,
        Audience::Terminal,
    );
    assert!(actions
        .iter()
        .any(|a| a.action == PromptChoice::AllowSession));
}

/// Answering the fetch prompt with the strongest allow must not leave the
/// crates.io hosts in the caveats later tool dispatch (run_command) receives.
#[test]
fn answering_the_fetch_prompt_with_the_strongest_allow_records_no_net_grant() {
    let base = Caveats {
        net: Scope::only(Vec::<String>::new()),
        ..Caveats::top()
    };
    let mut state = PermissionPromptState::default();
    let mut gate = scripted_gate(
        &mut state,
        base.clone(),
        None,
        None,
        vec![PromptChoice::AllowPermanent, PromptChoice::AllowPermanent],
        Rc::new(Cell::new(0)),
    );
    let requests = [
        fetch_request("index.crates.io"),
        fetch_request("static.crates.io"),
    ];
    let _ = gate.ask(&requests);
    let after = state.recalled_caveats(&base, None);
    for host in ["index.crates.io", "static.crates.io"] {
        assert!(
            !after.permits_net(host),
            "{host} leaked into the worker's caveats"
        );
    }
}
