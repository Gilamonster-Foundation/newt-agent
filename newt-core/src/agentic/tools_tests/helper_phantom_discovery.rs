use super::*;

// ---- #717: classify_phantom_reach (pure, no fs) ----

#[test]
fn classify_phantom_rewrite_alias() {
    // A shell alias resolves to the canonical run_command rewrite.
    let got = classify_phantom_reach("bash", &serde_json::json!({"command": "ls"}), "ok", true);
    assert_eq!(
        got,
        Some(crate::PhantomResolution::Rewrite("run_command".into()))
    );
}

#[test]
fn classify_phantom_correct_alias() {
    // An edit alias with the wrong arg shape returns Correct guidance.
    let got = classify_phantom_reach(
        "str_replace_editor",
        &serde_json::json!({}),
        "ignored",
        false,
    );
    match got {
        Some(crate::PhantomResolution::Correct(msg)) => {
            assert!(msg.contains("edit_file"), "guidance names the tool: {msg}");
        }
        other => panic!("expected Correct, got {other:?}"),
    }
}

#[test]
fn classify_phantom_unknown_name() {
    // A foreign name with no alias is a true phantom tool. (Note: #716 turned
    // the plan/crew/workflow notions into recognized aliases, so this uses a
    // name no family claims.)
    let got = classify_phantom_reach(
        "summon_kraken",
        &serde_json::json!({}),
        "unknown tool: summon_kraken",
        false,
    );
    assert_eq!(got, Some(crate::PhantomResolution::Unknown));
}

#[test]
fn classify_phantom_plan_alias_is_correct() {
    // #716 + #717: a foreign plan notion now resolves through the alias seam,
    // so the telemetry classifier records it as a Correct (coach) reach — the
    // new arms get phantom-reach telemetry for free.
    let got = classify_phantom_reach("make_plan", &serde_json::json!({}), "ignored", false);
    match got {
        Some(crate::PhantomResolution::Correct(msg)) => {
            assert!(
                msg.contains("update_plan"),
                "guidance names the tool: {msg}"
            );
        }
        other => panic!("expected Correct, got {other:?}"),
    }
}

#[test]
fn classify_phantom_state_get_miss() {
    // state_get on an unset key is an empty-by-design real-tool miss.
    let got = classify_phantom_reach(
        "state_get",
        &serde_json::json!({"key": "nope"}),
        "no such key: nope",
        true,
    );
    assert_eq!(
        got,
        Some(crate::PhantomResolution::RealToolMiss(
            "state_get on an unset key".into()
        ))
    );
}

#[test]
fn classify_phantom_recall_miss() {
    // recall with no hits is an empty-by-design real-tool miss.
    let got = classify_phantom_reach(
        "recall",
        &serde_json::json!({"query": "zzz"}),
        "no matches in past conversations for \"zzz\" — try different keywords",
        true,
    );
    assert_eq!(
        got,
        Some(crate::PhantomResolution::RealToolMiss(
            "recall returned no matches".into()
        ))
    );
}

#[test]
fn classify_phantom_resume_reach_is_a_rewrite() {
    // #714 + #717: a "where were we" reach resolves through the alias seam to
    // a Rewrite, so the telemetry already captures it (no new wiring needed).
    let got = classify_phantom_reach("where_were_we", &serde_json::json!({}), "ignored", false);
    assert_eq!(
        got,
        Some(crate::PhantomResolution::Rewrite("resume_context".into()))
    );
}

#[test]
fn classify_phantom_real_success_is_none() {
    // An ordinary successful real tool call is not phantom telemetry.
    let got = classify_phantom_reach(
        "read_file",
        &serde_json::json!({"path": "src/lib.rs"}),
        "line 1\nline 2\n",
        true,
    );
    assert_eq!(got, None);
}

// ---- #725: tool_search discovery (alias + name registry) ----

#[test]
fn tool_search_is_a_real_tool_name() {
    // It must be in the canonical registry so a model calling it is never
    // treated as a hallucination.
    assert!(ALL_TOOL_NAMES.contains(&"tool_search"));
}

#[test]
fn discovery_verbs_alias_to_tool_search() {
    // The instinctive "which tool does X?" reaches silently Rewrite to the
    // real tool_search.
    for verb in [
        "find_tool",
        "search_tools",
        "list_tools",
        "which_tool",
        "available_tools",
        "what_tools",
        "tools",
    ] {
        match resolve_tool_alias(verb) {
            Some(AliasOutcome::Rewrite(c)) => assert_eq!(c, "tool_search", "verb: {verb}"),
            other => panic!(
                "expected Rewrite(tool_search) for {verb}, got something else: {}",
                other.is_some()
            ),
        }
    }
}

#[test]
fn tool_search_is_not_an_alias_of_itself() {
    // The real name must fall through unchanged (no recursive rewrite).
    assert!(resolve_tool_alias("tool_search").is_none());
}

#[test]
fn classify_phantom_discovery_reach_is_a_rewrite() {
    // #725 + #717: a discovery reach resolves through the alias seam to a
    // Rewrite, so the phantom telemetry captures it for free.
    let got = classify_phantom_reach("find_tool", &serde_json::json!({}), "ignored", false);
    assert_eq!(
        got,
        Some(crate::PhantomResolution::Rewrite("tool_search".into()))
    );
}

#[test]
fn classify_phantom_tool_search_real_call_is_none() {
    // A real tool_search call is not phantom telemetry.
    let got = classify_phantom_reach(
        "tool_search",
        &serde_json::json!({"query": "read"}),
        "Tools matching \"read\":\n- read_file — Read a file",
        true,
    );
    assert_eq!(got, None);
}

#[test]
fn tool_search_is_not_a_hallucination() {
    assert!(!is_hallucination(
        "tool_search",
        &serde_json::json!({"query": "x"})
    ));
}

/// `is_hallucination` correctly identifies tool-name-as-command and unknown
/// tool names, and correctly skips MCP-namespaced tools.
#[test]
fn hallucination_detection_coverage() {
    // tool name passed to run_command → hallucination
    assert!(is_hallucination(
        "run_command",
        &serde_json::json!({"command": "list_dir ."})
    ));
    // normal shell command → not a hallucination
    assert!(!is_hallucination(
        "run_command",
        &serde_json::json!({"command": "cargo test"})
    ));
    // unknown tool → hallucination
    assert!(is_hallucination(
        "definitely_not_a_real_tool",
        &serde_json::json!({})
    ));
    // MCP-namespaced tool → not a hallucination
    assert!(!is_hallucination(
        "my_server__some_tool",
        &serde_json::json!({})
    ));
    // known direct tools → not hallucinations when called correctly
    for t in [
        "list_dir",
        "read_file",
        "write_file",
        "edit_file",
        "delete_file",
        "use_skill",
        "web_fetch",
        "save_note",
        "recall",
    ] {
        assert!(!is_hallucination(t, &serde_json::json!({"path": "."})));
    }
}
