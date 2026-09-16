use super::*;

#[tokio::test]
async fn persona_preferences_research_discovers_and_requests_remote_permission() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    for disposition in [PromptDisposition::Explain, PromptDisposition::Research] {
        let mut mcp = OneRemoteTool::new("mail__search");
        let defs =
            filter_tools_for_disposition(serde_json::Value::Array(mcp.tool_defs()), disposition);
        assert_eq!(
            defs.as_array().unwrap().len(),
            1,
            "{disposition:?} needs discovery"
        );
        for allow in [false, true] {
            let mut gate = MockGate::new(allow, &caveats);
            let out = run_tool_with_disposition(
                "mail__search",
                serde_json::json!({}),
                ws.path(),
                &caveats,
                &mut mcp,
                Some(&mut gate),
                None,
                disposition,
            )
            .await;
            assert_eq!(gate.asks.len(), 1, "{disposition:?}: {out}");
            assert_eq!(mcp.called, allow, "{out}");
        }
    }
    for disposition in [PromptDisposition::Plan, PromptDisposition::Ask] {
        let mut mcp = OneRemoteTool::new("mail__search");
        let mut gate = MockGate::new(true, &caveats);
        run_tool_with_disposition(
            "mail__search",
            serde_json::json!({}),
            ws.path(),
            &caveats,
            &mut mcp,
            Some(&mut gate),
            None,
            disposition,
        )
        .await;
        assert!(!mcp.called);
        assert!(
            gate.asks.is_empty(),
            "{disposition:?} must not request authority"
        );
    }
}

#[tokio::test]
async fn persona_preferences_unknown_remote_name_cannot_request_permission() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    let mut gate = MockGate::new(true, &caveats);
    run_tool_with_disposition(
        "unknown__search",
        serde_json::json!({}),
        ws.path(),
        &caveats,
        &mut NoMcp,
        Some(&mut gate),
        None,
        PromptDisposition::Research,
    )
    .await;
    assert!(gate.asks.is_empty());
}

#[tokio::test]
async fn persona_preferences_never_authorize_a_remote_call() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    let preferred = vec!["mail__search".to_string()];
    let mut mcp = OneRemoteTool::new("mail__search");
    let out = run_remote_gated(
        "mail__search",
        ws.path(),
        &caveats,
        Some(&preferred),
        &mut mcp,
        None,
    )
    .await;
    assert!(
        !mcp.called,
        "persona preferences cannot mint a remote grant"
    );
    assert!(out.contains("permission"), "{out}");
    assert!(!out.contains("active persona"), "{out}");
}

#[tokio::test]
async fn persona_preferences_remote_refusal_reports_the_permission_failure() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = Caveats::top();
    let mut mcp = OneRemoteTool::new("mail__search");
    let mut gate = MockGate::new(false, &caveats);
    let out = run_remote_gated(
        "mail__search",
        ws.path(),
        &caveats,
        None,
        &mut mcp,
        Some(&mut gate),
    )
    .await;
    assert!(!mcp.called);
    assert_eq!(gate.asks.len(), 1);
    assert!(out.contains("permission"), "{out}");
    assert!(!out.contains("persona"), "{out}");
}

/// FR-2 (#1001): a remote MCP tool OUTSIDE the persona's allow-list is
/// `mcp-under-leash`: with NO active persona, a MUTATING remote tool must NOT
/// dispatch unleashed — "no persona" is not "unrestricted". Headless (no
/// gate) → fail-closed. Regression for the pre-leash hole where a no-persona
/// session dispatched every remote tool with zero mediation.
#[tokio::test]
async fn no_persona_does_not_dispatch_a_mutating_mcp_tool_unleashed() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = crate::caveats::Caveats::top();

    let mut mcp = OneRemoteTool::new("incident__delete"); // mutating verb
    let out = run_remote_gated(
        "incident__delete",
        ws.path(),
        &caveats,
        None, // NO persona
        &mut mcp,
        None, // headless: no gate to consult
    )
    .await;
    assert!(
        !mcp.called,
        "a no-persona mutating remote tool must NOT dispatch unleashed"
    );
    assert!(out.contains("permission"));
}

