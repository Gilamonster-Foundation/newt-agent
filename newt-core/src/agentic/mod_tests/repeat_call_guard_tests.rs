use super::*;

#[test]
fn loop_owned_tool_results_share_one_complete_spill_block() {
    let mut repeated = Vec::new();
    {
        let mut display = display::ToolDisplay::new(&mut repeated, false, 80, 3, false);
        present_synthetic_tool_result(
            &mut display,
            "find",
            &serde_json::json!({"path": ".", "name": "*.rs", "type": "f"}),
            std::path::Path::new("."),
            "a.rs\nb.rs\nc.rs\nd.rs",
        );
    }
    // #1973 declared amendment: this golden MOVED from tail-only
    // (b.rs/c.rs/d.rs) to head+tail (a.rs .. d.rs) — see the module doc on
    // `display::spill_view_lines` for why tail-only is a defect, not a
    // style choice. This test's own property (one complete spill block per
    // result, not split across a repeat-call boundary) is unaffected.
    assert_eq!(
        String::from_utf8(repeated).unwrap(),
        "⚙  find: . (name=*.rs, type=f)\n\
             ▒ a.rs\n\
             ▲ 2 lines hidden  [/spill N raises this view]\n\
             ▓ d.rs\n\
             …\n"
    );

    let mut budget = Vec::new();
    {
        let mut display = display::ToolDisplay::new(&mut budget, false, 80, 3, false);
        present_synthetic_tool_result(
            &mut display,
            "tokens_left",
            &serde_json::json!({}),
            std::path::Path::new("."),
            "context budget: 75% remaining",
        );
    }
    assert_eq!(
        String::from_utf8(budget).unwrap(),
        "⚙  get_context_remaining: \n\
             ▒ context budget: 75% remaining\n\
             …\n"
    );
}

/// disclosure-gate-live-path (#5): the repeat-steer synthetic message is a
/// model-ingress path (re-injected as a `{"role":"tool"}` turn) that
/// interpolates the first line of a FAILED tool result. A registered session
/// secret that lands in that line must be value-filtered before the steer
/// reaches the model — the convergence-audit falsification. This FAILS on the
/// pre-fix code (the raw secret is interpolated verbatim).
#[test]
fn repeat_steer_value_filters_a_registered_session_secret() {
    let secret = "CANARY-repeatsteer-9f3a2b71";
    let mut filter = crate::ocap::DisclosureFilter::new();
    filter.register(secret);
    let _guard = crate::ocap::scoped_session_disclosure(filter);

    let mut g = RepeatCallGuard::default();
    let args = serde_json::json!({"command": "cat /etc/token"});
    // The tool FAILS with the secret echoed in the first line of its result.
    g.record(
        "run_command",
        &args,
        false,
        &format!("error: unexpected token {secret} in response"),
    );
    // The model repeats the exact call → the steer re-injects the prior line.
    let steer = g
        .repeat_steer("run_command", &args)
        .expect("steers on repeat");
    assert!(
        !steer.contains(secret),
        "the repeat-steer message leaked a registered session secret: {steer}"
    );
    assert!(
        steer.contains("[REDACTED]"),
        "the secret should be redacted in place: {steer}"
    );
    // The steer is still useful (names the tool + the do-not-repeat guidance).
    assert!(steer.contains("already called"), "{steer}");
}

#[test]
fn short_circuits_exact_repeat_and_escalates() {
    let mut g = RepeatCallGuard::default();
    let args = serde_json::json!({"command": "mkdir x"});
    // First sight of the call → let it run (no steer).
    assert!(g.repeat_steer("run_command", &args).is_none());
    // After a failure, an exact repeat is steered, quoting the prior error.
    g.record("run_command", &args, false, "error: shell unavailable");
    let s = g.repeat_steer("run_command", &args).expect("repeat steers");
    assert!(s.contains("already called"), "{s}");
    assert!(s.contains("error: shell unavailable"), "{s}");
    assert!(
        !s.contains("stop using"),
        "one failure → no escalation yet: {s}"
    );
    // A second (distinct-args) failure of the same tool crosses ESCALATE_AFTER.
    g.record(
        "run_command",
        &serde_json::json!({"command": "ls"}),
        false,
        "error: denied",
    );
    let s2 = g.repeat_steer("run_command", &args).expect("still steers");
    assert!(s2.contains("stop using"), "escalates: {s2}");
}

