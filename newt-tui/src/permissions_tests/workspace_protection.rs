//! Operator-resource protection at real permission gate boundaries.

use super::permission_prompt_tests::scripted_gate;
use super::*;
use newt_core::{Caveats, Scope};
use newt_policy::{DenialKind, PermissionGate as _, PermissionRequest, ScopeExt as _};
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

/// Grounds `refusal_tests::pre_prompt_refusal_records_guard_reason_and_notifies_operator_once`:
/// a real mutable build anchor produces the same refusal and the production
/// append path persists its reason in a verifiable chain. Run single-threaded
/// with the existing native permission test feature.
#[cfg(feature = "native-permission-test")]
#[test]
fn real_build_refusal_writes_one_chained_record_and_one_notice() {
    let fixture = Fixture::new();
    let git = fixture.workspace.join(".git");
    std::fs::create_dir(&git).unwrap();
    let mut build = newt_core::confined_exec::build_tool_caveats(&fixture.workspace);
    if let Scope::Only(reads) = &mut build.fs_read {
        reads.insert(git.to_string_lossy().into_owned());
    }
    let reason = fixture
        .guard
        .validate_caveats(&build)
        .unwrap_err()
        .to_string();
    assert!(reason.contains("model-writable ancestor"), "{reason}");
    let log = fixture._temp.path().join("permission-log.jsonl");
    let notices = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut state = fixture.state();
    state.notices = Some(notices.clone());
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        build.clone(),
        None,
        Some(log.clone()),
        vec![],
        prompts.clone(),
    );
    assert!(matches!(
        gate.ask_with_caveats(&build, &[request(DenialKind::Build, &fixture.workspace)]),
        newt_policy::PermissionDecision::Deny
    ));
    assert_eq!(prompts.get(), 0);
    assert_eq!(gate.state.decisions.len(), 1);
    let notices = notices.lock().unwrap();
    assert_eq!(
        *notices,
        [format!(
            "permission refused (workspace-protection): {reason}"
        )]
    );
    let lines = newt_core::permission_journal::read_jsonl(&std::fs::read_to_string(&log).unwrap());
    assert_eq!(lines.len(), 1);
    let record = lines[0].node.payload();
    assert_eq!(record.decision, "deny");
    assert_eq!(record.scope, "workspace-protection");
    assert_eq!(record.reason.as_deref(), Some(reason.as_str()));
    let head = newt_core::permission_journal::read_head(&log).unwrap();
    assert!(newt_core::permission_journal::verify_chain(&lines, Some(&head)).is_empty());
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
        newt_policy::PermissionDecision::Deny
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
        newt_policy::PermissionDecision::Deny
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
        newt_policy::PermissionDecision::Deny
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
    let newt_policy::PermissionDecision::Allow(allowed) =
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
        newt_policy::PermissionDecision::Deny
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
    assert_ne!(
        state.widen_caveats(
            &base,
            &state.durable_grants.iter().cloned().collect::<Vec<_>>()
        ),
        base,
        "an alias must reach validation, not be silently treated as covered"
    );
    assert_eq!(state.recalled_caveats(&base, None).unwrap(), base);
    assert_eq!(state.rejected_durable_grants.borrow().len(), 1);
    assert_eq!(
        state.durable_grants.len(),
        1,
        "signed store remains untouched"
    );
    assert_eq!(state.recalled_caveats(&base, None).unwrap(), base);
}