/// `mcp-under-leash` (name-classification vector): a no-persona READ-verb
/// remote tool is NOT auto-granted by its name. A hostile admitted server
/// that names a destructive operation with a read verb (`get_…`) earns
/// nothing — headless it is DENIED (fail-closed), and it dispatches only on
/// an explicit human grant. (Was `..._still_dispatches`, which asserted the
/// now-closed name-auto-grant; flipped red→green with the leash slice.)
#[tokio::test]
async fn no_persona_read_verb_tool_is_not_name_granted() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = crate::caveats::Caveats::top();

    // A hostile server named a destructive op with a read verb. Headless, no
    // persona: the read verb does NOT grant — fail-closed.
    let mut mcp = OneRemoteTool::new("evil__get_wipe_everything");
    let out = run_remote_gated(
        "evil__get_wipe_everything",
        ws.path(),
        &caveats,
        None, // no persona
        &mut mcp,
        None, // headless: no human to consult
    )
    .await;
    assert!(
        !mcp.called,
        "a no-persona read-verb-named remote tool must NOT dispatch on the name alone"
    );
    assert!(out.contains("permission"), "{out}");

    // The SAME tool dispatches only when a present human explicitly grants it.
    let mut mcp = OneRemoteTool::new("evil__get_wipe_everything");
    let mut gate = MockGate::new(true, &caveats);
    let out = run_remote_gated(
        "evil__get_wipe_everything",
        ws.path(),
        &caveats,
        None,
        &mut mcp,
        Some(&mut gate),
    )
    .await;
    assert!(mcp.called, "an explicitly human-granted call dispatches");
    assert_eq!(
        gate.asks.len(),
        1,
        "the human WAS prompted (no silent name-grant)"
    );
    assert_eq!(out, "remote-tool-ran");
}

/// A human can still grant a no-persona MUTATING tool through the gate.
#[tokio::test]
async fn no_persona_mutating_mcp_tool_dispatches_when_human_grants() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = crate::caveats::Caveats::top();

    let mut mcp = OneRemoteTool::new("incident__delete");
    let mut gate = MockGate::new(true, &caveats); // human allows
    let out = run_remote_gated(
        "incident__delete",
        ws.path(),
        &caveats,
        None,
        &mut mcp,
        Some(&mut gate),
    )
    .await;
    assert!(
        mcp.called,
        "a human-granted mutating remote tool dispatches"
    );
    assert_eq!(
        gate.asks.len(),
        1,
        "the human was prompted for the mutating op"
    );
    assert_eq!(out, "remote-tool-ran");
}

/// PROMPTED (not hard-vetoed like a built-in). Deny → withheld and `call`
/// never runs; Allow → dispatched; a tool the persona already grants
/// dispatches with NO prompt; headless (no gate) fails closed.
#[tokio::test]
async fn remote_tool_outside_allow_list_is_prompted_not_hard_vetoed() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = crate::caveats::Caveats::top();
    let coach = vec!["read_file".to_string()]; // no incident__create

    // Gate DENIES → withheld; `call` never invoked; the human WAS prompted,
    // and prompted as a remote-tool grant (not an fs/exec/net axis).
    let mut mcp = OneRemoteTool::new("incident__create");
    let mut gate = MockGate::new(false, &caveats);
    let out = run_remote_gated(
        "incident__create",
        ws.path(),
        &caveats,
        Some(&coach),
        &mut mcp,
        Some(&mut gate),
    )
    .await;
    assert!(!mcp.called, "denied remote tool must NOT dispatch");
    assert_eq!(gate.asks.len(), 1, "the human was prompted");
    assert_eq!(gate.asks[0].1, "remote_tool:incident__create");
    assert!(out.contains("permission"), "returns a denial: {out}");

    // Gate ALLOWS → dispatched.
    let mut mcp = OneRemoteTool::new("incident__create");
    let mut gate = MockGate::new(true, &caveats);
    let out = run_remote_gated(
        "incident__create",
        ws.path(),
        &caveats,
        Some(&coach),
        &mut mcp,
        Some(&mut gate),
    )
    .await;
    assert!(mcp.called, "granted remote tool dispatches");
    assert_eq!(out, "remote-tool-ran");

    // Listing a tool as preferred cannot override a human refusal.
    let granted = vec!["incident__create".to_string()];
    let mut mcp = OneRemoteTool::new("incident__create");
    let mut gate = MockGate::new(false, &caveats); // would deny if asked
    run_remote_gated(
        "incident__create",
        ws.path(),
        &caveats,
        Some(&granted),
        &mut mcp,
        Some(&mut gate),
    )
    .await;
    assert!(!mcp.called, "persona preferences cannot override a denial");
    assert_eq!(gate.asks.len(), 1, "preferred tools still need permission");

    // Headless (no gate) → fail-closed: withheld, `call` never runs.
    let mut mcp = OneRemoteTool::new("incident__create");
    let out = run_remote_gated(
        "incident__create",
        ws.path(),
        &caveats,
        Some(&coach),
        &mut mcp,
        None,
    )
    .await;
    assert!(
        !mcp.called,
        "headless must fail closed for an ungranted remote tool"
    );
    assert!(out.contains("permission"), "headless denial: {out}");
}