#[test]
fn ignores_successes_and_distinct_calls() {
    let mut g = RepeatCallGuard::default();
    let a = serde_json::json!({"path": "f.rs"});
    g.record("read_file", &a, true, "file contents"); // success → not remembered
    assert!(g.repeat_steer("read_file", &a).is_none());
    // A failure under different args does not short-circuit a distinct call.
    let b = serde_json::json!({"path": "g.rs"});
    g.record("read_file", &b, false, "error reading g.rs");
    assert!(
        g.repeat_steer("read_file", &a).is_none(),
        "distinct args still run"
    );
    assert!(g.repeat_steer("read_file", &b).is_some());
}

#[test]
fn steers_no_result_repeats_on_second_issuance() {
    // #718: a success-shaped no-result that the model re-issues byte-for-byte
    // is steered on its 2nd call — distinct from a hard failure (no escalation),
    // distinct from a genuine success (which is never steered).
    let mut g = RepeatCallGuard::default();

    // recall "no matches" — first sight runs; record it; the identical 2nd
    // issuance is steered before re-execution.
    let q = serde_json::json!({"query": "newt-tui PyO3 bindings"});
    assert!(
        g.repeat_steer("recall", &q).is_none(),
        "first recall must run"
    );
    g.record(
        "recall",
        &q,
        true,
        "no matches in past conversations for \"newt-tui PyO3 bindings\" — try different keywords.",
    );
    let s = g
        .repeat_steer("recall", &q)
        .expect("2nd identical recall steers");
    assert!(s.contains("no matches"), "{s}");
    assert!(
        s.contains("resume_context"),
        "recall steer points at resume_context: {s}"
    );
    assert!(
        !s.contains("stop using"),
        "a no-result is not a hard failure — no escalation: {s}"
    );

    // state_get "no such key" — same: 2nd identical probe is steered.
    let k = serde_json::json!({"key": "current_task"});
    assert!(g.repeat_steer("state_get", &k).is_none());
    g.record("state_get", &k, true, "no such key: current_task");
    assert!(
        g.repeat_steer("state_get", &k).is_some(),
        "2nd identical state_get steers"
    );

    // plan_get empty ledger — same: the second identical read is steered
    // toward creating the missing plan instead of polling the empty ledger.
    let empty_plan_args = serde_json::json!({});
    assert!(g.repeat_steer("plan_get", &empty_plan_args).is_none());
    g.record(
        "plan_get",
        &empty_plan_args,
        true,
        "no active plan — if this is multi-step work, call update_plan next",
    );
    let plan_steer = g
        .repeat_steer("plan_get", &empty_plan_args)
        .expect("2nd identical empty plan_get steers");
    assert!(plan_steer.contains("update_plan"), "{plan_steer}");

    // A genuine success with content is still NEVER steered on repeat.
    let f = serde_json::json!({"path": "f.rs"});
    g.record("read_file", &f, true, "file contents");
    assert!(g.repeat_steer("read_file", &f).is_none());

    // A no-result under DIFFERENT args is a distinct call — let it run.
    let q2 = serde_json::json!({"query": "something else entirely"});
    assert!(
        g.repeat_steer("recall", &q2).is_none(),
        "distinct recall args still run"
    );
}

#[test]
fn steers_duplicate_successful_web_fetch() {
    let mut g = RepeatCallGuard::default();
    let issue = serde_json::json!({
        "url": "https://github.com/Gilamonster-Foundation/newt-agent/issues/771"
    });

    assert!(
        g.repeat_steer("web_fetch", &issue).is_none(),
        "first fetch must run"
    );
    g.record("web_fetch", &issue, true, "# Issue\n\nbody");
    let steer = g
        .repeat_steer("web_fetch", &issue)
        .expect("2nd identical successful fetch steers");
    assert!(steer.contains("already observed"), "{steer}");
    assert!(steer.contains("`web_fetch`"), "{steer}");
    assert!(
        steer.contains("https://github.com/Gilamonster-Foundation/newt-agent/issues/771"),
        "{steer}"
    );
    assert!(
        g.repeat_steer(
            "web_fetch",
            &serde_json::json!({"url": "https://github.com/hartsock/scrybe"})
        )
        .is_none(),
        "distinct URLs still run"
    );

    let file = serde_json::json!({"path": "src/lib.rs"});
    g.record("read_file", &file, true, "file contents");
    assert!(
        g.repeat_steer("read_file", &file).is_none(),
        "ordinary successful reads are still not steered"
    );
}

