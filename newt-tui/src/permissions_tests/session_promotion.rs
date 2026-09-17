use super::workflow::{load_session_grants, review_session_grants, store_session_grants};
use super::*;
use crate::mcp::Mcp;
use newt_core::{Caveats, CaveatsExt as _, DenialKind, PermissionGate as _, PermissionRequest};
use std::cell::Cell;
use std::rc::Rc;

fn grants() -> newt_core::durable_grants::GrantSet {
    [
        (DenialKind::Exec, "inspect"),
        (DenialKind::FsRead, "/fixture/read"),
        (DenialKind::FsWrite, "/fixture/write"),
        (DenialKind::Net, "example.test"),
        (DenialKind::RemoteTool, "server__search"),
        (DenialKind::GitWrite, "commit"),
    ]
    .into_iter()
    .map(|(kind, target)| (kind, target.to_owned()))
    .collect()
}

#[test]
fn promotion_reviews_only_actual_session_grants_and_cancel_writes_nothing() {
    let expected = grants();
    for answer in [
        HumanQuestionOutcome::Answer(String::new()),
        HumanQuestionOutcome::Answer("no".into()),
        HumanQuestionOutcome::Cancelled,
        HumanQuestionOutcome::InputClosed,
        HumanQuestionOutcome::InputFailed,
        HumanQuestionOutcome::Unavailable,
    ] {
        let mut state = PermissionPromptState {
            session_grants: expected.clone(),
            ..Default::default()
        };
        state
            .pending_once_grants
            .insert((DenialKind::Exec, "once-only".into()));
        state
            .durable_grants
            .insert((DenialKind::Net, "previous.test".into()));
        let ask = |interaction: &SurfaceInteraction| {
            assert_eq!(interaction.default_choice().unwrap().id.as_str(), "no");
            let rendered = plain::render(&interaction.definition);
            for (kind, target) in &expected {
                assert!(
                    rendered.contains(&format!("{}: {target}", kind.as_str())),
                    "{rendered}"
                );
            }
            assert!(!rendered.contains("once-only"));
            assert!(!rendered.contains("previous.test"));
            answer.clone()
        };
        assert_eq!(
            review_session_grants(&mut state, None, None, &ask, |_| panic!(
                "cancel reached persistence"
            ))
            .unwrap(),
            0
        );
        assert_eq!(state.session_grants, expected);
        assert_eq!(state.durable_grants.len(), 1);
    }
}

#[test]
fn promotion_saves_exact_reviewed_snapshot_and_uses_verified_result_only() {
    let mut state = PermissionPromptState {
        session_grants: grants(),
        ..Default::default()
    };
    let expected = state.session_grants.clone();
    let ask = |_interaction: &SurfaceInteraction| HumanQuestionOutcome::Answer("yes".into());
    assert!(
        review_session_grants(&mut state, None, None, &ask, |_| Err("save failed".into())).is_err()
    );
    assert!(state.durable_grants.is_empty());
    let count = review_session_grants(&mut state, None, None, &ask, |snapshot| {
        assert_eq!(snapshot, &expected);
        Ok(snapshot.clone())
    })
    .unwrap();
    assert_eq!(count, expected.len());
    assert_eq!(state.durable_grants, expected);
    assert_eq!(state.session_grants, expected);
}

#[test]
fn empty_session_never_opens_confirmation_or_persistence() {
    let mut state = PermissionPromptState::default();
    assert_eq!(
        review_session_grants(
            &mut state,
            None,
            None,
            &|_| panic!("nothing to confirm"),
            |_| panic!("nothing to save")
        )
        .unwrap(),
        0
    );
}

#[test]
fn a_nonreplayable_session_snapshot_is_rejected_before_confirmation() {
    for blocked in ["deny", "danger", "preset", "delegation"] {
        let mut state = PermissionPromptState::default();
        let grant = (
            DenialKind::Exec,
            if blocked == "danger" {
                "bash"
            } else {
                "inspect"
            }
            .to_string(),
        );
        state.session_grants.insert(grant.clone());
        if blocked == "deny" {
            state.session_denials.insert(grant);
        }
        let floor = Caveats {
            exec: newt_core::caveats::Scope::none(),
            ..Caveats::top()
        };
        let result = review_session_grants(
            &mut state,
            (blocked == "preset").then_some(&floor),
            (blocked == "delegation").then_some(&floor),
            &|_| panic!("blocked grant reached confirmation"),
            |_| panic!("blocked grant reached persistence"),
        );
        assert!(result.is_err(), "{blocked} must reject the whole snapshot");
        assert!(state.durable_grants.is_empty());
    }
}

