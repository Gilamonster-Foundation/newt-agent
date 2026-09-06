use super::*;

#[test]
fn use_skill_tool_is_advertised_in_definitions() {
    let defs = tool_definitions();
    let names: Vec<&str> = defs
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert!(names.contains(&"use_skill"), "got: {names:?}");
}

#[test]
fn merged_tool_definitions_with_empty_mcp_is_builtin_set() {
    let merged = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    let names: Vec<&str> = merged
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "run_command",
            "read_file",
            "write_file",
            "edit_file",
            "delete_file",
            "list_dir",
            "find",
            "use_skill",
            "web_fetch",
            // #721: advertised ALWAYS (core capability-grant request, no
            // presence gate) — part of the base tool_definitions() set.
            "request_permissions",
            // #714: advertised ALWAYS (no presence gate), so it joins the
            // base set even with every `with_*` flag off.
            "resume_context",
            // Exact prompt recovery is an invariant, independent of the
            // optional general-memory disclosure surface.
            "prompt_read",
            // Prompt-rooted work recovery is equally invariant and
            // always present, even before any artifact has been written.
            "artifact_read",
            // #725: advertised ALWAYS (a discovery tool must always be
            // present), so it too joins the base set with every flag off.
            "tool_search",
            // #727: advertised ALWAYS (read-only budget self-read, no
            // presence gate), pushed right after resume_context.
            "get_context_remaining",
            // #728: advertised ALWAYS (a model must always be able to ask the
            // human; degrades honestly headless), pushed last.
            "request_user_input",
            // #891: advertised ALWAYS (the model-facing lifecycle surface;
            // degrades honestly with "no command configured"), pushed after
            // request_user_input.
            "lifecycle",
            // #1004: advertised ALWAYS (present-findings surface; needs no
            // injected capability, degrades to raw source when color is
            // off), pushed after lifecycle.
            "render_report",
            // #1285: advertised ALWAYS (a read-only navigation utility like
            // tool_search; degrades honestly when no symbol index is built),
            // pushed after render_report.
            "where_is",
            // #1387 Code Navigator — Always-gated structural/lexical tools
            // (degrade when session indexes are absent).
            "goto_definition",
            "text_search",
            "find_references",
            "find_tests",
            "find_callers",
            "find_callees",
            "find_implementations",
            "find_hierarchy",
            "inspect_type",
            "impact",
        ]
    );
}

/// #894: each registry entry's schema-builder produces the SAME name the
/// entry declares — catches a copy-paste where the `ToolSpec.name` and the
/// `*_tool_definition()` disagree.
#[test]
fn registry_specs_match_their_definition_names() {
    for spec in EXTENDED_TOOL_REGISTRY {
        let def = (spec.definition)();
        assert_eq!(
            def["function"]["name"].as_str(),
            Some(spec.name),
            "ToolSpec name {:?} != definition name",
            spec.name
        );
    }
}

/// #894: no built-in tool name is declared twice across the base array and
/// the registry (a dup would double-advertise and confuse dispatch).
#[test]
fn builtin_tool_names_are_unique() {
    let mut seen = std::collections::HashSet::new();
    for name in ALL_TOOL_NAMES.iter() {
        assert!(seen.insert(*name), "duplicate built-in tool name: {name}");
    }
}

/// #894 anti-drift (the payoff): with EVERY gate on, the advertised set from
/// `merged_tool_definitions` equals `ALL_TOOL_NAMES` in BOTH directions. This
/// is the test that would have caught the `lifecycle` drift — a tool
/// advertised/dispatched but missing from the real-name set (or vice versa)
/// fails here.
#[test]
fn advertised_set_matches_all_tool_names_both_directions() {
    let all = merged_tool_definitions(
        &NoMcp, true, true, true, true, true, true, true, true, true, true, true, true,
    );
    let advertised: std::collections::HashSet<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    let names: std::collections::HashSet<&str> = ALL_TOOL_NAMES.iter().copied().collect();
    // Every advertised tool is a real (non-hallucinated) name...
    for a in &advertised {
        assert!(
            names.contains(a),
            "advertised but not in ALL_TOOL_NAMES: {a}"
        );
    }
    // ...and every real name is actually advertised when its gate is on.
    for n in &names {
        assert!(
            advertised.contains(n),
            "in ALL_TOOL_NAMES but never advertised: {n}"
        );
    }
}