#[test]
fn steers_duplicate_successful_read_only_run_command() {
    let mut g = RepeatCallGuard::default();
    let args = serde_json::json!({
        "command": "grep -n 'help_lines' /Users/shawnhartsock/workspaces/newt-agent/newt-tui/src/lib.rs"
    });

    assert!(
        g.repeat_steer("run_command", &args).is_none(),
        "first grep should run"
    );
    g.record(
        "run_command",
        &args,
        true,
        "9439:fn help_lines() -> &'static [&'static str] {",
    );

    let steer = g
        .repeat_steer("run_command", &args)
        .expect("second identical grep should steer");
    assert!(steer.contains("already observed"), "{steer}");
    assert!(steer.contains("read-only shell probe"), "{steer}");
    assert!(steer.contains("`run_command`"), "{steer}");
    assert!(steer.contains("grep -n"), "{steer}");
    assert!(steer.contains("Do NOT repeat"), "{steer}");
}

#[test]
fn does_not_steer_successful_write_capable_run_command() {
    let mut g = RepeatCallGuard::default();
    let args = serde_json::json!({"command": "cargo test -p newt-tui"});

    g.record("run_command", &args, true, "test result: ok");

    assert!(
        g.repeat_steer("run_command", &args).is_none(),
        "successful build/test commands are still repeatable"
    );
}

#[test]
fn classifier_leaves_ordinary_successes_repeatable() {
    let file = serde_json::json!({"path": "src/lib.rs"});
    assert_eq!(
        RepeatCallGuard::classify_repeat_memo("read_file", &file, true, "file contents"),
        None
    );

    let tests = serde_json::json!({"command": "cargo test -p newt-core"});
    assert_eq!(
        RepeatCallGuard::classify_repeat_memo("run_command", &tests, true, "test result: ok"),
        None
    );

    let mut g = RepeatCallGuard::default();
    g.record("read_file", &file, true, "file contents");
    g.record("run_command", &tests, true, "test result: ok");
    assert!(
        g.repeat_memos.is_empty(),
        "ordinary successful calls must stay repeatable"
    );
}

#[test]
fn workflow_error_fingerprint_captures_cargo_location() {
    let output = r#"
error[E0425]: cannot find value `SECTION_PROMPT_TOKENS` in this scope
   --> newt-tui/src/help_sections.rs:523:22
    |
523 |         lines: SECTION_PROMPT_TOKENS,
    |                ^^^^^^^^^^^^^^^^^^^^^ help: a static with a similar name exists: `SECTION_PROMPT`
"#;

    let fp = build_error_fingerprint(output).expect("cargo error should fingerprint");

    assert!(fp.contains("newt-tui/src/help_sections.rs:523:22"), "{fp}");
    assert!(fp.contains("error[E0425]"), "{fp}");
    assert!(fp.contains("SECTION_PROMPT_TOKENS"), "{fp}");
}

#[test]
fn tenacity_action_forcing_nudge_fires_at_the_budget_and_resets_on_a_write() {
    // #tenacity: the action-forcing nudge fires once the model has spent the
    // tenacity level's budget of consecutive read-only rounds, and a
    // workspace write resets the counter. This is what gives the OpenAI-chat
    // loop (which had no read-only nudge) a push from reading to acting.
    let mut state = WorkflowRuntimeState {
        tenacity: crate::tenacity::Tenacity::Relentless, // budget 1
        ..Default::default()
    };
    // Nothing spent yet → no nudge.
    assert!(state.action_forcing_nudge(5, None, None).is_none());
    // One read-only round → at the Relentless budget → fires.
    state.record_round_outcome(false, false);
    let nudge = state
        .action_forcing_nudge(5, None, None)
        .expect("relentless tenacity must force action after one read-only round");
    assert!(nudge.contains("edit_file or write_file"), "{nudge}");
    // Firing resets the counter; a follow-up read-only round re-accumulates.
    assert!(state.action_forcing_nudge(5, None, None).is_none());
    state.record_round_outcome(false, false);
    assert!(state.action_forcing_nudge(5, None, None).is_some());
    // A workspace-write round clears the counter entirely.
    state.record_round_outcome(true, true);
    assert!(
        state.action_forcing_nudge(5, None, None).is_none(),
        "a write must reset the read-only streak"
    );

    // Standard tenacity preserves the historical budget of 3.
    let mut standard = WorkflowRuntimeState::default();
    for _ in 0..2 {
        standard.record_round_outcome(false, false);
    }
    assert!(
        standard.action_forcing_nudge(5, None, None).is_none(),
        "standard must not fire before 3 read-only rounds"
    );
    standard.record_round_outcome(false, false);
    assert!(standard.action_forcing_nudge(5, None, None).is_some());
}

