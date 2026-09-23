use super::*;

#[test]
fn lifecycle_unavailable_run_suggests_explicit_build_with_same_phase_and_dir() {
    for dir in [None, Some("nested project")] {
        let mut args = serde_json::json!({"phase": "check", "action": "run"});
        let mut expected = serde_json::json!({"phase": "check", "action": "build"});
        if let Some(dir) = dir {
            args["dir"] = dir.into();
            expected["dir"] = dir.into();
        }
        let original = "error: cargo not in this profile's carried userland";
        let (text, outcome) =
            lifecycle_run_result(&args, (original.into(), crate::ExecOutcome::Unavailable));
        assert_eq!(outcome, crate::ExecOutcome::Unavailable);
        assert!(text.starts_with(original), "{text}");
        let suggestion = text
            .lines()
            .find_map(|line| line.strip_prefix("Suggested lifecycle call: "))
            .expect("unavailable run should expose the explicit build request");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(suggestion).unwrap(),
            expected
        );
        assert!(text.contains("explicit approval"), "{text}");
        assert!(text.contains("network denied"), "{text}");
        assert!(text.contains("denials remain binding"), "{text}");
        assert!(
            !text.contains("no command ran"),
            "a prior compound-command stage may have run"
        );
    }
}

#[test]
fn lifecycle_run_coaching_preserves_denials_and_executed_outcomes() {
    for outcome in [
        crate::ExecOutcome::Denied,
        crate::ExecOutcome::Failed,
        crate::ExecOutcome::Passed,
    ] {
        let original = ("original tool result".to_string(), outcome);
        assert_eq!(
            lifecycle_run_result(&serde_json::json!({"phase":"check"}), original.clone()),
            original,
        );
    }
}

/// F11: `lifecycle action=run` (the default) times out on the same 60s wall
/// as `run_command`. The confined shell already appends the build-lane
/// suggestion at the END of a (possibly truncated) envelope — measured
/// (newt main a996fb9e) not to steer the model. Put the exact next call
/// FIRST, ahead of the partial output, not just at the end.
#[test]
fn lifecycle_timed_out_run_puts_build_call_first() {
    let args = serde_json::json!({"phase": "test"});
    let original = (
        "partial output\n(the command hit the 60s wall and was killed...)".to_string(),
        crate::ExecOutcome::TimedOut,
    );
    let (text, outcome) = lifecycle_run_result(&args, original.clone());
    assert_eq!(outcome, crate::ExecOutcome::TimedOut);
    let first_line = text.lines().next().expect("non-empty result");
    let suggestion = first_line
        .strip_prefix("Suggested lifecycle call: ")
        .expect("build call must be the first line of a timed-out run result");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(suggestion).unwrap(),
        serde_json::json!({"phase": "test", "action": "build"})
    );
    assert!(
        text.ends_with(&original.0),
        "original (partial) output must be preserved: {text}"
    );
}