#[test]
fn durable_allows_are_exact_and_denials_still_win() {
    let mut state = PermissionPromptState::default();
    state
        .durable_grants
        .insert((DenialKind::Net, "example.test".into()));
    let prompts = Rc::new(Cell::new(0));
    let request = PermissionRequest {
        tool: "mcp connect".into(),
        kind: DenialKind::Net,
        target: "example.test".into(),
        reason: String::new(),
    };
    let base = Caveats {
        net: newt_core::caveats::Scope::none(),
        ..Caveats::top()
    };
    {
        let mut gate = super::permission_prompt_tests::scripted_gate(
            &mut state,
            base.clone(),
            None,
            None,
            vec![],
            prompts.clone(),
        );
        let (caveats, hosts, retained) = gate.ask_mcp_net_grant(&request).unwrap();
        assert!(caveats.permits_net("example.test"));
        assert_eq!(hosts, vec!["example.test"]);
        assert!(retained);
        assert!(!gate.mint(&gate.base, &[]).permits_net("sibling.test"));
    }
    assert_eq!(prompts.get(), 0);
    assert!(state.session_grants.is_empty());
    state
        .session_denials
        .insert((request.kind, request.target.clone()));
    let mut gate = super::permission_prompt_tests::scripted_gate(
        &mut state,
        base,
        None,
        None,
        vec![],
        prompts.clone(),
    );
    assert!(matches!(
        gate.ask(&[request]),
        newt_core::PermissionDecision::Deny
    ));
    assert!(!gate.mint(&gate.base, &[]).permits_net("example.test"));
}

/// Grounds recalled-scope denial filtering in the real built-in filesystem
/// dispatch: a directory grant must not bypass the gate for a denied child.
#[serial_test::serial(real_fs)]
#[tokio::test]
async fn recalled_directory_grants_do_not_bypass_permanent_child_denials() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let child = root.join("denied.txt");
    std::fs::write(&child, "original").unwrap();
    for (kind, tool) in [
        (DenialKind::FsRead, "read_file"),
        (DenialKind::FsWrite, "write_file"),
    ] {
        let mut state = PermissionPromptState::default();
        state
            .durable_grants
            .insert((kind, root.display().to_string()));
        state
            .persistent_denials
            .insert((kind, child.display().to_string()));
        let base = Caveats {
            fs_read: newt_core::caveats::Scope::none(),
            fs_write: newt_core::caveats::Scope::none(),
            ..Caveats::top()
        };
        let caveats = state.recalled_caveats(&base, None);
        let prompts = Rc::new(Cell::new(0));
        let mut gate = super::permission_prompt_tests::scripted_gate(
            &mut state,
            base,
            None,
            None,
            vec![],
            prompts.clone(),
        );
        let output = newt_core::agentic::execute_tool(
            tool,
            &serde_json::json!({"path": child, "content": "changed"}),
            &root.to_string_lossy(),
            false,
            20,
            &caveats,
            &mut Mcp::empty(),
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
        assert!(
            output.starts_with("capability denied:"),
            "{kind:?}: {output}"
        );
        assert_eq!(prompts.get(), 0, "permanent denial must not prompt");
        assert_eq!(std::fs::read_to_string(&child).unwrap(), "original");
    }
}

#[test]
fn recalled_basename_exec_grant_excludes_a_permanently_denied_path() {
    let mut state = PermissionPromptState::default();
    state
        .durable_grants
        .insert((DenialKind::Exec, "inspect".into()));
    state
        .persistent_denials
        .insert((DenialKind::Exec, "/fixture/bin/inspect".into()));
    let base = Caveats {
        exec: newt_core::caveats::Scope::none(),
        ..Caveats::top()
    };
    assert!(!state.recalled_caveats(&base, None).permits_exec("inspect"));
    let configured = Caveats {
        exec: newt_core::caveats::Scope::only(["inspect".into()]),
        ..base
    };
    assert!(
        state
            .recalled_caveats(&configured, None)
            .permits_exec("inspect"),
        "recall filtering must not rewrite configured authority"
    );
}