#[test]
fn workflow_runtime_nudges_after_error_without_writes() {
    let output = r#"
error[E0425]: cannot find value `SECTION_PROMPT_TOKENS` in this scope
   --> newt-tui/src/help_sections.rs:523:22
"#;
    let mut state = WorkflowRuntimeState::default();

    state.record_tool_result(output);
    state.record_round_outcome(false, false);

    let nudge = state
        .round_start_nudge(None)
        .expect("read-only round after evidence should lock the active repair");
    assert!(nudge.contains("<workflow_state>"), "{nudge}");
    assert!(
        nudge.contains("newt-tui/src/help_sections.rs:523:22"),
        "{nudge}"
    );
    assert!(nudge.contains("next_allowed_actions"), "{nudge}");
    assert!(nudge.contains("disallowed_actions"), "{nudge}");

    let classification = crate::NudgeClassification {
        class: crate::NudgeClass::PlanUpdate,
        score: 1.0,
    };
    let rediscovery = state
        .rediscovery_nudge(
            Some(&classification),
            "Summary of Findings\nRoot Cause: the build failure is still present.",
            None,
        )
        .expect("classified summary should be steered toward action");
    assert!(
        rediscovery.contains("Do not restate findings"),
        "{rediscovery}"
    );
    assert!(
        rediscovery.contains("newt-tui/src/help_sections.rs:523:22"),
        "{rediscovery}"
    );
}

#[test]
fn workflow_runtime_tracks_failed_edit_as_unresolved_evidence() {
    let output = "error: old_string not found in newt-tui/src/help_sections.rs";
    let mut state = WorkflowRuntimeState::default();

    state.record_tool_result(output);
    state.record_round_outcome(false, false);

    let nudge = state
        .round_start_nudge(None)
        .expect("failed edit should remain unresolved repair evidence");
    assert!(nudge.contains("old_string not found"), "{nudge}");

    let grace = state
        .cap_grace_nudge(None, 25, 5)
        .expect("cap after failed edit/read-only recovery should grant an action round");
    assert!(
        grace.contains("configured_workflow_grace_rounds = 5"),
        "{grace}"
    );
    assert!(
        grace.contains("call edit_file or write_file now"),
        "{grace}"
    );
    assert!(
        state.cap_grace_nudge(None, 25, 0).is_none(),
        "configured zero grace disables soft cap extension"
    );

    state.record_round_outcome(true, true);
    let verify = state
        .cap_grace_nudge(None, 25, 3)
        .expect("a successful edit at the cap should get a verification window");
    assert!(verify.contains("focused verification"), "{verify}");
    assert!(
        verify.contains("configured_workflow_grace_rounds = 3"),
        "{verify}"
    );
}

#[test]
fn workflow_runtime_grants_configured_grace_for_recent_plan_progress() {
    let ledger = SessionStepLedger::default();
    ledger.set_plan(&["finish round-cap grace".to_string(), "verify".to_string()]);
    let mut state = WorkflowRuntimeState::default();

    state.record_round_outcome(false, true);

    let nudge = state
        .cap_grace_nudge(Some(&ledger), 2, 4)
        .expect("recent active-plan progress should activate configured grace");
    assert!(
        nudge.contains("configured_workflow_grace_rounds = 4"),
        "{nudge}"
    );
    assert!(nudge.contains("finish round-cap grace"), "{nudge}");
    assert!(
        state.cap_grace_nudge(Some(&ledger), 2, 0).is_none(),
        "zero configured grace keeps the cap hard"
    );
}

/// #<issue>: a diagnostic workflow (e.g. `diagnose_failure.toml`,
/// `progress_horizon_rounds = 6`) legitimately spends more read-only
/// rounds between plan checkpoints than a routine edit does. Without a
/// horizon override, 4 rounds since the last checkpoint already exceeds
/// the shared default (`WORKFLOW_RECENT_PROGRESS_ROUNDS = 3`) and grace
/// does NOT activate — RED on the pre-fix behavior. Setting the override
/// widens the window so the same 4-rounds-stale state still counts as
/// "recent" — GREEN.
#[test]
fn progress_horizon_override_widens_the_recent_progress_window() {
    let ledger = SessionStepLedger::default();
    ledger.set_plan(&["diagnose the failure".to_string(), "fix it".to_string()]);

    let mut default_horizon = WorkflowRuntimeState::default();
    default_horizon.record_round_outcome(false, true); // a checkpoint...
    for _ in 0..4 {
        default_horizon.record_round_outcome(false, false); // ...then 4 idle rounds
    }
    assert!(
        default_horizon
            .cap_grace_nudge(Some(&ledger), 2, 4)
            .is_none(),
        "4 rounds since the last checkpoint exceeds the default 3-round horizon"
    );

    let mut widened = WorkflowRuntimeState::default();
    widened.set_progress_horizon(Some(6));
    widened.record_round_outcome(false, true);
    for _ in 0..4 {
        widened.record_round_outcome(false, false);
    }
    assert!(
        widened.cap_grace_nudge(Some(&ledger), 2, 4).is_some(),
        "a widened 6-round horizon still treats 4-rounds-stale as recent progress"
    );
}