/// F12 (verify-lane-steering round 2): a model that learned "use lifecycle
/// action=build" sends `phase="build"` instead — `build` is an action, not
/// a phase, so `Phase::from_key` rejects it and it fell into the generic
/// "unknown lifecycle phase" refusal with no pointer to the real call. The
/// refusal must carry the same `{"phase":...,"action":"build"}` suggestion
/// as the timed-out-run coaching (`lifecycle_run_result`) — one JSON source,
/// not a second copy of the shape.
#[tokio::test]
async fn lifecycle_phase_build_points_at_action_build() {
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({ "phase": "build" });
    let out = execute_tool(
        "lifecycle",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(
        out.starts_with("error: unknown lifecycle phase 'build'"),
        "{out}"
    );
    let suggestion = out
        .lines()
        .find_map(|line| line.strip_prefix("Suggested lifecycle call: "))
        .expect("must point at the real action=build call: {out}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(suggestion).unwrap(),
        serde_json::json!({"phase": "test", "action": "build"})
    );
}

/// F19 (red first): measured in replay 2483-r7 — nine `lifecycle action=run`
/// calls died at the 60s wall before one `action=build` call passed. The
/// timeout note already named the exact next call (F11); the model kept
/// retrying `run` anyway. `escalated_after_timeout` is what labels the
/// ONE re-run this fix routes into the build lane instead — it must say
/// plainly that the call escalated and why, and must preserve whatever the
/// build-lane re-run actually returned (pass, fail, or a fresh refusal),
/// never silently fall back to the original timeout text.
#[test]
fn escalated_after_timeout_labels_the_reroute_and_preserves_the_build_result() {
    for (build_result, outcome) in [
        ("  ✓ build check passed".to_string(), crate::ExecOutcome::Passed),
        (
            "capability denied: lifecycle action=build requires explicit confined build authority; no command ran".to_string(),
            crate::ExecOutcome::Denied,
        ),
    ] {
        let (text, out) = escalated_after_timeout((build_result.clone(), outcome));
        assert_eq!(out, outcome, "the build lane's own outcome is preserved");
        assert!(
            text.contains("action=run") && text.contains("60s wall"),
            "must say plainly it escalated and why: {text}"
        );
        assert!(
            text.ends_with(&build_result),
            "the build lane's real result must survive verbatim: {text}"
        );
    }
}

/// #2541 round 2 item 3: the escalation's decision, isolated from formatting
/// and table-tested against all five `ExecOutcome` variants — only a genuine
/// `TimedOut` justifies spending the build lane's authority on a second run.
#[test]
fn escalates_only_on_a_genuine_timeout() {
    for (outcome, expected) in [
        (crate::ExecOutcome::TimedOut, true),
        (crate::ExecOutcome::Passed, false),
        (crate::ExecOutcome::Failed, false),
        (crate::ExecOutcome::Denied, false),
        (crate::ExecOutcome::Unavailable, false),
    ] {
        assert_eq!(
            escalates(outcome),
            expected,
            "escalates({outcome:?}) should be {expected}"
        );
    }
}

/// #2541 round 2 item 1 (red first): when the escalation itself does not
/// execute — `Denied` (including a frame-isolation refusal, which
/// `run_confined_build_lane` also classes `Denied`) or `Unavailable` — the
/// FIRST run's result is the ONLY record of which test hung, and it was being
/// silently dropped in favor of the escalation's bare refusal text. It must
/// survive, composed through the SAME `lifecycle_run_result(args, first)` the
/// non-escalating path already uses (see item 4: this is what keeps that
/// function's `TimedOut` arm alive), with the refusal appended — never lost.
#[test]
fn a_refused_escalation_keeps_the_first_runs_evidence() {
    let args = serde_json::json!({"phase": "test"});
    let first = (
        "partial: test_foo hung\n(the command hit the 60s wall and was killed...)".to_string(),
        crate::ExecOutcome::TimedOut,
    );
    for (refusal, outcome) in [
        (
            "capability denied: lifecycle action=build requires explicit confined build authority; no command ran".to_string(),
            crate::ExecOutcome::Denied,
        ),
        (
            "Error: frame isolation: tool authority exceeds the frame's ceiling".to_string(),
            crate::ExecOutcome::Denied,
        ),
        (
            "error: build workspace: No such file or directory".to_string(),
            crate::ExecOutcome::Unavailable,
        ),
    ] {
        let (text, out) = escalation_result(&args, first.clone(), (refusal.clone(), outcome));
        assert_eq!(out, outcome, "the refusal's own class is the final outcome");
        assert!(
            text.contains("partial: test_foo hung"),
            "the first run's evidence (which test hung) must survive: {text}"
        );
        assert!(
            text.contains(&refusal),
            "the escalation's refusal must still be visible: {text}"
        );
    }
}

/// #2541 round 2 item 1, executing arm: when the escalation DOES run (any
/// outcome but `Denied`/`Unavailable`), its own result already speaks for the
/// whole call — `first`'s partial timeout output is superseded, not
/// duplicated, exactly as before this round's fix.
#[test]
fn an_executed_escalation_supersedes_the_first_runs_partial_output() {
    let args = serde_json::json!({"phase": "test"});
    let first = (
        "partial: test_foo hung\n(the command hit the 60s wall and was killed...)".to_string(),
        crate::ExecOutcome::TimedOut,
    );
    let build_result = "  ✓ build check passed".to_string();
    let (text, out) = escalation_result(
        &args,
        first,
        (build_result.clone(), crate::ExecOutcome::Passed),
    );
    assert_eq!(out, crate::ExecOutcome::Passed);
    assert!(
        !text.contains("test_foo hung"),
        "an executed escalation's own result supersedes the first run's partial output: {text}"
    );
    assert!(text.ends_with(&build_result), "{text}");
}

/// #2541 round 2 item 5 (red first): a build-authority prompt that fires
/// because `action=run` escalated must say so — otherwise the operator sees a
/// `lifecycle action=build` permission request out of a call they read as
/// `action=run` and has no idea why.
#[test]
fn the_escalation_names_itself_in_the_permission_prompt() {
    let base = crate::confined_exec::workspace_confined_caveats(std::path::Path::new("/ws"));
    let direct = lifecycle_build_request("/ws", "cargo test --offline", &base, None);
    assert!(
        !direct.reason.to_lowercase().contains("escalat"),
        "a direct action=build call names no escalation: {}",
        direct.reason
    );
    let escalated = lifecycle_build_request(
        "/ws",
        "cargo test --offline",
        &base,
        Some("lifecycle action=run hit the confined shell's 60s wall"),
    );
    assert!(
        escalated.reason.contains("escalation")
            && escalated.reason.contains("action=run")
            && escalated.reason.contains("60s wall"),
        "the prompt must name why a build-authority request appeared: {}",
        escalated.reason
    );
}

/// #894 regression for the concrete drift that motivated the registry: the
/// `lifecycle` tool (#891) is advertised + dispatched, so it MUST be a real
/// name — otherwise every legitimate `lifecycle` call is miscounted as a
/// hallucination (inflating the anti-loop counter). Before the registry it
/// was missing from `ALL_TOOL_NAMES`; the derivation makes that impossible.
#[test]
fn lifecycle_is_a_real_tool_name_not_a_hallucination() {
    assert!(
        ALL_TOOL_NAMES.contains(&"lifecycle"),
        "lifecycle must be a real tool name"
    );
    assert!(
        !is_hallucination("lifecycle", &serde_json::json!({"phase": "test"})),
        "a real lifecycle call must not be flagged as a hallucination"
    );
}

#[test]
fn lifecycle_definition_enum_matches_phase_vocabulary() {
    // The schema's phase enum is built from `Phase::ALL`, so it can never
    // drift from the vocabulary the executor parses with `Phase::from_key`.
    let def = lifecycle_tool_definition();
    assert_eq!(def["function"]["name"], "lifecycle");
    let enum_vals: Vec<&str> = def["function"]["parameters"]["properties"]["phase"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let vocab: Vec<&str> = crate::tooling::Phase::ALL
        .iter()
        .map(|p| p.as_str())
        .collect();
    assert_eq!(enum_vals, vocab);
}

#[test]
fn run_phase_aliases_route_to_lifecycle() {
    for a in ["run_phase", "run_lifecycle", "lifecycle_run"] {
        assert!(
            matches!(
                resolve_tool_alias(a),
                Some(AliasOutcome::Rewrite("lifecycle"))
            ),
            "{a} should rewrite to lifecycle"
        );
    }
    // The canonical name is NOT an alias — it dispatches directly.
    assert!(resolve_tool_alias("lifecycle").is_none());
}

#[tokio::test]
async fn lifecycle_unknown_phase_lists_valid_phases() {
    // An unknown phase returns before any fs/subprocess touch, so this is a
    // fully-mocked unit test.
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({ "phase": "deploy" });
    let out = execute_tool(
        "lifecycle",
        &args,
        ".",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(
        out.starts_with("error: unknown lifecycle phase 'deploy'"),
        "{out}"
    );
    assert!(out.contains("check"), "should name valid phases: {out}");
}

/// Regression: a model that learned `lifecycle` as a task-state reporter in
/// another harness calls it with `{event|status|state, message}` and no
/// `phase`. Before the fix it got `error: unknown lifecycle phase ''` plus a
/// list of build phases — a dead end that says nothing about where task
/// state actually lives. It must instead be coached to update_plan /
/// request_user_input / the final answer. Returns before any fs touch, so
/// this is a fully-mocked unit test.
#[tokio::test]
async fn lifecycle_task_state_args_are_coached_to_update_plan() {
    let caveats = crate::caveats::Caveats::top();
    for args in [
        serde_json::json!({ "event": "complete", "message": "done" }),
        serde_json::json!({ "status": "blocked" }),
        serde_json::json!({ "state": "in_progress" }),
    ] {
        let out = execute_tool(
            "lifecycle",
            &args,
            ".",
            false,
            20,
            &caveats,
            &mut NoMcp,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(out.starts_with("error:"), "{args}: {out}");
        assert!(
            !out.contains("unknown lifecycle phase"),
            "{args}: still the phase dead-end: {out}"
        );
        for tool in ["update_plan", "request_user_input"] {
            assert!(out.contains(tool), "{args}: should coach to {tool}: {out}");
        }
    }
}

/// #1972 red-first, reproduced against this repo's own real tree (no
/// tempfile): `crates/` carries no lifecycle markers of its own, but its
/// child `crates/newt-tuner/` has a real `Cargo.toml` — the same shape as
/// the reported bug's `agent-voice/Cargo.toml`, invisible to root-anchored
/// detection before this fix. `workspace` is relative to `cargo test`'s cwd
/// (this crate's own directory), so `../crates` is the repo's real
/// `crates/` dir. Closes the loop end to end: the nested project is named
/// (not silently dropped), the message is honest (not `error:`-prefixed),
/// and the no-op no longer ledgers as a claimable success.
#[tokio::test]
async fn lifecycle_root_empty_names_a_nested_project_instead_of_a_silent_noop() {
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({ "phase": "test" });
    let out = execute_tool(
        "lifecycle",
        &args,
        "../crates",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    assert!(
        out.starts_with("no command configured for lifecycle phase 'test'"),
        "{out}"
    );
    assert!(
        out.contains("newt-tuner"),
        "names the nested project: {out}"
    );
    assert!(out.contains("dir=\"<path>\""), "points at the fix: {out}");
    assert!(
        !out.starts_with("error:"),
        "an honest degrade is not a fake failure: {out}"
    );
    assert!(
        !tool_result_ok(&out),
        "a no-op must not ledger as a claimable success: {out}"
    );
}

/// Twin of the above: `dir` resolves detection AND execution against the
/// SAME real nested project directly — proving the resolve_exec_cwd reuse
/// (#1972 part 1). `action=list` keeps this subprocess-free; `ok=true`
/// confirms a genuinely resolved phase is unaffected by the no-op
/// classifier added for the case above.
#[tokio::test]
async fn lifecycle_dir_param_resolves_a_nested_project_directly() {
    let caveats = crate::caveats::Caveats::top();
    let args = serde_json::json!({ "phase": "test", "action": "list", "dir": "newt-tuner" });
    let out = execute_tool(
        "lifecycle",
        &args,
        "../crates",
        false,
        20,
        &caveats,
        &mut NoMcp,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    assert_eq!(out, "lifecycle test → cargo test", "got: {out}");
    assert!(
        tool_result_ok(&out),
        "a genuinely resolved phase is still ok=true: {out}"
    );
}

#[test]
fn run_build_check_reports_pass_fail_and_spawn_error() {
    let ws = tempfile::TempDir::new().unwrap();
    let ws_str = ws.path().to_string_lossy();

    // build_check now runs CONFINED through `ConstrainedExecutor` (P4). On the
    // normative Linux+Landlock platform the trivial commands run under the
    // fence, so we assert the exact confined pass/fail. Off it, the outcome
    // depends on the platform's kernel backend — Windows AppContainer / macOS
    // Seatbelt may confine-and-run, or the spawn fails closed — and BOTH are
    // secure (the executor never runs the repo-controlled command unconfined).
    // So off Linux we assert only a well-formed outcome, never the specific
    // one; the strong confinement guarantee is proven by the real-resource
    // Landlock test (`tests/confined_exec_landlock.rs`).
    //
    // `kernel_fs_fence_available()` is used (not `cfg!() &&
    // agent_bridle::landlock_is_supported()`): that symbol is Linux-only, so
    // calling it under a runtime `cfg!()` fails to COMPILE off Linux.
    let passed = run_build_check(passing_build_check_cmd(), &ws_str);
    if crate::confined_exec::kernel_fs_fence_available() {
        // Under the DenyAll egress floor the trivial command runs confined via
        // the net guard — resolved as a sibling `newt-net-guard` in a dev/test
        // build, or by `newt __net-guard` self-exec in production. In a minimal
        // build layout where the guard binary is not present the spawn fails
        // CLOSED (a secure outcome), so accept either the confined pass or the
        // fail-closed refusal; assert the fail path only when the pass path ran.
        if passed == "  ✓ build check passed" {
            let failed = run_build_check(&failing_build_check_cmd("boom"), &ws_str);
            assert!(failed.contains("✗ build check failed"), "got: {failed}");
            assert!(failed.contains("boom"), "stderr excerpt shown: {failed}");
        } else {
            assert!(
                passed.contains("⚠ build check could not run"),
                "with the egress floor, build_check must confine-and-run or fail \
                     closed, got: {passed}"
            );
        }
    } else {
        assert!(
            passed == "  ✓ build check passed" || passed.contains("⚠ build check could not run"),
            "off Linux, build_check must confine-and-run or fail closed, got: {passed}"
        );
    }
    // A nonexistent workspace dir → the command can't even spawn/confine.
    let err = run_build_check(passing_build_check_cmd(), "/definitely/not/a/dir");
    assert!(err.contains("⚠ build check could not run"), "got: {err}");
}

#[test]
fn lifecycle_build_authority_is_explicit_and_does_not_widen_shell_grants() {
    let base = crate::confined_exec::workspace_confined_caveats(std::path::Path::new("/ws"));
    let request = lifecycle_build_request("/ws", "cargo test --offline", &base, None);
    assert_eq!(request.kind, DenialKind::Build);
    assert_eq!(request.target, "/ws");
    assert!(request.reason.contains("cargo test --offline"));
    assert!(request.reason.contains("network denied"));
    assert!(
        !request.reason.contains("escalation"),
        "a direct action=build call names no escalation: {}",
        request.reason
    );
    assert_eq!(
        crate::agentic::permissions::widen_caveats(&base, &[(request.kind, request.target)]),
        base
    );
    assert_eq!(
        lifecycle_tool_definition()["function"]["parameters"]["properties"]["action"]["enum"],
        serde_json::json!(["run", "list", "build"])
    );
}