/// #2959 regression: the `/context request` preview recalls authority through
/// `peek_recalled_caveats`. The committing form quarantines a grant that fails
/// validation (and `denied` then refuses it for the rest of the session), so a
/// preview made while a durable root was briefly invalid used to poison it.
/// The peek returns the same caveats and leaves every set it can reach as it
/// found it; a later committing recall still behaves as before.
#[cfg(unix)]
#[test]
fn peeking_recalled_caveats_does_not_quarantine_a_failing_grant() {
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
    let before = (
        state.rejected_durable_grants.borrow().clone(),
        state.durable_grants.clone(),
        state.session_grants.clone(),
        state.session_denials.clone(),
    );
    for _ in 0..2 {
        assert_eq!(state.peek_recalled_caveats(&base, None).unwrap(), base);
        assert_eq!(
            before,
            (
                state.rejected_durable_grants.borrow().clone(),
                state.durable_grants.clone(),
                state.session_grants.clone(),
                state.session_denials.clone(),
            ),
            "a peek must not change permission state"
        );
    }
    assert!(state.rejected_durable_grants.borrow().is_empty());
    assert_eq!(state.recalled_caveats(&base, None).unwrap(), base);
    assert_eq!(
        state.rejected_durable_grants.borrow().len(),
        1,
        "the committing recall still quarantines"
    );
}

/// A dead exact approval under an existing same-axis root is redundant too.
#[test]
fn dead_covered_grants_do_not_add_unstable_nested_roots() {
    let fixture = Fixture::new();
    let state = fixture.state();
    let base = fixture.caveats();
    let dead = fixture
        .workspace
        .join("deleted/plan.md")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        state.widen_caveats(
            &base,
            &[
                (DenialKind::FsRead, dead.clone()),
                (DenialKind::FsWrite, dead)
            ]
        ),
        base
    );
    let outside = fixture
        ._temp
        .path()
        .join("outside/deleted.md")
        .to_string_lossy()
        .into_owned();
    assert_ne!(
        state.widen_caveats(&base, &[(DenialKind::FsWrite, outside)]),
        base
    );
}

/// Grounds `refusal_tests::refresh_refusal_records_mint_reason_and_notifies_operator_once`
/// against real Git metadata and the production guard. A failed worktree add
/// must leave both spellings of Git usable under workspace-scoped authority.
/// Run explicitly with `native-permission-test`, single-threaded.
#[cfg(all(unix, feature = "native-permission-test"))]
#[tokio::test]
async fn failed_worktree_creation_does_not_poison_git_refresh() {
    use crate::disable_ocap_session_tests::EnvVar;
    let _env = crate::test_env_guard::env_write_guard_async().await;
    let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "safe-subset");
    let _ocap = EnvVar::unset("NEWT_DISABLE_OCAP");
    let _full = EnvVar::unset("NEWT_FULL_ACCESS");
    assert!(newt_core::confined_exec::kernel_fs_fence_available());
    let fixture = Fixture::new();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(&fixture.workspace)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-b", "main"]).status.success());
    assert!(git(&["commit", "--allow-empty", "-m", "fixture"])
        .status
        .success());
    newt_core::git_hardening::own_gitdir_grants(&fixture.workspace);
    assert!(git(&["checkout", "-b", "refresh-fixture"]).status.success());
    let base = fixture.caveats();
    let destination = fixture._temp.path().join("destination");
    let mut state = fixture.state();
    let mut gate = scripted_gate(
        &mut state,
        base.clone(),
        None,
        None,
        vec![PromptChoice::AllowOnce, PromptChoice::AllowOnce],
        Rc::new(Cell::new(0)),
    );
    assert!(matches!(
        gate.ask_with_caveats(
            &base,
            &[
                request(DenialKind::FsRead, &destination),
                request(DenialKind::FsWrite, &destination),
            ]
        ),
        newt_policy::PermissionDecision::Allow(_)
    ));
    std::fs::create_dir(&destination).unwrap();
    let failed = git(&[
        "worktree",
        "add",
        "-b",
        "refresh-fixture",
        destination.to_str().unwrap(),
    ]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("already exists"));
    for program in ["git", "/usr/bin/git"] {
        let out = newt_core::execute_tool(
            "run_command",
            &serde_json::json!({"command": format!("{program} status --short --branch")}),
            fixture.workspace.to_str().unwrap(),
            false,
            20,
            &base,
            &mut newt_core::NoMcp,
            None,
            None,
            None,
            None,
            Some(&mut gate),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(out.contains("## refresh-fixture"), "{program}: {out}");
    }
    assert_eq!(
        gate.state.decisions.len(),
        2,
        "only the destination approvals are recorded"
    );
}
