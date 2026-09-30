//! Operator-resource protection at real permission gate boundaries.

use super::permission_prompt_tests::scripted_gate;
use super::*;
use newt_core::{
    Caveats, DenialKind, PermissionGate as _, PermissionRequest, Scope, ScopeExt as _,
};
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

struct Fixture {
    _temp: tempfile::TempDir,
    workspace: PathBuf,
    operator: PathBuf,
    key: PathBuf,
    guard: Arc<newt_core::workspace_protection::WorkspaceProtection>,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        let operator = root.join("operator");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&operator).unwrap();
        let key = operator.join("identity.pem");
        std::fs::write(&key, "private test fixture").unwrap();
        let guard = Arc::new(
            newt_core::workspace_protection::WorkspaceProtection::new(
                &[operator.join("profiles.age")],
                std::slice::from_ref(&key),
            )
            .unwrap(),
        );
        Self {
            _temp: temp,
            workspace,
            operator,
            key,
            guard,
        }
    }

    fn state(&self) -> PermissionPromptState {
        PermissionPromptState {
            protection: Some(self.guard.clone()),
            ..Default::default()
        }
    }

    fn caveats(&self) -> Caveats {
        Caveats {
            fs_read: Scope::only([self.workspace.to_string_lossy().into_owned()]),
            fs_write: Scope::only([self.workspace.to_string_lossy().into_owned()]),
            ..Caveats::top()
        }
    }
}

fn request(kind: DenialKind, target: &Path) -> PermissionRequest {
    PermissionRequest {
        tool: "request_permissions".into(),
        kind,
        target: target.to_string_lossy().into_owned(),
        reason: "fixture request".into(),
        harness_bound: false,
    }
}

#[test]
fn protected_batch_denies_before_prompt_consumption_or_persistence() {
    let fixture = Fixture::new();
    let mut state = fixture.state();
    let lawful = fixture.workspace.join("result.txt");
    state
        .pending_once_grants
        .insert((DenialKind::FsWrite, lawful.to_string_lossy().into_owned()));
    let pending = state.pending_once_grants.clone();
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        fixture.caveats(),
        Some(fixture.operator.join("new-signing-key.pem")),
        None,
        vec![PromptChoice::AllowPermanent],
        prompts.clone(),
    );
    gate.config_path = Some(fixture.operator.join("config.toml"));
    assert!(matches!(
        gate.ask(&[
            request(DenialKind::FsWrite, &lawful),
            request(DenialKind::FsRead, &fixture.key),
        ]),
        newt_core::PermissionDecision::Deny
    ));
    drop(gate);
    assert_eq!(prompts.get(), 0);
    assert_eq!(state.pending_once_grants, pending);
    assert!(state.session_grants.is_empty());
    assert!(!fixture.operator.join("new-signing-key.pem").exists());
    assert!(!fixture.operator.join("ocap").exists());
}

#[test]
fn recalled_permissions_filter_protected_paths_but_keep_independent_grants() {
    let fixture = Fixture::new();
    let mut state = fixture.state();
    state.session_grants.insert((
        DenialKind::FsRead,
        fixture.key.to_string_lossy().into_owned(),
    ));
    state.durable_grants.insert((
        DenialKind::FsWrite,
        fixture.operator.to_string_lossy().into_owned(),
    ));
    state
        .durable_grants
        .insert((DenialKind::Exec, "inspect".into()));
    let mut base = fixture.caveats();
    base.exec = Scope::none();
    let recalled = state.recalled_caveats(&base, None).unwrap();
    assert_eq!(recalled.fs_read, base.fs_read);
    assert_eq!(recalled.fs_write, base.fs_write);
    assert!(recalled.exec.permits(&"inspect".into()));
    assert_eq!(recalled.net, base.net);
}

#[test]
fn unsafe_baseline_is_refused_by_refresh_current_and_recall() {
    let fixture = Fixture::new();
    let mut state = fixture.state();
    let mut unsafe_base = fixture.caveats();
    unsafe_base.fs_read = Scope::All;
    assert!(state.recalled_caveats(&unsafe_base, None).is_err());
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        unsafe_base.clone(),
        None,
        None,
        vec![],
        prompts.clone(),
    );
    assert!(gate.current_caveats().is_err());
    assert!(matches!(
        gate.refresh_caveats(&unsafe_base),
        newt_core::PermissionDecision::Deny
    ));
    assert_eq!(prompts.get(), 0);
}

