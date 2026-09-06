use super::*;

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
