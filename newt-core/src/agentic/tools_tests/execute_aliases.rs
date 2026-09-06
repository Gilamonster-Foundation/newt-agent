use super::*;

#[tokio::test]
async fn unknown_tool_name_is_reported_not_executed() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "definitely_not_a_tool",
        serde_json::json!({}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    // Step 27.1: the bare "unknown tool: X" is now a corrective message that
    // still leads with the same prefix but also names the real catalog.
    assert!(
        out.starts_with("unknown tool: definitely_not_a_tool"),
        "got: {out}"
    );
    assert!(out.contains("Available tools include:"), "got: {out}");
}

// -- Step 27.1: tool-alias resolution + corrective feedback -------------

#[test]
fn alias_rewrites_shell_names_to_run_command() {
    for n in [
        "execute",
        "exec",
        "bash",
        "shell",
        "sh",
        "zsh",
        "terminal",
        "run_shell_command",
        "shell_command",
        "system",
    ] {
        assert!(
            matches!(
                resolve_tool_alias(n),
                Some(AliasOutcome::Rewrite("run_command"))
            ),
            "{n} should rewrite to run_command"
        );
    }
}

#[test]
fn alias_corrects_edit_and_create_names() {
    for n in [
        "str_replace_editor",
        "str_replace",
        "apply_patch",
        "edit",
        "replace_in_file",
    ] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("edit_file"), "{n}: {msg}");
        assert!(msg.contains("write_file"), "{n}: {msg}");
    }
    for n in ["create_file", "new_file", "touch"] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("write_file"), "{n}: {msg}");
    }
    for n in ["remove_file", "delete", "remove", "unlink", "rm_file"] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("delete_file"), "{n}: {msg}");
        assert!(msg.contains("fs_write"), "{n}: {msg}");
    }
}

#[test]
fn alias_coaches_mkdir_to_write_file() {
    // #721: newt has no directory-creation tool — coach to write_file, which
    // does create_dir_all on the parent. Turns the issue's `mkdir -p …/src`
    // dead-end into a self-correcting tool call.
    for n in [
        "mkdir",
        "make_dir",
        "makedirs",
        "mkdirs",
        "create_dir",
        "create_directory",
    ] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("write_file"), "{n}: {msg}");
        assert!(msg.contains("create_dir_all"), "{n}: {msg}");
    }
    // `touch` is intentionally NOT in the mkdir arm — it stays a create-file
    // alias (→ write_file), so there is no duplicate match arm / collision.
    let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias("touch") else {
        panic!("touch should still be a create-file Correct outcome");
    };
    assert!(msg.contains("write_file"), "touch: {msg}");
}

#[test]
fn alias_passes_through_real_and_mcp_names() {
    for n in [
        "run_command",
        "read_file",
        "write_file",
        "edit_file",
        "delete_file",
        "git",
        "update_plan",
        "plan_get",
        "server__do_thing",
    ] {
        assert!(
            resolve_tool_alias(n).is_none(),
            "{n} must dispatch unchanged"
        );
    }
}

// -- #716: plan / plan-read / crew / workflow alias families --------------

#[test]
fn alias_corrects_plan_names_to_update_plan() {
    // #1193: enter_plan_mode / exit_plan_mode are now REAL tools (a
    // read-only plan phase), so they no longer coach to update_plan — they
    // dispatch. The plan-CONTENT verbs still coach to update_plan.
    for n in [
        "make_plan",
        "create_plan",
        "plan",
        "planning",
        "todo",
        "todos",
        "todo_write",
    ] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("update_plan"), "{n}: {msg}");
    }
    // The phase verbs are real tools now — NOT aliases.
    for n in ["enter_plan_mode", "exit_plan_mode"] {
        assert!(
            resolve_tool_alias(n).is_none(),
            "{n} is a real tool, not an alias"
        );
    }
    // #715 PR2: the advance-ish verbs coach update_plan + "completed" too.
    for n in [
        "next_step",
        "complete_step",
        "finish_step",
        "mark_done",
        "step_done",
    ] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("update_plan"), "{n}: {msg}");
        assert!(msg.contains("completed"), "{n}: {msg}");
    }
    // #715 PR2: update_plan is the REAL tool now → not an alias (returns None),
    // exactly like the resume_context fix; the old set_plan name is gone too.
    assert!(
        resolve_tool_alias("update_plan").is_none(),
        "update_plan must dispatch as the real tool, not a self-alias"
    );
}

#[test]
fn alias_rewrites_plan_read_names_to_plan_get() {
    for n in [
        "get_plan",
        "show_plan",
        "read_plan",
        "current_plan",
        "what_was_i_doing",
    ] {
        assert!(
            matches!(
                resolve_tool_alias(n),
                Some(AliasOutcome::Rewrite("plan_get"))
            ),
            "{n} should rewrite to plan_get"
        );
    }
}

#[test]
fn alias_rewrites_resume_reaches_to_resume_context() {
    // #714: the instinctive "where did we leave off" reaches redirect to the
    // self-recovery tool, not plan_get.
    for n in [
        "resume",
        "where_were_we",
        "where_did_we_leave_off",
        "catch_me_up",
        "recap",
    ] {
        assert!(
            matches!(
                resolve_tool_alias(n),
                Some(AliasOutcome::Rewrite("resume_context"))
            ),
            "{n} should rewrite to resume_context"
        );
    }
    // The REAL tool name is not an alias: it returns None so a direct
    // resume_context call dispatches as a real tool and is NOT logged as a
    // phantom Rewrite by #717 telemetry (real names must return None).
    assert!(
        resolve_tool_alias("resume_context").is_none(),
        "the real tool name must return None, not a self-Rewrite"
    );
    // No regression: `what_was_i_doing` still asks specifically for the plan.
    assert!(
        matches!(
            resolve_tool_alias("what_was_i_doing"),
            Some(AliasOutcome::Rewrite("plan_get"))
        ),
        "what_was_i_doing must stay → plan_get"
    );
}

