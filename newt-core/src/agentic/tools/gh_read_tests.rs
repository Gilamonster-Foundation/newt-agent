//! In-memory acceptance through the production run_command dispatcher.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
pub(crate) struct Probe {
    pub(super) host: AtomicUsize,
    pub(super) confined: AtomicUsize,
}

#[derive(Default)]
struct Gate(Vec<PermissionRequest>);
impl PermissionGate for Gate {
    fn ask_question(&mut self, _: &str) -> HumanQuestionOutcome {
        panic!("read broker must not ask a freeform question")
    }
    fn ask(&mut self, requests: &[PermissionRequest]) -> PermissionDecision {
        self.0.extend_from_slice(requests);
        PermissionDecision::Deny
    }
}

/// #3089: launch grants must reach a host-enforcing read broker, not the
/// confined child whose network scope is necessarily deny-all. Resources at
/// the dispatch boundaries are in-memory; no directory, executable or server
/// is consulted. The ignored real broker ground verifies these stand-ins.
#[tokio::test]
async fn launch_granted_gh_issue_view_uses_host_broker_without_prompts() {
    let probe = Probe::default();
    let mut gate = Gate::default();
    let caveats = crate::Caveats {
        net: crate::Scope::only(["github.com".into(), "api.github.com".into()]),
        fs_write: crate::Scope::only(["/virtual-workspace".into()]),
        ..crate::Caveats::top()
    };
    let mut display = ToolDisplay::new(Vec::<u8>::new(), false, 80, 20, false);
    let result = execute_tool_inner(
        &mut display,
        "run_command",
        &serde_json::json!({"command": "gh issue view 1 -R owner/repo"}),
        "/virtual-workspace",
        false,
        20,
        &caveats,
        &mut crate::agentic::NoMcp,
        ToolCollaborators {
            command_probe: Some(&probe),
            permission_gate: Some(&mut gate),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
    )
    .await;
    assert_eq!(
        probe.host.load(Ordering::SeqCst),
        1,
        "launch-granted gh issue view did not reach the host broker: {result}"
    );
    assert_eq!(probe.confined.load(Ordering::SeqCst), 0);
    assert!(
        gate.0.is_empty(),
        "launch grants must avoid permission prompts"
    );
    assert!(result.contains("issue 1"), "{result}");
}
