//! #2524 item 1: a signed OCAP `approve.toml` entry must reach the session's
//! recalled caveats — the same authority a spawned MCP child inherits via
//! `chat.rs`'s `startup_caveats` (`recalled_caveats` at the child-spawn seam).
//!
//! RED-FIRST evidence: `ocap_approval_does_not_reach_caveats_before_folding`
//! passes today and pins the GAP (an unfolded `ocap_policy` never widens
//! `recalled_caveats`, even though it was loaded and verified at session
//! start). `fold_ocap_approvals_widens_recalled_caveats_like_a_durable_grant`
//! failed to compile before `PermissionPromptState::fold_ocap_approvals`
//! existed (there was nothing to call) and is the fix's regression test.

use super::*;
use newt_core::caveats::{Caveats, Scope};
use newt_core::ocap_store::{
    build_store, verify_approves, Ed25519ApproveVerifier, PolicyFile, Verdict,
};

/// A verified `PolicySet` carrying one signed read-only `[[fs]]` approve for
/// `path`, signed by a disposable in-memory root key — never the real
/// `~/.newt` identity (mirrors `ocap_store::tests::signed_approve_store`).
fn verified_fs_read_policy(path: &str) -> newt_core::ocap_store::PolicySet {
    let key = agent_mesh_protocol::UserKey::generate();
    let mut file = PolicyFile::parse(&format!("[[fs]]\npath = \"{path}\"\n")).unwrap();
    let (signed, refused) = newt_core::ocap_store::sign_approves(
        &mut file,
        |_, _| false,
        |payload| key.sign(payload).to_bytes(),
    );
    assert_eq!(signed, 1);
    assert!(refused.is_empty());
    let (set, warnings) = build_store(&[(Verdict::Approve, Some(file.to_toml().unwrap()))]);
    assert!(warnings.is_empty(), "{warnings:?}");
    let verifier = Ed25519ApproveVerifier {
        verifying_key: key.public().as_bytes(),
    };
    let (set, warnings) = verify_approves(set, Some(&verifier));
    assert!(warnings.is_empty(), "{warnings:?}");
    set
}

fn fenced_base(workspace: &str) -> Caveats {
    Caveats {
        fs_read: Scope::only([workspace.to_string()]),
        fs_write: Scope::only([workspace.to_string()]),
        ..Caveats::top()
    }
}

/// RED (verify-first, #2524 item 1): loading a signed approve into
/// `ocap_policy` alone (as `chat.rs:1992-1998` does today) never reaches
/// `recalled_caveats` — the gate only consults `ocap_policy` at prompt time
/// (`evaluate_request`, `permissions.rs:733,1684`), not at caveat-recall time.
/// This passes BEFORE and AFTER the fix; it documents the exact shape of the
/// bug (an unfolded policy is inert for recall) rather than the fix.
#[test]
fn ocap_approval_does_not_reach_caveats_before_folding() {
    let mut state = PermissionPromptState {
        ocap_policy: verified_fs_read_policy("/outside/token"),
        ..Default::default()
    };
    let base = fenced_base("/ws");
    let caveats = state.recalled_caveats(&base, None);
    assert!(
        !newt_core::caveats::permits_path(&caveats.fs_read, "/outside/token"),
        "an ocap_policy that is never folded must not widen recall"
    );
    // Folding is what closes the gap (proved by the sibling test below); this
    // guard only pins that `ocap_policy` alone (pre-fold) does nothing.
    state.fold_ocap_approvals();
    assert!(newt_core::caveats::permits_path(
        &state.recalled_caveats(&base, None).fs_read,
        "/outside/token"
    ));
}

/// GREEN: after `fold_ocap_approvals`, the verified grant behaves exactly
/// like a v1 `/permissions`-promoted durable grant — attenuate-only against
/// the caller's ceiling, and it reaches a fenced (`Scope::Only`) axis without
/// ever turning an already-open axis into something narrower.
#[test]
fn fold_ocap_approvals_widens_recalled_caveats_like_a_durable_grant() {
    let mut state = PermissionPromptState {
        ocap_policy: verified_fs_read_policy("/outside/token"),
        ..Default::default()
    };
    state.fold_ocap_approvals();
    let base = fenced_base("/ws");
    let widened = state.recalled_caveats(&base, None);
    assert!(newt_core::caveats::permits_path(
        &widened.fs_read,
        "/outside/token"
    ));
    // Read-only: a write approve was never granted, so fs_write is untouched.
    assert!(!newt_core::caveats::permits_path(
        &widened.fs_write,
        "/outside/token"
    ));
    // Attenuate-only: the ceiling still wins over a folded grant.
    let ceiling = fenced_base("/ws");
    let capped = state.recalled_caveats(&base, Some(&ceiling));
    assert!(!newt_core::caveats::permits_path(
        &capped.fs_read,
        "/outside/token"
    ));
}

/// An unsigned approve entry never reaches `ocap_policy` in the first place
/// (`ocap_store::verify_approves` drops it loudly at load) — folding a store
/// that was loaded with no verifier folds nothing, so an unsigned grant can
/// never widen the session even via this new seam.
#[test]
fn folding_never_resurrects_an_unverified_approve() {
    let file = PolicyFile::parse("[[fs]]\npath = \"/outside/token\"\n").unwrap();
    let (set, _) = build_store(&[(Verdict::Approve, Some(file.to_toml().unwrap()))]);
    // No verifier available this session (no root key) — the production path
    // `ocap_store::load_store` calls `verify_approves` unconditionally.
    let (set, warnings) = verify_approves(set, None);
    assert!(!warnings.is_empty(), "an unsigned approve must warn loudly");
    let mut state = PermissionPromptState {
        ocap_policy: set,
        ..Default::default()
    };
    state.fold_ocap_approvals();
    let widened = state.recalled_caveats(&fenced_base("/ws"), None);
    assert!(!newt_core::caveats::permits_path(
        &widened.fs_read,
        "/outside/token"
    ));
}