/// #894: `BASE_TOOL_NAMES` mirrors the names inlined in `tool_definitions()`
/// exactly and in order — the one hand-kept mirror, guarded here.
#[test]
fn base_tool_names_match_tool_definitions() {
    let defs = tool_definitions();
    let base: Vec<&str> = defs
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["function"]["name"].as_str())
        .collect();
    assert_eq!(base, BASE_TOOL_NAMES);
}

/// `save_note` is sink-gated: absent from the base `tool_definitions`
/// (headless/eval callers see no memory tool) and from the merged set
/// without a sink; present in the merged set when a sink exists.
#[test]
fn save_note_advertised_only_with_a_sink() {
    fn names(defs: &serde_json::Value) -> Vec<&str> {
        defs.as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect()
    }
    // Headless/eval callers see no memory tool in the base set …
    let base = tool_definitions();
    assert!(!names(&base).contains(&"save_note"), "got: {base}");
    // … nor in the merged set without a sink …
    let without = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(!names(&without).contains(&"save_note"));
    // … but a sink advertises it.
    let with = merged_tool_definitions(
        &NoMcp, true, false, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(names(&with).contains(&"save_note"), "got: {with}");
}

/// `recall` is source-gated exactly like `save_note` is sink-gated
/// (Step 17.5): absent from the base set and from the merged set
/// without a source; present when one exists.
#[test]
fn recall_advertised_only_with_a_source() {
    fn names(defs: &serde_json::Value) -> Vec<&str> {
        defs.as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect()
    }
    let base = tool_definitions();
    assert!(!names(&base).contains(&"recall"), "got: {base}");
    let without = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(!names(&without).contains(&"recall"));
    let with = merged_tool_definitions(
        &NoMcp, false, true, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(names(&with).contains(&"recall"), "got: {with}");
    // The two gates are independent: both on advertises both.
    let both = merged_tool_definitions(
        &NoMcp, true, true, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(names(&both).contains(&"save_note"));
    assert!(names(&both).contains(&"recall"));
}

/// `memory_fetch` is source-gated exactly like `recall` (#319): absent
/// from the base set and from the merged set without a `MemorySource`;
/// present when one exists. The flag is independent of the others.
#[test]
fn memory_fetch_advertised_only_with_a_source() {
    fn names(defs: &serde_json::Value) -> Vec<&str> {
        defs.as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect()
    }
    let base = tool_definitions();
    assert!(!names(&base).contains(&"memory_fetch"), "got: {base}");
    // Flag off (every existing caller, the inert default) → not advertised.
    let without = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(!names(&without).contains(&"memory_fetch"));
    // Flag on → advertised.
    let with = merged_tool_definitions(
        &NoMcp, false, false, true, false, false, false, false, false, false, false, false, false,
    );
    assert!(names(&with).contains(&"memory_fetch"), "got: {with}");
    // Independent of the save_note / recall gates: all three on lists all.
    let all = merged_tool_definitions(
        &NoMcp, true, true, true, false, false, false, false, false, false, false, false, false,
    );
    assert!(names(&all).contains(&"save_note"));
    assert!(names(&all).contains(&"recall"));
    assert!(names(&all).contains(&"memory_fetch"));
}

// --- PR4: the `git` tool is presence-gated -----------------------------

#[test]
fn git_tool_advertised_only_with_the_presence_gate() {
    fn names(defs: &serde_json::Value) -> Vec<&str> {
        defs.as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect()
    }
    let with = merged_tool_definitions(
        &NoMcp, false, false, false, true, false, false, false, false, false, false, false, false,
    );
    assert!(names(&with).contains(&"git"), "with_git advertises git");
    let without = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(!names(&without).contains(&"git"), "no git without the gate");
    // #479: the /team toggle advertises both crew tools, and only then.
    let team = merged_tool_definitions(
        &NoMcp, false, false, false, false, true, false, false, false, false, false, false, false,
    );
    assert!(
        names(&team).contains(&"crew") && names(&team).contains(&"compose_roster"),
        "with_team advertises crew + compose_roster"
    );
    assert!(
        !names(&without).contains(&"crew"),
        "no crew without the gate"
    );
    // Step 26.4 (#583): the scratchpad state tools, only with the gate on.
    let scratch = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, true, false, false, false, false, false, false,
    );
    for t in ["state_set", "state_get", "state_clear"] {
        assert!(
            names(&scratch).contains(&t),
            "{t} advertised with_scratchpad"
        );
        assert!(!names(&without).contains(&t), "{t} hidden without the gate");
        assert!(
            !is_hallucination(t, &serde_json::json!({})),
            "{t} is a real tool"
        );
    }
    // Step 26.5.5 (#582): the code_search tool, only with its gate on.
    let code = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, true, false, false, false, false, false,
    );
    assert!(
        names(&code).contains(&"code_search"),
        "code_search advertised"
    );
    assert!(
        !names(&without).contains(&"code_search"),
        "code_search hidden without the gate"
    );
    assert!(!is_hallucination("code_search", &serde_json::json!({})));
    // Step 26.6a (#585): the experiential record/recall tools, only with the gate.
    let exp = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, true, false, false, false, false,
    );
    for t in ["experience_record", "experience_recall"] {
        assert!(names(&exp).contains(&t), "{t} advertised with_experiential");
        assert!(!names(&without).contains(&t), "{t} hidden without the gate");
        assert!(
            !is_hallucination(t, &serde_json::json!({})),
            "{t} is a real tool"
        );
    }
    // Step 26.6b (#586) / #715 PR2: the scheduled update_plan + plan_get tools,
    // only with the gate (plan_set/plan_advance collapsed into update_plan).
    let sched = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, true, false, false, false,
    );
    for t in ["update_plan", "plan_get"] {
        assert!(names(&sched).contains(&t), "{t} advertised with_scheduled");
        assert!(!names(&without).contains(&t), "{t} hidden without the gate");
        assert!(
            !is_hallucination(t, &serde_json::json!({})),
            "{t} is a real tool"
        );
    }
    for t in ["enter_plan_mode", "exit_plan_mode"] {
        assert!(
            !names(&sched).contains(&t),
            "{t} needs a session Plan control as well as the scheduled ledger"
        );
    }
    let plan_control_only = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, true, false,
    );
    assert!(
        !names(&plan_control_only).contains(&"enter_plan_mode"),
        "enter_plan_mode needs scheduled planning as well as the session control"
    );
    assert!(
        !names(&plan_control_only).contains(&"exit_plan_mode"),
        "an inactive control must not advertise an unnecessary exit"
    );
    let active_plan_control = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, true, true,
    );
    assert!(
        !names(&active_plan_control).contains(&"enter_plan_mode"),
        "enter still requires the scheduled ledger"
    );
    assert!(
        names(&active_plan_control).contains(&"exit_plan_mode"),
        "an active Plan phase must keep exit available if scheduled planning is toggled off"
    );
    let plan_ready_inactive = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, true, false, true, false,
    );
    assert!(
        names(&plan_ready_inactive).contains(&"enter_plan_mode"),
        "scheduled planning plus a control advertises enter"
    );
    assert!(
        names(&plan_ready_inactive).contains(&"exit_plan_mode"),
        "a frozen multi-round catalog that advertises enter must also advertise same-turn exit"
    );
    let plan_mode = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, true, false, true, true,
    );
    for t in ["enter_plan_mode", "exit_plan_mode"] {
        assert!(
            names(&plan_mode).contains(&t),
            "{t} is advertised when both required seams are present"
        );
    }
    // `/mode auto`: the model-facing selector exists only when the
    // session injects its bounded next-turn control.
    let operating_mode = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, true, false, false,
    );
    assert!(
        names(&operating_mode).contains(&"select_operating_mode"),
        "auto-mode control advertises its selector"
    );
    assert!(
        !names(&without).contains(&"select_operating_mode"),
        "selector is hidden outside /mode auto"
    );
    assert!(!is_hallucination(
        "select_operating_mode",
        &serde_json::json!({})
    ));
}
