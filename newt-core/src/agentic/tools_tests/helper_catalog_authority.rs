use super::*;

#[test]
fn exit_plan_mode_result_appends_mandatory_edit_only_when_tenacity_requires_it() {
    use crate::tenacity::Tenacity;
    // Advisory levels: plain result, no forcing directive.
    for t in [Tenacity::Relaxed, Tenacity::Standard] {
        let out = exit_plan_mode_result(t);
        assert!(out.starts_with("exited the model-entered PLAN PHASE"));
        assert!(
            !out.contains("must be a concrete"),
            "{t} must not force an edit: {out}"
        );
    }
    // Forcing levels: the mandatory-edit directive is appended.
    for t in [Tenacity::Insistent, Tenacity::Relentless] {
        let out = exit_plan_mode_result(t);
        assert!(out.contains("now EXECUTE it"), "{t}: {out}");
        assert!(out.contains("must be a concrete"), "{t}: {out}");
        assert!(out.contains("edit_file or write_file"), "{t}: {out}");
    }
    // The two sets agree with the level's own predicate.
    for t in Tenacity::all() {
        assert_eq!(
            exit_plan_mode_result(t).contains("now EXECUTE it"),
            t.exit_plan_requires_edit()
        );
    }
}

/// FR-1 part 2 (#997): a persona's `tools:` allow-list scopes the ADVERTISED
/// catalog — only the named tools survive, PLUS the always-on infra tools the
/// loop can't run without (which no persona may fence off). `None` leaves the
/// catalog whole (the zero-cost path for every non-persona session).
#[test]
fn persona_allow_list_filters_the_advertised_catalog() {
    let full = merged_tool_definitions(
        &NoMcp, true, true, true, true, true, true, true, true, true, true, true, true,
    );
    let name_set = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .filter_map(|d| d["function"]["name"].as_str().map(str::to_owned))
            .collect()
    };
    // No persona → catalog untouched.
    assert_eq!(
        name_set(&filter_advertised_tools(full.clone(), None)),
        name_set(&full),
        "None must be a no-op"
    );
    // A read-only coach (`tools = ["read_file"]`): read_file survives; the
    // mutating built-ins are dropped; every always-on infra tool still rides.
    let allow = vec!["read_file".to_string()];
    let got = name_set(&filter_advertised_tools(full, Some(&allow)));
    assert!(got.iter().any(|n| n == "read_file"), "granted tool kept");
    for denied in [
        "write_file",
        "edit_file",
        "delete_file",
        "run_command",
        "list_dir",
    ] {
        assert!(
            !got.iter().any(|n| n == denied),
            "{denied} must be filtered out"
        );
    }
    for infra in [
        "resume_context",
        "prompt_read",
        "tool_search",
        "get_context_remaining",
        "request_user_input",
        "lifecycle",
        "select_operating_mode",
    ] {
        assert!(
            got.iter().any(|n| n == infra),
            "{infra} is session infrastructure and must survive any persona"
        );
    }
}

/// FR-1 part 2 (#997): `persona_tool_allowed` is the single predicate behind
/// BOTH the advertise-filter and the executor reject — a tool is callable iff
/// the persona names it OR it is always-on infra — so the set the model sees
/// and the set it may run can never drift apart.
#[test]
fn persona_tool_allowed_admits_named_and_always_on_only() {
    let allow = vec!["read_file".to_string()];
    assert!(persona_tool_allowed("read_file", &allow), "named → allowed");
    assert!(
        persona_tool_allowed("request_user_input", &allow),
        "always-on infra → allowed even when unlisted"
    );
    assert!(
        persona_tool_allowed("select_operating_mode", &allow),
        "presence-gated session control → allowed even when unlisted"
    );
    assert!(
        !persona_tool_allowed("write_file", &allow),
        "unlisted non-infra → denied"
    );
    assert!(
        !persona_tool_allowed("delete_file", &allow),
        "unlisted non-infra → denied"
    );
}

