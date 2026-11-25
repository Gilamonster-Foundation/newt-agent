//! Refusal observability with injected protection, clock and operator output.

use super::permission_prompt_tests::scripted_gate;
use super::*;
use newt_policy::{DenialKind, PermissionDecision, PermissionGate, PermissionRequest};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

const REASON: &str =
    "workspace-protection anchor operator-only/identity has a model-writable ancestor";

struct RejectBaseline;

impl PermissionProtection for RejectBaseline {
    fn validate_caveats(&self, _: &newt_core::Caveats) -> anyhow::Result<()> {
        anyhow::bail!(REASON)
    }

    fn validate_request(&self, _: DenialKind, _: &str) -> anyhow::Result<()> {
        panic!("the rejected baseline must short-circuit request validation")
    }
}

#[test]
fn all_pre_prompt_paths_report_once_without_consuming_pending_authority() {
    use newt_core::{Caveats, Scope};
    use std::sync::atomic::AtomicUsize;

    struct RejectAt(usize, AtomicUsize);
    impl PermissionProtection for RejectAt {
        fn validate_caveats(&self, _: &Caveats) -> anyhow::Result<()> {
            let call = self.1.fetch_add(1, Ordering::Relaxed) + 1;
            anyhow::ensure!(call != self.0, REASON);
            Ok(())
        }
        fn validate_request(&self, _: DenialKind, _: &str) -> anyhow::Result<()> {
            anyhow::ensure!(self.0 != 0, REASON);
            Ok(())
        }
    }

    for (case, cause) in [
        ("operation", "invalid-operation-request"),
        ("empty", "empty-request"),
        ("request", "workspace-protection"),
        ("combined", "workspace-protection"),
        ("prepared", "preset-ceiling"),
        ("filesystem", "preset-ceiling"),
        ("prior", "prior-denial"),
        ("mint", "authority-mint"),
        // The actual build projection resolves its scratch root; keep that
        // branch in the native tier alongside the real guard/log grounding.
        #[cfg(feature = "native-permission-test")]
        ("fallback", "workspace-protection"),
        ("disabled", "authorization-prompts-disabled"),
    ] {
        let notices = Arc::new(Mutex::new(Vec::new()));
        let mut state = PermissionPromptState {
            decision_time: Some("2026-11-24T00:00:00Z"),
            notices: Some(notices.clone()),
            ..Default::default()
        };
        let request = PermissionRequest {
            tool: "build_exec".into(),
            kind: DenialKind::Exec,
            target: "cargo".into(),
            reason: "build".into(),
            harness_bound: true,
        };
        state
            .pending_once_grants
            .insert((DenialKind::Exec, "unrelated".into()));
        let pending = state.pending_once_grants.clone();
        if case == "prior" {
            state
                .session_denials
                .insert((request.kind, request.target.clone()));
        }
        if let Some(at) = match case {
            "request" => Some(0),
            "combined" => Some(2),
            "mint" | "fallback" => Some(3),
            _ => None,
        } {
            state.protection = Some(Arc::new(RejectAt(at, AtomicUsize::new(0))));
        }
        if case == "fallback" {
            state
                .session_grants
                .insert((DenialKind::Build, "workspace".into()));
        }
        let prompts = Rc::new(Cell::new(0));
        let mut gate = scripted_gate(
            &mut state,
            Caveats::top(),
            None,
            None,
            vec![],
            prompts.clone(),
        );
        let mut requests = vec![request];
        match case {
            "empty" => requests.clear(),
            "prior" => requests.insert(
                0,
                PermissionRequest {
                    target: "lawful-sibling".into(),
                    ..requests[0].clone()
                },
            ),
            "prepared" | "filesystem" => {
                gate.preset_clamp = Some(Caveats {
                    fs_read: Scope::none(),
                    ..Caveats::top()
                });
                requests[0].kind = if case == "prepared" {
                    DenialKind::Build
                } else {
                    DenialKind::FsRead
                };
            }
            "disabled" => gate.authorization_prompts_enabled = false,
            _ => {}
        }
        assert!(
            matches!(
                gate.ask_recorded(
                    &Caveats::top(),
                    &requests,
                    case == "operation",
                    case == "mint"
                ),
                PermissionDecision::Deny
            ),
            "{case}"
        );
        assert_eq!(prompts.get(), 0, "{case}");
        assert_eq!(gate.state.pending_once_grants, pending, "{case}");
        assert_eq!(gate.state.decisions.len(), 1, "{case}");
        let record = &gate.state.decisions[0];
        assert_eq!(record.decision, "deny", "{case}");
        assert_eq!(record.scope, cause, "{case}");
        if case == "prior" {
            assert_eq!(record.target, "cargo", "record the refused sibling");
        }
        assert!(
            record
                .reason
                .as_ref()
                .is_some_and(|reason| !reason.is_empty()),
            "{case}"
        );
        assert_eq!(notices.lock().unwrap().len(), 1, "{case}");
        assert_eq!(gate.take_refusal_cause(), Some(cause), "{case}");
    }
}