#[test]
fn alias_corrects_crew_names_and_flags_team_gating() {
    for n in [
        "delegate",
        "spawn_agent",
        "subagent",
        "sub_agent",
        "crew_dispatch",
        "run_crew",
        "dispatch_crew",
        "fork_agent",
        "assign",
        "team",
    ] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        // Names the real targets...
        assert!(msg.contains("compose_roster"), "{n}: {msg}");
        assert!(msg.contains("crew"), "{n}: {msg}");
        // ...but makes clear the model cannot self-enable the /team surface.
        assert!(msg.contains("/team"), "{n}: {msg}");
        assert!(
            msg.contains("human enables") || msg.contains("cannot turn it on yourself"),
            "crew correction must not imply the model can invoke it: {msg}"
        );
    }
}

#[test]
fn alias_corrects_workflow_names_to_plan_plus_crew() {
    for n in ["workflow", "run_workflow", "start_workflow", "pipeline"] {
        let Some(AliasOutcome::Correct(msg)) = resolve_tool_alias(n) else {
            panic!("{n} should produce a Correct outcome");
        };
        assert!(msg.contains("no workflow tool"), "{n}: {msg}");
        assert!(msg.contains("update_plan"), "{n}: {msg}");
    }
}

#[test]
fn levenshtein_matches_known_distances() {
    assert_eq!(levenshtein("kitten", "sitting"), 3);
    assert_eq!(levenshtein("read_file", "read_file"), 0);
    assert_eq!(levenshtein("read_fil", "read_file"), 1);
    assert_eq!(levenshtein("", "abc"), 3);
}

#[test]
fn nearest_tool_name_suggests_close_only() {
    assert_eq!(nearest_tool_name("read_fil"), Some("read_file"));
    assert_eq!(nearest_tool_name("edit_fil"), Some("edit_file"));
    assert_eq!(nearest_tool_name("memory_fetchh"), Some("memory_fetch"));
    assert_eq!(nearest_tool_name("definitely_not_a_tool"), None);
}

#[test]
fn unknown_tool_message_names_catalog_and_suggestion() {
    let m = unknown_tool_message("read_fil");
    assert!(m.starts_with("unknown tool: read_fil"), "{m}");
    assert!(m.contains("Did you mean 'read_file'"), "{m}");
    assert!(m.contains("Available tools include:"), "{m}");

    let m2 = unknown_tool_message("zzzzzzzzzzzz");
    assert!(m2.starts_with("unknown tool: zzzzzzzzzzzz"), "{m2}");
    assert!(!m2.contains("Did you mean"), "{m2}");
    assert!(m2.contains("Available tools include:"), "{m2}");
}

/// An incompatible-arg alias is corrected (not dead-ended) by execute_tool:
/// a model that emits `str_replace_editor` is told to use edit_file. The
/// correction returns before any fs/caveat work, so this is deterministic.
#[tokio::test]
async fn execute_tool_corrects_str_replace_editor_alias() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = caveats_rw(ws.path());
    let out = run_tool(
        "str_replace_editor",
        serde_json::json!({"command": "str_replace", "path": "f.txt"}),
        ws.path(),
        &caveats,
        None,
    )
    .await;
    assert!(out.contains("edit_file"), "got: {out}");
    assert!(!out.starts_with("unknown tool"), "got: {out}");
}

#[test]
fn get_context_remaining_is_a_real_tool_not_a_phantom() {
    // #727: real, always-advertised, no-arg budget read — never treated as
    // an alias of itself or a hallucination.
    assert!(resolve_tool_alias("get_context_remaining").is_none());
    assert!(ALL_TOOL_NAMES.contains(&"get_context_remaining"));
    assert!(classify_phantom_reach(
        "get_context_remaining",
        &serde_json::json!({}),
        "Context budget: ~10 tokens used of an input ceiling of ~80 (80% of num_ctx 100).",
        true,
    )
    .is_none());
    // The always-advertised def rides in every session (empty MCP).
    let defs = merged_tool_definitions(
        &NoMcp, false, false, false, false, false, false, false, false, false, false, false, false,
    );
    assert!(defs
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["function"]["name"] == "get_context_remaining"));
}

#[test]
fn budget_verbs_rewrite_to_get_context_remaining() {
    // #727: the instinctive "how much context is left" reaches all resolve
    // to the canonical no-arg read (safe silent Rewrite — matching arg shape).
    for n in [
        "context_remaining",
        "tokens_left",
        "remaining_tokens",
        "budget",
        "how_much_context",
        "context_budget",
        "token_budget",
    ] {
        assert!(
            matches!(
                resolve_tool_alias(n),
                Some(AliasOutcome::Rewrite("get_context_remaining"))
            ),
            "{n} must rewrite to get_context_remaining"
        );
        // A Rewrite alias is mined by the #717 telemetry as a Rewrite.
        assert!(
            is_context_remaining_call(n),
            "{n} must be recognized as a budget call by the loop"
        );
    }
    // The canonical name is recognized by the loop but is NOT an alias.
    assert!(is_context_remaining_call("get_context_remaining"));
    assert!(resolve_tool_alias("get_context_remaining").is_none());
    // An unrelated name is neither.
    assert!(!is_context_remaining_call("read_file"));
}