/// Prompt disposition is an independent, fail-closed catalog boundary:
/// non-Act turns retain only explicit read/recovery tools, so a generic MCP
/// name cannot appear merely because its schema was connected to the session.
#[test]
fn prompt_disposition_filters_catalog_and_unknown_names_fail_closed() {
    let defs = serde_json::json!([
        { "type": "function", "function": { "name": "read_file" } },
        { "type": "function", "function": { "name": "write_file" } },
        { "type": "function", "function": { "name": "run_command" } },
        { "type": "function", "function": { "name": "update_plan" } },
        { "type": "function", "function": { "name": "exit_plan_mode" } },
        { "type": "function", "function": { "name": "select_operating_mode" } },
        { "type": "function", "function": { "name": "request_permissions" } },
        { "type": "function", "function": { "name": "incident__read" } },
        { "not": "a callable definition" }
    ]);
    let names = |defs: &serde_json::Value| {
        defs.as_array()
            .unwrap()
            .iter()
            .filter_map(|def| def["function"]["name"].as_str())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };

    let research = filter_tools_for_disposition(defs.clone(), PromptDisposition::Research);
    assert_eq!(names(&research), vec!["read_file", "select_operating_mode"]);
    let plan = filter_tools_for_disposition(defs.clone(), PromptDisposition::Plan);
    assert_eq!(
        names(&plan),
        vec![
            "read_file",
            "update_plan",
            "exit_plan_mode",
            "select_operating_mode"
        ]
    );
    assert!(tool_allowed(PromptDisposition::Explain, "read_file"));
    assert!(tool_allowed(PromptDisposition::Plan, "update_plan"));
    assert!(!tool_allowed(PromptDisposition::Explain, "update_plan"));
    assert!(!tool_allowed(PromptDisposition::Research, "update_plan"));
    assert!(tool_allowed(PromptDisposition::Plan, "exit_plan_mode"));
    assert!(
        !tool_allowed(PromptDisposition::Plan, "web_fetch"),
        "offline Plan must not advertise a tool its caveat always denies"
    );
    assert!(
        tool_allowed(PromptDisposition::Research, "web_fetch"),
        "Research (including Diagnose) may gather remote read-only evidence"
    );
    assert!(tool_allowed(
        PromptDisposition::Research,
        "select_operating_mode"
    ));
    assert!(tool_allowed(
        PromptDisposition::Plan,
        "select_operating_mode"
    ));
    assert!(!tool_allowed(
        PromptDisposition::Ask,
        "select_operating_mode"
    ));
    assert!(!tool_allowed(PromptDisposition::Explain, "write_file"));
    assert!(!tool_allowed(PromptDisposition::Research, "incident__read"));
    assert!(!tool_allowed(PromptDisposition::Ask, "read_file"));
    assert!(tool_allowed(PromptDisposition::Act, "incident__write"));
    // #1258: `find` carries the size column (sort=size/show_size), so an
    // evidence-only turn answers "largest files" through it — pin that it
    // stays in the Explain/Research set (guards against a future move to a
    // gated tool that would re-box the diagnosed session).
    assert!(tool_allowed(PromptDisposition::Explain, "find"));
    assert!(tool_allowed(PromptDisposition::Research, "find"));
    // #1387 / line-count lock-in: Research must also keep `find`, AND the
    // advertised schema must teach `sort=lines` + `show_lines`. Losing either
    // re-opens the double-bind (Research admits find but can't answer lines
    // → model dumps or reaches for `wc -l` → empty/denied).
    let research_catalog = filter_tools_for_disposition(
        merged_tool_definitions(
            &NoMcp, false, false, false, false, false, false, false, false, false, false, false,
            false,
        ),
        PromptDisposition::Research,
    );
    let find_def = research_catalog
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["function"]["name"].as_str() == Some("find"))
        .expect("Research must advertise find");
    let props = &find_def["function"]["parameters"]["properties"];
    assert!(
        props.get("show_lines").is_some(),
        "Research find schema must expose show_lines: {find_def}"
    );
    assert!(
        props.get("code").is_some(),
        "Research find schema must expose code (source-only filter): {find_def}"
    );
    let desc = find_def["function"]["description"].as_str().unwrap_or("");
    assert!(
        desc.contains("category") && desc.contains("source"),
        "find description must teach category=source for source rankings: {desc}"
    );
    // #1406: GFM-table response steering moved out of the tool description
    // into the prompt-intake layer (see prompt_intake.rs
    // `*_steers_*_markdown_table` tests); the description no longer carries it.
    let sort_enum = props["sort"]["enum"]
        .as_array()
        .expect("sort must be an enum");
    assert!(
        sort_enum.iter().any(|v| v.as_str() == Some("lines")),
        "Research find sort enum must include 'lines': {sort_enum:?}"
    );
    assert!(
        props.get("category").is_some() && props.get("language").is_some(),
        "Research find schema must teach the harness source category + language filter: \
             {find_def}"
    );
    assert!(
        find_def["function"]["description"]
            .as_str()
            .is_some_and(|description| {
                description.contains("repository code investigation")
                    && description.contains("source by default")
            }),
        "the tool catalog must reinforce the standing source-first repository policy: \
             {find_def}"
    );
    // #1259: the formal ask-the-human escalation IS admitted in evidence
    // turns — a boxed-in model ends as a question, not penalized narration…
    assert!(tool_allowed(
        PromptDisposition::Explain,
        "request_user_input"
    ));
    assert!(tool_allowed(
        PromptDisposition::Research,
        "request_user_input"
    ));
    // …but the capability-GRANT path stays excluded: an evidence turn must
    // never mint caveats (the #1259 security boundary, pinned).
    assert!(!tool_allowed(
        PromptDisposition::Explain,
        "request_permissions"
    ));
    assert!(!tool_allowed(
        PromptDisposition::Research,
        "request_permissions"
    ));
    assert_eq!(
        filter_tools_for_disposition(
            serde_json::json!({ "not": "a catalog" }),
            PromptDisposition::Research
        ),
        serde_json::json!([]),
        "a non-Act catalog with no enumerable tool names must fail closed"
    );

    // Act is the compatibility/default path: it preserves definitions,
    // including an opaque extension definition the disposition filter cannot
    // classify by name.
    assert_eq!(
        filter_tools_for_disposition(defs.clone(), PromptDisposition::Act),
        defs
    );
}