#[test]
fn pre_prompt_refusal_records_guard_reason_and_notifies_operator_once() {
    let notices = Arc::new(Mutex::new(Vec::new()));
    let mut state = PermissionPromptState {
        protection: Some(Arc::new(RejectBaseline)),
        decision_time: Some("2026-11-24T00:00:00Z"),
        notices: Some(notices.clone()),
        ..Default::default()
    };
    let prompts = Rc::new(Cell::new(0));
    let mut gate = scripted_gate(
        &mut state,
        newt_core::Caveats::top(),
        None,
        None,
        vec![],
        prompts.clone(),
    );
    let request = PermissionRequest {
        tool: "build_exec".into(),
        kind: DenialKind::Build,
        target: "workspace".into(),
        reason: "build the workspace".into(),
        harness_bound: true,
    };
    assert!(matches!(
        gate.ask_with_caveats(&newt_core::Caveats::top(), &[request]),
        PermissionDecision::Deny
    ));
    assert_eq!(prompts.get(), 0);
    assert_eq!(
        gate.state.decisions.len(),
        1,
        "pre-prompt refusals must enter the existing decision log"
    );
    let record = &gate.state.decisions[0];
    assert_eq!(record.decision, "deny");
    assert_eq!(record.scope, "workspace-protection");
    assert_eq!(serde_json::to_value(record).unwrap()["reason"], REASON);
    assert_eq!(
        *notices.lock().unwrap(),
        [format!(
            "permission refused (workspace-protection): {REASON}"
        )]
    );
    assert_eq!(gate.take_refusal_cause(), Some("workspace-protection"));
    assert_eq!(gate.take_refusal_cause(), None);
}

#[test]
fn refusal_reason_is_addressed_without_readdressing_existing_permission_records() {
    use newt_core::{event_journal::Journal, permission_journal};
    let legacy = serde_json::json!({
        "ts_claim": "2026-11-24T00:00:00Z", "conversation_id": "test",
        "tool": "build_exec", "kind": "build", "target": "workspace",
        "decision": "deny", "scope": "workspace-protection",
    });
    let mut journal = Journal::new();
    let old = journal.append(legacy).unwrap();
    let mut lines = permission_journal::read_jsonl(&old.render_line().unwrap());
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].id, old.id);
    assert!(
        lines[0].is_intact(),
        "absent reasons must preserve old canonical bytes"
    );
    let mut record = lines[0].node.payload().clone();
    record.reason = Some(REASON.into());
    let new = journal.append(record).unwrap();
    assert_ne!(new.id, old.id);
    let id = new.id.clone();
    lines.extend(permission_journal::read_jsonl(&new.render_line().unwrap()));
    assert!(permission_journal::verify_chain(&lines, Some(&id)).is_empty());
    assert_eq!(lines[1].node.payload().reason.as_deref(), Some(REASON));
    lines[1].node.payload.reason = Some("altered".into());
    assert!(
        !lines[1].is_intact(),
        "refusal detail must be covered by the CID"
    );
}