#[test]
fn recall_overlap_checks_all_deny_sources_without_removing_unrelated_authority() {
    for source in ["session", "persistent", "ocap"] {
        for (kind, allowed, denied, unrelated) in [
            (
                DenialKind::FsRead,
                "/fixture",
                "/fixture/denied",
                "/fixture-safe",
            ),
            (
                DenialKind::FsWrite,
                "/fixture",
                "/fixture/denied",
                "/fixture-safe",
            ),
            (
                DenialKind::Exec,
                "inspect",
                "/fixture/bin/inspect",
                "/other/bin/inspect",
            ),
        ] {
            let mut state = PermissionPromptState::default();
            state.session_grants.insert((kind, allowed.into()));
            state.durable_grants.insert((kind, unrelated.into()));
            match source {
                "session" => {
                    state.session_denials.insert((kind, denied.into()));
                }
                "persistent" => {
                    state.persistent_denials.insert((kind, denied.into()));
                }
                _ => {
                    let (class, field) = if kind == DenialKind::Exec {
                        ("exec", "target")
                    } else {
                        ("fs", "path")
                    };
                    state.ocap_policy = newt_core::ocap_store::build_store(&[(
                        newt_core::ocap_store::Verdict::Deny,
                        Some(format!("[[{class}]]\n{field} = \"{denied}\"\n")),
                    )])
                    .0;
                }
            }
            let base = Caveats {
                fs_read: newt_core::caveats::Scope::none(),
                fs_write: newt_core::caveats::Scope::none(),
                exec: newt_core::caveats::Scope::none(),
                ..Caveats::top()
            };
            let recalled = state.recalled_caveats(&base, None);
            assert!(
                !ceiling_permits(&recalled, kind, allowed),
                "{source} {kind:?}"
            );
            assert!(
                ceiling_permits(&recalled, kind, unrelated),
                "{source} {kind:?}"
            );
            let configured = newt_core::widen_caveats(&base, &[(kind, allowed.into())]);
            assert!(ceiling_permits(
                &state.recalled_caveats(&configured, None),
                kind,
                allowed
            ));
        }
    }
}

#[test]
fn promotion_rejects_broad_grants_overlapping_a_denial_atomically() {
    for (kind, allowed, denied) in [
        (DenialKind::FsRead, "/fixture", "/fixture/denied.txt"),
        (DenialKind::FsWrite, "/fixture", "/fixture/denied.txt"),
        (DenialKind::Exec, "inspect", "/fixture/bin/inspect"),
    ] {
        let mut state = PermissionPromptState::default();
        state.session_grants.insert((kind, allowed.into()));
        state
            .session_grants
            .insert((DenialKind::Net, "example.test".into()));
        state.persistent_denials.insert((kind, denied.into()));
        let result = review_session_grants(
            &mut state,
            None,
            None,
            &|_| panic!("overlapping authority reached confirmation"),
            |_| panic!("overlapping authority reached persistence"),
        );
        assert!(
            result.is_err(),
            "{kind:?}: overlapping snapshot must fail closed"
        );
        assert!(state.durable_grants.is_empty());
    }
}

#[test]
fn durable_remote_allow_works_with_new_prompts_disabled() {
    let mut state = PermissionPromptState::default();
    let request = PermissionRequest {
        tool: "server__search".into(),
        kind: DenialKind::RemoteTool,
        target: "server__search".into(),
        reason: String::new(),
    };
    state
        .durable_grants
        .insert((request.kind, request.target.clone()));
    let prompts = Rc::new(Cell::new(0));
    let mut gate = super::permission_prompt_tests::scripted_gate(
        &mut state,
        Caveats::top(),
        None,
        None,
        vec![],
        prompts.clone(),
    );
    gate.authorization_prompts_enabled = false;
    assert!(matches!(
        gate.ask(std::slice::from_ref(&request)),
        newt_core::PermissionDecision::Allow(_)
    ));
    let missing = PermissionRequest {
        target: "server__other".into(),
        ..request
    };
    assert!(matches!(
        gate.ask(&[missing]),
        newt_core::PermissionDecision::Deny
    ));
    assert_eq!(prompts.get(), 0);
}

/// Real filesystem grounding for the startup/save key checks: existing store
/// bytes and missing keys must survive a failed load or promotion unchanged.
#[test]
fn existing_store_without_signing_key_never_generates_or_replaces_keys() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.toml");
    let store = newt_core::durable_grants::store_path(&config);
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    std::fs::write(&store, "existing ciphertext").unwrap();
    let key = temp.path().join("missing.pem");
    assert!(load_session_grants(Some(&config), Some(&key), temp.path()).is_err());
    assert!(store_session_grants(Some(&config), Some(&key), temp.path(), &grants()).is_err());
    assert!(!key.exists());
    assert_eq!(
        std::fs::read_to_string(store).unwrap(),
        "existing ciphertext"
    );
}

// Model: GPT-6 | Harness: Codex | Operator: Shawn Hartsock | Time: 14:31 EDT | Date: 2026-09-16