#[test]
fn prepared_build_fence_cannot_borrow_a_protected_read_root() {
    let fixture = Fixture::new();
    let mut state = fixture.state();
    state.session_grants.insert((
        DenialKind::Build,
        fixture.workspace.to_string_lossy().into_owned(),
    ));
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        fixture.caveats(),
        None,
        None,
        vec![],
        prompts.clone(),
    );
    let mut build = newt_core::confined_exec::build_tool_caveats(&fixture.workspace);
    build.fs_read = Scope::only([fixture.key.to_string_lossy().into_owned()]);
    assert!(matches!(
        gate.ask_with_caveats(&build, &[request(DenialKind::Build, &fixture.workspace)]),
        newt_core::PermissionDecision::Deny
    ));
    assert_eq!(prompts.get(), 0);
}

#[test]
fn lawful_build_preserves_network_and_does_not_require_profile_exec_ceiling() {
    let fixture = Fixture::new();
    let mut state = fixture.state();
    let prompts = Rc::new(Cell::new(0));
    let mut base = fixture.caveats();
    base.exec = Scope::none();
    let mut gate = scripted_gate(
        &mut state,
        base,
        None,
        None,
        vec![PromptChoice::AllowOnce],
        prompts.clone(),
    );
    let mut build = newt_core::confined_exec::build_tool_caveats(&fixture.workspace);
    build.net = Scope::All;
    let newt_core::PermissionDecision::Allow(allowed) =
        gate.ask_with_caveats(&build, &[request(DenialKind::Build, &fixture.workspace)])
    else {
        panic!("ordinary authorized build must remain available")
    };
    assert_eq!(allowed, build);
    assert_eq!(prompts.get(), 1);
}

#[cfg(unix)]
#[test]
fn combined_grants_are_checked_before_a_writable_alias_anchor_is_approved() {
    let fixture = Fixture::new();
    let public = fixture._temp.path().join("public");
    std::fs::create_dir(&public).unwrap();
    let alias = fixture.workspace.join("read-root");
    std::os::unix::fs::symlink(&public, &alias).unwrap();
    let mut base = fixture.caveats();
    base.fs_read = Scope::only([alias.to_string_lossy().into_owned()]);
    base.fs_write = Scope::none();
    fixture.guard.validate_caveats(&base).unwrap();
    let mut state = fixture.state();
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        base,
        None,
        None,
        vec![PromptChoice::AllowSession],
        prompts.clone(),
    );
    assert!(matches!(
        gate.ask(&[request(DenialKind::FsWrite, &fixture.workspace)]),
        newt_core::PermissionDecision::Deny
    ));
    drop(gate);
    assert_eq!(prompts.get(), 0);
    assert!(state.session_grants.is_empty());
}

#[test]
fn recalled_redundant_canonical_children_do_not_disable_a_workspace_profile() {
    let fixture = Fixture::new();
    let child = fixture.workspace.join("already-covered");
    std::fs::create_dir(&child).unwrap();
    let mut state = fixture.state();
    state.durable_grants.extend([
        (DenialKind::FsRead, child.to_string_lossy().into_owned()),
        (DenialKind::FsWrite, child.to_string_lossy().into_owned()),
    ]);
    let base = fixture.caveats();
    let recalled = state.recalled_caveats(&base, None).unwrap();
    assert_eq!(
        recalled, base,
        "existing authority already covers both roots"
    );
    let gate = scripted_gate(
        &mut state,
        base.clone(),
        None,
        None,
        vec![],
        Rc::new(Cell::new(0)),
    );
    assert_eq!(gate.current_caveats().unwrap(), base);
    assert_eq!(
        state.durable_grants.len(),
        2,
        "emission must not edit the saved approvals"
    );
}

#[cfg(unix)]
#[test]
fn recalled_alias_is_not_discarded_as_an_already_covered_lexical_child() {
    let fixture = Fixture::new();
    let outside = fixture._temp.path().canonicalize().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let alias = fixture.workspace.join("alias");
    std::os::unix::fs::symlink(&outside, &alias).unwrap();
    let mut state = fixture.state();
    state
        .durable_grants
        .insert((DenialKind::FsRead, alias.to_string_lossy().into_owned()));
    let base = fixture.caveats();
    assert!(
        state.recalled_caveats(&base, None).is_err(),
        "the named child resolves outside the already-granted root and has a writable alias anchor"
    );
}
