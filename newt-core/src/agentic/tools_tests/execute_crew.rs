use super::*;

// #479: the agent-callable crew/compose_roster tools route through the
// injected CrewRunner — same presence-gating + dispatch shape as `git`.
struct StubCrew;

#[async_trait::async_trait]
impl crate::agentic::CrewRunner for StubCrew {
    async fn dispatch(
        &self,
        op: &str,
        _args: &serde_json::Value,
        _caveats: &Caveats,
    ) -> Result<String, String> {
        match op {
            "compose_roster" => Ok("proposed roster: planner <- qwen3-coder:30b".to_string()),
            "crew" => Ok("crew ran: diff +1/-0, status PASS".to_string()),
            other => Err(format!("unknown op: {other}")),
        }
    }
}

async fn run_crew_tool(
    name: &str,
    args: serde_json::Value,
    crew: Option<&dyn crate::agentic::CrewRunner>,
) -> String {
    let ws = tempfile::TempDir::new().unwrap();
    execute_tool(
        name,
        &args,
        &ws.path().to_string_lossy(),
        false,
        20,
        &caveats_rw(ws.path()),
        &mut NoMcp,
        None, // build_check_cmd
        None, // note_sink
        None, // recall_source
        None, // memory_source
        None, // permission_gate
        None, // exec_floor
        None, // git_tool
        crew,
        None, // scratchpad_store
        None, // code_search
        None, // where_is
        None, // experience_store
        None, // step_ledger
    )
    .await
}

#[tokio::test]
async fn crew_arm_dispatches_when_injected() {
    let out = run_crew_tool(
        "crew",
        serde_json::json!({ "task": "do X" }),
        Some(&StubCrew),
    )
    .await;
    assert!(
        out.contains("crew ran") && out.contains("PASS"),
        "got: {out}"
    );
    let out = run_crew_tool(
        "compose_roster",
        serde_json::json!({ "mode": "crew" }),
        Some(&StubCrew),
    )
    .await;
    assert!(out.contains("proposed roster"), "got: {out}");
}

/// #479 (G4): with no `CrewRunner` injected (the OFF default), the dispatch
/// arm coaches recovery instead of the old flat `unknown tool` dead-end — it
/// names the operator gesture (`NEWT_TEAM`) and a real solo alternative, and
/// must NOT read as "unknown tool".
#[tokio::test]
async fn crew_arm_without_injection_coaches_recovery() {
    for name in ["crew", "compose_roster"] {
        let out = run_crew_tool(name, serde_json::json!({ "task": "x" }), None).await;
        assert!(out.contains("NEWT_TEAM"), "{name}: {out}");
        assert!(out.contains("read_file"), "{name}: {out}");
        assert!(!out.contains("unknown tool"), "{name}: {out}");
    }
}

/// #479 (G4): the factored coach helper names the gate + a real alternative
/// and never reads as "unknown tool" — the regression point for the wording.
#[test]
fn crew_off_recovery_result_names_gate_and_alternative() {
    let out = crew_off_recovery_result("crew");
    assert!(out.contains("'crew'"), "{out}");
    assert!(out.contains("NEWT_TEAM"), "{out}");
    // A real, always-available solo alternative is offered.
    assert!(out.contains("write_file"), "{out}");
    assert!(!out.contains("unknown tool"), "{out}");
}

/// #479 (G4): the gated-off telemetry seam. A `crew`/`compose_roster` reach
/// with the surface OFF records a `GatedOff` phantom; the same names with the
/// surface ON record nothing (they dispatch normally), and a non-crew name is
/// never gated-off.
#[test]
fn classify_gated_off_reach_only_fires_for_off_crew_names() {
    for name in ["crew", "compose_roster"] {
        assert_eq!(
            classify_gated_off_reach(name, false),
            Some(crate::PhantomResolution::GatedOff(
                "crew/team surface off (NEWT_TEAM)".into()
            )),
            "{name} OFF should record GatedOff"
        );
        assert_eq!(
            classify_gated_off_reach(name, true),
            None,
            "{name} ON dispatches normally — no phantom"
        );
    }
    // A non-crew tool is never a gated-off reach, OFF or ON.
    assert_eq!(classify_gated_off_reach("read_file", false), None);
    assert_eq!(classify_gated_off_reach("read_file", true), None);
}

/// #479 (G4) guard: the OFF-state changes do not touch `is_hallucination`
/// (crew/compose_roster stay real names) or `classify_phantom_reach` for the
/// crew names — both kept exactly so the ON path stays a normal dispatch.
#[test]
fn crew_names_stay_real_and_unflagged_by_existing_seams() {
    for name in ["crew", "compose_roster"] {
        assert!(
            !is_hallucination(name, &serde_json::json!({ "task": "x" })),
            "{name} must stay a real tool name"
        );
        assert_eq!(
            classify_phantom_reach(name, &serde_json::json!({ "task": "x" }), "ok", true),
            None,
            "{name} must not be flagged by classify_phantom_reach"
        );
    }
}