/// FR-1 part 2 (#997): the executor is the ENFORCEMENT half. Even a
/// hallucinated call the advertise-filter can't intercept is refused BY NAME
/// before any side effect — while a granted tool and the always-on infra
/// pass. Regression for a coach persona whose `tools:` list must be a real
/// boundary, not a cosmetic hint.
#[tokio::test]
async fn executor_refuses_tools_outside_the_persona_allow_list() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = crate::caveats::Caveats::top();
    let allow = vec!["read_file".to_string()];
    // write_file is NOT granted → refused with the persona message, and the
    // file is never written (top caveats would otherwise permit it).
    let target = ws.path().join("blocked.txt");
    let args = serde_json::json!({
        "path": target.to_string_lossy(),
        "content": "should never be written",
    });
    let out = call_offload("write_file", &args, &ws, &caveats, Some(&allow)).await;
    assert!(
        out.contains("not available under the active persona"),
        "expected persona refusal, got: {out}"
    );
    assert!(!target.exists(), "a denied write must not touch the fs");
    // An always-on infra tool rides even though it is unlisted.
    let infra = call_offload(
        "get_context_remaining",
        &serde_json::json!({}),
        &ws,
        &caveats,
        Some(&allow),
    )
    .await;
    assert!(
        !infra.contains("not available under the active persona"),
        "always-on infra must not be refused: {infra}"
    );
}

/// FR-3 (#998): the absolute deny-list is wired into the executor and is
/// GRANT-INDEPENDENT — even with top caveats and NO persona, a `run_command`
/// whose exec target is forbidden (`ssh`) is refused before the shell runs,
/// while an ordinary command is untouched. Guards against the deny module
/// being present but never called.
#[tokio::test]
async fn executor_enforces_the_absolute_deny_list() {
    let ws = tempfile::TempDir::new().unwrap();
    let caveats = crate::caveats::Caveats::top(); // maximal grant — deny still bites
    let denied = call_offload(
        "run_command",
        &serde_json::json!({ "command": "ssh host 'uptime'" }),
        &ws,
        &caveats,
        None, // no persona — the floor is independent of any grant
    )
    .await;
    assert!(
        denied.contains("absolute deny-list"),
        "ssh must hit the deny-list, got: {denied}"
    );
    // A benign command sails past the deny gate (it reaches normal exec).
    let ok = call_offload(
        "run_command",
        &serde_json::json!({ "command": "echo coaching" }),
        &ws,
        &caveats,
        None,
    )
    .await;
    assert!(
        !ok.contains("absolute deny-list"),
        "an ordinary command must not be denied, got: {ok}"
    );
}

/// Test-only thin wrapper over the 22-arg [`execute_tool_with_offload`] that
/// fixes every optional seam to `None` and surfaces just the persona list.
async fn call_offload(
    name: &str,
    args: &serde_json::Value,
    ws: &tempfile::TempDir,
    caveats: &crate::caveats::Caveats,
    persona_tools: Option<&[String]>,
) -> String {
    execute_tool_with_offload(
        name,
        args,
        &ws.path().to_string_lossy(),
        false,
        20,
        caveats,
        &mut NoMcp,
        None,  // build_check_cmd
        None,  // note_sink
        None,  // recall_source
        None,  // memory_source
        None,  // permission_gate
        None,  // exec_floor
        None,  // git_tool
        None,  // crew_runner
        None,  // scratchpad_store
        None,  // code_search
        None,  // where_is
        None,  // experience_store
        None,  // step_ledger
        false, // tool_offload
        None,  // spill_store
        persona_tools,
    )
    .await
}
