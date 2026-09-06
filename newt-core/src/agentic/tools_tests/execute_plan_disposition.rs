use super::*;

async fn run_scheduled_tool(
    name: &str,
    args: &serde_json::Value,
    ws: &tempfile::TempDir,
    ledger: &crate::agentic::scheduled::SessionStepLedger,
    plan_mode_control: &dyn crate::agentic::PlanModeControl,
) -> String {
    execute_tool_with_collaborators(
        name,
        args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats_rw(ws.path()),
        &mut NoMcp,
        ToolCollaborators {
            step_ledger: Some(ledger as &dyn crate::agentic::scheduled::StepLedger),
            plan_mode_control: Some(plan_mode_control),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .expect("test dispatch is not cancellable")
}

/// #1056: a git WRITE the projected authority denies is no longer a dead end
/// — with a gate that ALLOWS, the arm re-dispatches under the local-write
/// surface and the commit lands (the deadlock fix). The gate is consulted for
/// a `git_write` capability.
#[test]
fn plan_phase_seam_and_clamp() {
    use crate::caveats::ScopeExt as _;
    // The clamp is read-only: reads yes, writes/exec/net no.
    let c = plan_phase_clamp();
    assert!(c.fs_read.permits(&"/anything".to_string()));
    assert!(!c.fs_write.permits(&"/anything".to_string()));
    assert!(!c.exec.permits(&"cargo".to_string()));
    assert!(!c.net.permits(&"github.com".to_string()));
    // MEETing it into a full grant yields read-only (never widens).
    let full = crate::caveats::Caveats::top();
    let planned = full.meet(&c);
    assert!(
        !planned.fs_write.permits(&"/x".to_string()),
        "writes denied in plan phase"
    );
    assert!(planned.fs_read.permits(&"/x".to_string()), "reads allowed");
}

#[tokio::test]
async fn enter_and_exit_plan_mode_are_session_local_and_immediate() {
    use crate::agentic::PlanModeControl as _;

    #[derive(Default)]
    struct TestPlanModeControl(std::sync::atomic::AtomicBool);

    impl crate::agentic::PlanModeControl for TestPlanModeControl {
        fn is_plan_mode(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::Acquire)
        }

        fn set_plan_mode(&self, active: bool) -> Result<(), String> {
            self.0.store(active, std::sync::atomic::Ordering::Release);
            Ok(())
        }
    }

    // enter_plan_mode / exit_plan_mode mutate only their injected session
    // control; there is no process-global flag shared with another session.
    let ws = tempfile::TempDir::new().unwrap();
    let ledger = crate::agentic::scheduled::SessionStepLedger::default();
    let control = TestPlanModeControl::default();
    let other_session = TestPlanModeControl::default();
    assert!(!control.is_plan_mode());
    let control_only = execute_tool_with_collaborators(
        "enter_plan_mode",
        &serde_json::json!({}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats_rw(ws.path()),
        &mut NoMcp,
        ToolCollaborators {
            plan_mode_control: Some(&control),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .expect("test dispatch is not cancellable");
    assert!(
        control_only.contains("scheduled planning"),
        "control-only fabricated call must fail honestly: {control_only}"
    );
    assert!(
        !control.is_plan_mode(),
        "a control without a plan ledger must not enter Plan"
    );
    let control_only_exit = execute_tool_with_collaborators(
        "exit_plan_mode",
        &serde_json::json!({}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats_rw(ws.path()),
        &mut NoMcp,
        ToolCollaborators {
            plan_mode_control: Some(&control),
            ..Default::default()
        },
        false,
        PromptDisposition::Plan,
        None,
    )
    .await
    .expect("test dispatch is not cancellable");
    assert!(
        control_only_exit.contains("exited the model-entered PLAN PHASE"),
        "exit must remain available when scheduled planning is off: {control_only_exit}"
    );
    let enter = run_scheduled_tool(
        "enter_plan_mode",
        &serde_json::json!({}),
        &ws,
        &ledger,
        &control,
    )
    .await;
    assert!(enter.contains("PLAN MODE"), "{enter}");
    assert!(control.is_plan_mode(), "enter_plan_mode set the phase");
    assert!(
        !other_session.is_plan_mode(),
        "one session must not change another"
    );
    let denied_write = run_scheduled_tool(
        "write_file",
        &serde_json::json!({
            "path": "must-not-write.txt",
            "content": "no",
        }),
        &ws,
        &ledger,
        &control,
    )
    .await;
    assert!(
        denied_write.contains("is not available for this request"),
        "a write later in the same tool round must hit the immediate Plan clamp: {denied_write}"
    );
    assert!(
        !ws.path().join("must-not-write.txt").exists(),
        "entering Plan must prevent a later call from mutating the workspace"
    );
    let exit = run_scheduled_tool(
        "exit_plan_mode",
        &serde_json::json!({}),
        &ws,
        &ledger,
        &control,
    )
    .await;
    assert!(
        exit.contains("exited the model-entered PLAN PHASE"),
        "{exit}"
    );
    assert!(!control.is_plan_mode(), "exit_plan_mode cleared the phase");
}

/// A non-Act disposition is an executor boundary, not just a reduced tool
/// schema: fabricated mutations, exec, capability requests, and remote MCP
/// calls must be refused before they reach their own dispatch logic.
#[tokio::test]
async fn non_act_disposition_denies_mutation_exec_grants_and_generic_mcp() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top(); // prove disposition wins over ambient authority

    let mut no_mcp = NoMcp;
    let write = run_tool_with_disposition(
        "write_file",
        serde_json::json!({ "path": "must-not-write.txt", "content": "no" }),
        ws.path(),
        &caveats,
        &mut no_mcp,
        None,
        None,
        PromptDisposition::Research,
    )
    .await;
    assert!(
        write.contains("is not available for this request"),
        "got: {write}"
    );
    assert!(
        !ws.path().join("must-not-write.txt").exists(),
        "disposition rejection must precede the write handler"
    );

    let exec = run_tool_with_disposition(
        "run_command",
        serde_json::json!({ "command": "touch must-not-exec.txt" }),
        ws.path(),
        &caveats,
        &mut no_mcp,
        None,
        None,
        PromptDisposition::Explain,
    )
    .await;
    assert!(
        exec.contains("is not available for this request"),
        "got: {exec}"
    );
    assert!(
        !ws.path().join("must-not-exec.txt").exists(),
        "disposition rejection must precede the shell handler"
    );

    let mut gate = MockGate::new(true, &caveats);
    let grant = run_tool_with_disposition(
        "request_permissions",
        serde_json::json!({
            "capability": "fs_write",
            "target": "/tmp/should-not-be-granted",
            "reason": "test",
        }),
        ws.path(),
        &caveats,
        &mut no_mcp,
        Some(&mut gate),
        None,
        PromptDisposition::Research,
    )
    .await;
    assert!(
        grant.contains("is not available for this request"),
        "got: {grant}"
    );
    assert!(
        gate.asks.is_empty(),
        "non-Act must not consult a grant gate"
    );

    let mut mcp = OneRemoteTool::new("incident__read");
    let remote = run_tool_with_disposition(
        "incident__read",
        serde_json::json!({}),
        ws.path(),
        &caveats,
        &mut mcp,
        None,
        None,
        PromptDisposition::Research,
    )
    .await;
    assert!(
        remote.contains("is not available for this request"),
        "got: {remote}"
    );
    assert!(
        !mcp.called,
        "generic MCP must be denied before remote routing in non-Act"
    );

    std::fs::write(ws.path().join("evidence.txt"), "durable evidence\n").unwrap();
    let read = run_tool_with_disposition(
        "read_file",
        serde_json::json!({ "path": "evidence.txt" }),
        ws.path(),
        &caveats,
        &mut no_mcp,
        None,
        None,
        PromptDisposition::Research,
    )
    .await;
    assert!(
        read.contains("durable evidence"),
        "safe read must remain usable: {read}"
    );
}

/// Plan is a read-only workspace disposition with one explicit
/// control-plane write: the harness-owned step ledger.
#[tokio::test]
async fn plan_disposition_updates_ledger_but_still_denies_workspace_mutation() {
    use crate::agentic::scheduled::{SessionStepLedger, StepLedger};

    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    let ledger = SessionStepLedger::default();
    let mut no_mcp = NoMcp;
    let plan = run_tool_with_disposition(
        "update_plan",
        serde_json::json!({ "plan": [
                { "step": "inspect", "status": "completed" },
                { "step": "repair", "status": "in_progress" }
            ] }),
        ws.path(),
        &caveats,
        &mut no_mcp,
        None,
        Some(&ledger),
        PromptDisposition::Plan,
    )
    .await;
    assert!(plan.starts_with("<plan>\n"), "{plan}");
    assert_eq!(ledger.count(), 2);

    let write = run_tool_with_disposition(
        "write_file",
        serde_json::json!({ "path": "must-not-write.txt", "content": "no" }),
        ws.path(),
        &caveats,
        &mut no_mcp,
        None,
        Some(&ledger),
        PromptDisposition::Plan,
    )
    .await;
    assert!(
        write.contains("is not available for this request"),
        "{write}"
    );
    assert!(!ws.path().join("must-not-write.txt").exists());
}

#[tokio::test]
async fn auto_mode_selector_dispatches_through_session_control_without_current_widening() {
    #[derive(Default)]
    struct RecordingControl(std::sync::Mutex<Vec<String>>);

    impl crate::agentic::OperatingModeControl for RecordingControl {
        fn select_operating_mode(&self, mode: &str) -> Result<String, String> {
            self.0.lock().unwrap().push(mode.to_string());
            Ok(format!("scheduled {mode}; current turn unchanged"))
        }
    }

    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    let control = RecordingControl::default();
    let mut no_mcp = NoMcp;
    let result = execute_tool_with_collaborators(
        "select_operating_mode",
        &serde_json::json!({ "mode": "dev" }),
        ws.path().to_str().unwrap(),
        false,
        20,
        &caveats,
        &mut no_mcp,
        ToolCollaborators {
            operating_mode_control: Some(&control),
            ..Default::default()
        },
        false,
        PromptDisposition::Research,
        None,
    )
    .await
    .unwrap();
    assert!(result.contains("current turn unchanged"), "{result}");
    assert_eq!(*control.0.lock().unwrap(), vec!["dev"]);

    let unavailable = execute_tool_with_collaborators(
        "select_operating_mode",
        &serde_json::json!({ "mode": "dev" }),
        ws.path().to_str().unwrap(),
        false,
        20,
        &caveats,
        &mut no_mcp,
        ToolCollaborators::default(),
        false,
        PromptDisposition::Research,
        None,
    )
    .await
    .unwrap();
    assert!(unavailable.contains("/mode auto"), "{unavailable}");
}

/// Permitted non-Act reads still honor their caveats, but they must not
/// silently turn a denial into an interactive authority grant.
#[tokio::test]
async fn non_act_read_tools_do_not_consult_permission_gate() {
    let ws = tempfile::TempDir::new().unwrap();
    let mut caveats = Caveats::top();
    caveats.net = crate::caveats::Scope::none();
    let mut gate = MockGate::new(true, &caveats);
    let mut mcp = NoMcp;

    let _ = run_tool_with_disposition(
        "web_fetch",
        serde_json::json!({ "url": "https://example.com" }),
        ws.path(),
        &caveats,
        &mut mcp,
        Some(&mut gate),
        None,
        PromptDisposition::Research,
    )
    .await;
    assert!(
        gate.asks.is_empty(),
        "a non-Act web read may be caveat-denied but must never mint net authority"
    );
}
