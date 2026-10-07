use super::*;

async fn run_scheduled_tool(
    name: &str,
    args: &serde_json::Value,
    ws: &tempfile::TempDir,
    ledger: &crate::agentic::scheduled::SessionStepLedger,
    plan_mode_control: &dyn crate::agentic::PlanModeControl,
) -> String {
    run_scheduled_tool_with_disposition(
        name,
        args,
        ws,
        ledger,
        plan_mode_control,
        PromptDisposition::Act,
    )
    .await
}

async fn run_scheduled_tool_with_disposition(
    name: &str,
    args: &serde_json::Value,
    ws: &tempfile::TempDir,
    ledger: &crate::agentic::scheduled::SessionStepLedger,
    plan_mode_control: &dyn crate::agentic::PlanModeControl,
    disposition: PromptDisposition,
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
        disposition,
        None,
    )
    .await
    .expect("legacy fixture has no durable writer")
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
    struct TestPlanModeControl(std::sync::atomic::AtomicBool, std::sync::atomic::AtomicBool);

    impl crate::agentic::PlanModeControl for TestPlanModeControl {
        fn is_plan_mode(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::Acquire)
        }

        fn set_plan_mode(&self, active: bool) -> Result<(), String> {
            self.0.store(active, std::sync::atomic::Ordering::Release);
            Ok(())
        }

        fn request_exit(&self) -> Result<(), String> {
            self.1.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        }

        fn take_exit_requested(&self) -> bool {
            self.1.swap(false, std::sync::atomic::Ordering::AcqRel)
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
    .expect("legacy fixture has no durable writer")
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
    .expect("legacy fixture has no durable writer")
    .expect("test dispatch is not cancellable");
    // #2424: exit_plan_mode now REQUESTS the clamp lift; it never lifts it
    // itself. Only the turn-end approval hook may call set_plan_mode(false).
    assert!(
        control_only_exit.contains("exit requested"),
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
    for disposition in [
        PromptDisposition::Act,
        PromptDisposition::Explain,
        PromptDisposition::Research,
    ] {
        let denied_write = run_scheduled_tool_with_disposition(
            "write_file",
            &serde_json::json!({
                "path": "must-not-write.txt",
                "content": "no",
            }),
            &ws,
            &ledger,
            &control,
            disposition,
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
    }
    let exit = run_scheduled_tool(
        "exit_plan_mode",
        &serde_json::json!({}),
        &ws,
        &ledger,
        &control,
    )
    .await;
    assert!(exit.contains("exit requested"), "{exit}");
    assert!(
        control.is_plan_mode(),
        "exit_plan_mode must NOT clear the clamp itself — only a turn-end \
         approval may (#2424, the second root cause this design fixes)"
    );
    assert!(
        control.take_exit_requested(),
        "the request must be recorded for the turn-end hook to see"
    );
}

/// Explicit Plan is an executor boundary, not just a reduced tool schema.
#[tokio::test]
async fn explicit_plan_denies_mutation_exec_grants_and_generic_mcp() {
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
        PromptDisposition::Plan,
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
        PromptDisposition::Plan,
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
        PromptDisposition::Plan,
    )
    .await;
    assert!(
        grant.contains("is not available for this request"),
        "got: {grant}"
    );
    assert!(
        gate.asks.is_empty(),
        "explicit Plan must not consult a grant gate"
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
        PromptDisposition::Plan,
    )
    .await;
    assert!(
        remote.contains("not available for this request"),
        "got: {remote}"
    );
    assert!(
        !mcp.called,
        "a remote call without permission must not reach the server"
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
        PromptDisposition::Plan,
    )
    .await;
    assert!(
        read.contains("durable evidence"),
        "safe read must remain usable: {read}"
    );
}

/// Style cannot grant a denied effect or suppress an operator permission
/// decision. Exercise the actual handlers, not only the advertised catalog.
#[tokio::test]
async fn inferred_style_enforces_caveats_and_preserves_permission_requests() {
    let _lock = super::disable_ocap_tests::env_lock().await;
    let _ocap = super::disable_ocap_tests::EnvVar::set("NEWT_DISABLE_OCAP", "0");
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats {
        fs_read: Scope::only([ws.path().to_string_lossy().into_owned()]),
        fs_write: Scope::none(),
        exec: Scope::none(),
        net: Scope::none(),
        ..Caveats::top()
    };
    for disposition in [PromptDisposition::Explain, PromptDisposition::Research] {
        let write = run_tool_with_disposition(
            "write_file",
            serde_json::json!({"path": "denied.txt", "content": "no"}),
            ws.path(),
            &caveats,
            &mut NoMcp,
            None,
            None,
            disposition,
        )
        .await;
        assert!(
            write.contains("capability denied: fs_write"),
            "{disposition:?}: {write}"
        );
        let exec = run_tool_with_disposition(
            "run_command",
            serde_json::json!({"command": "touch denied.txt"}),
            ws.path(),
            &caveats,
            &mut NoMcp,
            None,
            None,
            disposition,
        )
        .await;
        assert!(
            exec.contains("capability denied:"),
            "{disposition:?}: {exec}"
        );
        assert!(!ws.path().join("denied.txt").exists());
        let mut gate = MockGate::new(true, &caveats);
        let grant = run_tool_with_disposition(
            "request_permissions",
            serde_json::json!({
                "capability": "fs_write",
                "target": ws.path().join("allowed.txt"),
                "reason": "requested task needs an output file",
            }),
            ws.path(),
            &caveats,
            &mut NoMcp,
            Some(&mut gate),
            None,
            disposition,
        )
        .await;
        assert_eq!(gate.asks.len(), 1, "{disposition:?}: {grant}");
        assert!(!grant.contains("not available for this request"), "{grant}");
    }
}

/// Inferred style preserves discovery while actual remote authority remains
/// subject to the permission gate.
#[tokio::test]
async fn explain_discovery_finds_mcp_but_dispatch_requires_permission() {
    let ws = std::path::Path::new("/nonexistent-workspace");
    let caveats = Caveats::top();
    let mut mcp = OneRemoteTool::new("review__fetch_change");

    let found = run_tool_with_disposition(
        "tool_search",
        serde_json::json!({ "query": "fetch change" }),
        ws,
        &caveats,
        &mut mcp,
        None,
        None,
        PromptDisposition::Explain,
    )
    .await;
    assert!(
        found.contains("- review__fetch_change — ")
            && !found.contains("review__fetch_change — not callable"),
        "connected MCP tools remain discoverable: {found}"
    );

    let call = run_tool_with_disposition(
        "review__fetch_change",
        serde_json::json!({}),
        ws,
        &caveats,
        &mut mcp,
        None,
        None,
        PromptDisposition::Explain,
    )
    .await;
    assert!(
        call.contains("requires OCAP permission"),
        "dispatch remains the boundary: {call}"
    );
    assert!(
        !mcp.called,
        "a tool without permission must not reach the remote server"
    );

    // Twin: Ask admits no tool at all. Discovery itself is refused and
    // reports nothing as hidden.
    let asked = run_tool_with_disposition(
        "tool_search",
        serde_json::json!({ "query": "fetch change" }),
        ws,
        &caveats,
        &mut mcp,
        None,
        None,
        PromptDisposition::Ask,
    )
    .await;
    assert!(
        asked.contains("Tool `tool_search` is not available for this request"),
        "{asked}"
    );
    assert!(!asked.contains("review__fetch_change"), "{asked}");
}

/// A session Plan-mode control that also answers `exit_requested`, so a
/// test can see a pending approval without consuming it.
#[derive(Default)]
struct ApprovalControl(
    std::sync::atomic::AtomicBool,
    std::sync::atomic::AtomicBool,
    std::sync::atomic::AtomicBool,
);

impl crate::agentic::PlanModeControl for ApprovalControl {
    fn is_plan_mode(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }

    fn set_plan_mode(&self, active: bool) -> Result<(), String> {
        self.0.store(active, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn request_exit(&self) -> Result<(), String> {
        self.1.store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn exit_requested(&self) -> bool {
        self.1.load(std::sync::atomic::Ordering::Acquire)
    }

    fn take_exit_requested(&self) -> bool {
        self.1.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    fn implementing_approved_plan(&self) -> bool {
        self.2.load(std::sync::atomic::Ordering::Acquire)
    }
}

fn two_step_plan() -> serde_json::Value {
    serde_json::json!({ "plan": [
        { "step": "inspect", "status": "in_progress" },
        { "step": "repair", "status": "pending" }
    ] })
}

/// Plan-before-act: the first multi-step plan an acting turn sets, under an
/// initiative level that looks before it acts, enters the Plan phase on the
/// model's behalf and requests the operator's approval — then a tick of the
/// same plan asks nothing more.
#[tokio::test]
async fn a_fresh_multi_step_plan_under_a_looking_initiative_requests_approval() {
    use crate::agentic::PlanModeControl as _;
    let _initiative =
        crate::initiative::scoped_effective_initiative(crate::initiative::Initiative::Measured);
    let ws = tempfile::TempDir::new().unwrap();
    let ledger = crate::agentic::scheduled::SessionStepLedger::default();
    let control = ApprovalControl::default();

    let out = run_scheduled_tool("update_plan", &two_step_plan(), &ws, &ledger, &control).await;
    assert!(out.starts_with("<plan>\n"), "{out}");
    assert!(out.contains("awaiting operator approval"), "{out}");
    assert!(
        control.is_plan_mode(),
        "the clamp is on for the rest of the batch"
    );
    assert!(
        control.exit_requested(),
        "the turn-end hook is owed a question"
    );

    // A write later in the same batch is clamped, not run.
    let write = run_scheduled_tool(
        "write_file",
        &serde_json::json!({ "path": "early.txt", "content": "no" }),
        &ws,
        &ledger,
        &control,
    )
    .await;
    assert!(
        write.contains("is not available for this request"),
        "{write}"
    );
    assert!(!ws.path().join("early.txt").exists());

    // The hook consumed the request; a tick of the same plan is silent.
    assert!(control.take_exit_requested());
    let tick = run_scheduled_tool(
        "update_plan",
        &serde_json::json!({ "plan": [
            { "step": "inspect", "status": "completed" },
            { "step": "repair", "status": "in_progress" }
        ] }),
        &ws,
        &ledger,
        &control,
    )
    .await;
    assert!(!tick.contains("awaiting operator approval"), "{tick}");
    assert!(!control.exit_requested());
}

/// The levels chosen for models that must be pushed to act are not held at a
/// question: decisive and eager set the plan and keep going.
#[tokio::test]
async fn an_acting_initiative_sets_the_plan_silently() {
    use crate::agentic::PlanModeControl as _;
    let _initiative =
        crate::initiative::scoped_effective_initiative(crate::initiative::Initiative::Eager);
    let ws = tempfile::TempDir::new().unwrap();
    let ledger = crate::agentic::scheduled::SessionStepLedger::default();
    let control = ApprovalControl::default();

    let out = run_scheduled_tool("update_plan", &two_step_plan(), &ws, &ledger, &control).await;
    assert!(out.starts_with("<plan>\n"), "{out}");
    assert!(!out.contains("awaiting operator approval"), "{out}");
    assert!(!control.is_plan_mode());
    assert!(!control.exit_requested());
}

/// The implementing turn an approval seeded carries the operator's decision:
/// when the model records the approved plan in the ledger, nothing asks again.
#[tokio::test]
async fn an_approved_plans_implementing_turn_is_not_asked_again() {
    use crate::agentic::PlanModeControl as _;
    let _initiative =
        crate::initiative::scoped_effective_initiative(crate::initiative::Initiative::Measured);
    let ws = tempfile::TempDir::new().unwrap();
    let ledger = crate::agentic::scheduled::SessionStepLedger::default();
    let control = ApprovalControl::default();
    control.2.store(true, std::sync::atomic::Ordering::Release);

    let out = run_scheduled_tool("update_plan", &two_step_plan(), &ws, &ledger, &control).await;
    assert!(out.starts_with("<plan>\n"), "{out}");
    assert!(!out.contains("awaiting operator approval"), "{out}");
    assert!(!control.is_plan_mode());
    assert!(!control.exit_requested());
}

/// An evidence turn may plan its reading without being halted at a question
/// whose "yes" would seed an implementing turn nobody asked for.
#[tokio::test]
async fn a_research_turn_sets_its_plan_without_requesting_approval() {
    use crate::agentic::PlanModeControl as _;
    let _initiative =
        crate::initiative::scoped_effective_initiative(crate::initiative::Initiative::Measured);
    let ws = tempfile::TempDir::new().unwrap();
    let ledger = crate::agentic::scheduled::SessionStepLedger::default();
    let control = ApprovalControl::default();
    let out = run_scheduled_tool_with_disposition(
        "update_plan",
        &two_step_plan(),
        &ws,
        &ledger,
        &control,
        PromptDisposition::Research,
    )
    .await;
    assert!(out.starts_with("<plan>\n"), "{out}");
    assert!(!out.contains("awaiting operator approval"), "{out}");
    assert!(!control.is_plan_mode());
    assert!(!control.exit_requested());
}

/// A single-step plan is not a plan worth a question, and a turn already in
/// the Plan phase is headed for the approval hook anyway — neither asks.
#[tokio::test]
async fn single_step_and_plan_phase_plans_do_not_request_approval() {
    use crate::agentic::PlanModeControl as _;
    let _initiative =
        crate::initiative::scoped_effective_initiative(crate::initiative::Initiative::Patient);
    let ws = tempfile::TempDir::new().unwrap();

    let ledger = crate::agentic::scheduled::SessionStepLedger::default();
    let control = ApprovalControl::default();
    let one = run_scheduled_tool(
        "update_plan",
        &serde_json::json!({ "plan": [{ "step": "inspect", "status": "in_progress" }] }),
        &ws,
        &ledger,
        &control,
    )
    .await;
    assert!(one.starts_with("<plan>\n"), "{one}");
    assert!(!control.exit_requested());

    let ledger = crate::agentic::scheduled::SessionStepLedger::default();
    let control = ApprovalControl::default();
    let planned = run_scheduled_tool_with_disposition(
        "update_plan",
        &two_step_plan(),
        &ws,
        &ledger,
        &control,
        PromptDisposition::Plan,
    )
    .await;
    assert!(planned.starts_with("<plan>\n"), "{planned}");
    assert!(!planned.contains("awaiting operator approval"), "{planned}");
    assert!(!control.is_plan_mode());
    assert!(!control.exit_requested());
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
    .expect("legacy fixture has no durable writer")
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
    .expect("legacy fixture has no durable writer")
    .unwrap();
    assert!(unavailable.contains("/mode auto"), "{unavailable}");
}

/// Explicit Plan forbids new grants; inferred style preserves the operator's
/// permission decision. A denied network request cannot be silently widened.
#[tokio::test]
async fn web_reads_respect_explicit_plan_and_operator_network_decisions() {
    let ws = tempfile::TempDir::new().unwrap();
    let mut caveats = Caveats::top();
    caveats.net = crate::caveats::Scope::none();
    let mut mcp = NoMcp;
    for (disposition, questions) in [
        (PromptDisposition::Plan, 0),
        (PromptDisposition::Explain, 1),
        (PromptDisposition::Research, 1),
    ] {
        let mut gate = MockGate::new(false, &caveats);
        let _ = run_tool_with_disposition(
            "web_fetch",
            serde_json::json!({ "url": "https://example.com" }),
            ws.path(),
            &caveats,
            &mut mcp,
            Some(&mut gate),
            None,
            disposition,
        )
        .await;
        assert_eq!(
            gate.asks.len(),
            questions,
            "{disposition:?} must preserve the explicit authority boundary"
        );
    }
}

/// #2424: the dispatch arm itself must route `render_report` to the wired
/// draft sink under Plan, and back to the ordinary rendered-document path
/// under Act with the exact same sink still wired (i.e. it's Plan disposition
/// that gates the behavior, not merely whether a sink exists).
#[tokio::test]
async fn render_report_dispatch_drafts_under_plan_and_renders_under_act() {
    use crate::agentic::PlanDraftSink as _;

    #[derive(Default)]
    struct FakeSink(std::sync::Mutex<Option<crate::agentic::PlanDraft>>);
    impl crate::agentic::PlanDraftSink for FakeSink {
        fn save_draft(&self, markdown: String) -> Result<u32, String> {
            let mut slot = self.0.lock().unwrap();
            let revision = slot.as_ref().map_or(1, |d| d.revision + 1);
            *slot = Some(crate::agentic::PlanDraft { revision, markdown });
            Ok(revision)
        }

        fn latest_draft(&self) -> Option<crate::agentic::PlanDraft> {
            self.0.lock().unwrap().clone()
        }
    }

    let ws = tempfile::TempDir::new().unwrap();
    let sink = FakeSink::default();

    let plan_result = execute_tool_with_collaborators(
        "render_report",
        &serde_json::json!({"title": "Draft under Plan"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats_rw(ws.path()),
        &mut NoMcp,
        ToolCollaborators {
            plan_draft_sink: Some(&sink as &dyn crate::agentic::PlanDraftSink),
            ..Default::default()
        },
        false,
        PromptDisposition::Plan,
        None,
    )
    .await
    .unwrap()
    .unwrap();

    assert!(
        plan_result.contains("revision 1"),
        "Plan disposition must route to the draft slot: {plan_result}"
    );
    assert!(
        sink.latest_draft().is_some(),
        "the sink must have received the draft"
    );

    let act_result = execute_tool_with_collaborators(
        "render_report",
        &serde_json::json!({"title": "Rendered under Act"}),
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats_rw(ws.path()),
        &mut NoMcp,
        ToolCollaborators {
            plan_draft_sink: Some(&sink as &dyn crate::agentic::PlanDraftSink),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();

    assert!(
        act_result.contains("report rendered"),
        "Act disposition must render as before, even with a draft sink wired: {act_result}"
    );
    assert_eq!(
        sink.latest_draft().unwrap().revision,
        1,
        "an Act-disposition report must never touch the draft slot"
    );
}