#[test]
fn workspace_write_classifier_is_narrow() {
    assert!(is_workspace_write_call("edit_file"));
    assert!(is_workspace_write_call("write_file"));
    assert!(!is_workspace_write_call("run_command"));
    assert!(!is_workspace_write_call("read_file"));
}

#[test]
fn no_result_reason_classifies_and_routes() {
    // recall / state_get no-result prefixes classify…
    assert!(RepeatCallGuard::no_result_reason(
        "recall",
        "no matches in past conversations for \"x\" — try different keywords."
    )
    .is_some_and(|r| r.contains("no matches") && r.contains("resume_context")));
    assert!(
        RepeatCallGuard::no_result_reason("state_get", "no such key: current_task")
            .is_some_and(|r| r.contains("not set"))
    );
    assert!(
        RepeatCallGuard::no_result_reason("plan_get", "no active plan — call update_plan")
            .is_some_and(|r| r.contains("update_plan"))
    );
    // …a real success with content does not.
    assert!(
        RepeatCallGuard::no_result_reason("recall", "3 match(es) in past conversations").is_none()
    );
    assert!(RepeatCallGuard::no_result_reason("read_file", "file contents").is_none());

    // A recall ERROR (ok=false) goes through the FAILURE path, not no-result
    // classification: it lands in repeat_memos as escalation-eligible.
    let mut g = RepeatCallGuard::default();
    let q = serde_json::json!({"query": "x"});
    g.record("recall", &q, false, "error: index unavailable");
    assert!(matches!(
        g.repeat_memos.get(&RepeatCallGuard::key("recall", &q)),
        Some(RepeatMemo::Failure { first_line }) if first_line == "error: index unavailable"
    ));
}

#[test]
fn first_line_caps_and_takes_first() {
    assert_eq!(first_line("one\ntwo\nthree"), "one");
    assert_eq!(first_line(""), "");
    assert_eq!(first_line(&"x".repeat(500)).chars().count(), 200);
}

/// **#1969's second consumer, repaired for free.**
///
/// `RepeatCallGuard` memoizes a `Failure` only for `!ok`, and `ok` came from
/// a prefix test that read every failing compile as a success. So the guard
/// that exists to stop a model re-issuing a dead call was silently disabled
/// for the single most common failing call in a coding session — the build.
///
/// It is not a change to the guard. It is the guard finally being told the
/// truth about the outcome.
#[test]
fn a_failing_build_now_memoizes_and_steers_the_repeat() {
    let mut g = RepeatCallGuard::default();
    let args = serde_json::json!({"command": "cargo check -p thing", "cwd": "/w"});
    // What the shell path renders for a failing compile since #1969.
    let result = "error: command exited 101\nerror[E0308]: mismatched types\n";
    let ok = crate::agentic::tools::tool_result_ok(result);
    assert!(
        !ok,
        "the outcome bit is still wrong, so this proves nothing"
    );

    assert!(g.repeat_steer("run_command", &args).is_none());
    g.record("run_command", &args, ok, result);
    let steer = g
        .repeat_steer("run_command", &args)
        .expect("a repeated failing build is not steered");
    assert!(steer.contains("already called"), "{steer}");
}

/// The twin: a PASSING build stays repeatable. Builds are re-run constantly
/// and for good reason, so the repair must not memoize success.
#[test]
fn a_passing_build_stays_repeatable() {
    let mut g = RepeatCallGuard::default();
    let args = serde_json::json!({"command": "cargo check -p thing", "cwd": "/w"});
    let result = "    Finished dev [unoptimized] target(s) in 0.04s\n";
    let ok = crate::agentic::tools::tool_result_ok(result);
    assert!(ok, "a successful build is being read as a failure");
    g.record("run_command", &args, ok, result);
    assert!(
        g.repeat_steer("run_command", &args).is_none(),
        "a passing build was memoized; re-running a build is legitimate"
    );
}
