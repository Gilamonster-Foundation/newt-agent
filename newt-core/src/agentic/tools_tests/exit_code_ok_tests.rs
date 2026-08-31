//! **#1969: the ledger's `ok` bit must follow the exit code, not a prefix.**
//!
//! `tool_result_ok` classifies a tool result by string prefix (`error:`,
//! `capability denied:`, `unknown tool`). The shell path returns
//! `{stdout}{stderr}` and consulted `exit_code` ONLY when that was empty — so
//! a command that failed loudly, which is every failing compile, produced a
//! non-empty string with no failure prefix and ledgered `ok = true`.
//!
//! Three consumers were blinded by one bit:
//!
//! * the turn's `ToolEvent` ledger, whose `ok` field is this value;
//! * `RepeatCallGuard`, which only memoizes a `Failure` for `!ok`, so the
//!   per-run steer never fired on a repeated failing build;
//! * `loop_watch::repeated_failure` (#1946), which counts `ok == false`.

use super::*;

fn failing_compile_envelope() -> serde_json::Value {
    serde_json::json!({
        "exit_code": 101,
        "stdout": "",
        // Note the shape: cargo says `error[E0308]`, which does NOT match the
        // `error:` prefix `tool_result_ok` looks for. The near-miss is why
        // this went unnoticed.
        "stderr": "error[E0308]: mismatched types\n  --> src/main.rs:4:9\n",
        "timed_out": false,
    })
}

fn render(envelope: &serde_json::Value) -> String {
    shell_envelope_output(envelope, 200, false, false, None, None)
}

#[test]
fn a_failing_compile_with_output_is_recorded_as_a_failure() {
    let out = render(&failing_compile_envelope());
    assert!(
        !tool_result_ok(&out),
        "a command that exited 101 ledgered ok=true; the rendered result was: {out}"
    );
}

#[test]
fn the_compiler_diagnostics_survive_the_failure_marking() {
    let out = render(&failing_compile_envelope());
    assert!(
        out.contains("error[E0308]: mismatched types"),
        "marking the failure discarded the diagnostics the model needs: {out}"
    );
    assert!(
        out.contains("101"),
        "the exit code is the evidence for the failure claim: {out}"
    );
}

/// The twin that stops "everything is a failure now". A successful command
/// with output must stay `ok = true`, or the ledger becomes uniformly
/// pessimistic and the detectors above fire on healthy sessions.
#[test]
fn a_successful_command_with_output_is_still_a_success() {
    let out = render(&serde_json::json!({
        "exit_code": 0,
        "stdout": "    Finished dev [unoptimized] target(s) in 0.04s\n",
        "stderr": "",
        "timed_out": false,
    }));
    assert!(
        tool_result_ok(&out),
        "a successful build was recorded as a failure: {out}"
    );
    assert!(out.contains("Finished dev"), "output was lost: {out}");
}

/// The pre-existing empty-output path keeps its `(exit N)` rendering, and a
/// zero-exit empty result stays a success.
#[test]
fn the_empty_output_path_is_unchanged_for_success_and_failure() {
    let ok_empty = render(&serde_json::json!({
        "exit_code": 0, "stdout": "", "stderr": "", "timed_out": false,
    }));
    assert_eq!(ok_empty, "(exit 0)");
    assert!(tool_result_ok(&ok_empty));

    let bad_empty = render(&serde_json::json!({
        "exit_code": 1, "stdout": "", "stderr": "", "timed_out": false,
    }));
    assert!(
        !tool_result_ok(&bad_empty),
        "an empty failing command must also ledger a failure: {bad_empty}"
    );
}

/// **The bridge, and the reason #1969 had to land before #1946.**
///
/// `loop_watch::repeated_failure` counts `ok == false`. Its own tests plant a
/// synthetic ledger — and a synthetic ledger is only evidence if the real
/// writer produces that shape. Before this fix it did not: every failing
/// compile ledgered `ok = true`, so a detector proven against `ok = false`
/// rows would have been proven against data production never emitted.
///
/// That is the vacuous-green shape one level down, and neither half of an
/// anti-vacuous PAIR catches it, because both halves share the wrong
/// assumption. Only a test that builds its events through the REAL `ok`
/// computation can. So this one does.
#[test]
fn the_real_writer_produces_events_the_thrash_detector_can_see() {
    let envelope = failing_compile_envelope();
    let args = serde_json::json!({"command": "cargo check -p thing", "cwd": "/w"});

    // Exactly how the loop builds a ledger event: render, classify, record.
    let turn = |ms: u64| {
        let rendered = render(&envelope);
        vec![crate::ToolEvent::from_call(
            "run_command",
            &args,
            tool_result_ok(&rendered),
            Some(ms),
        )]
    };
    let turns = vec![turn(3226), turn(14885), turn(9051)];

    assert!(
        turns.iter().all(|t| !t[0].ok),
        "the real writer still records a failing compile as a success"
    );
    let found = crate::loop_watch::repeated_failure(&turns)
        .expect("thrash built from real writer output went unseen");
    assert_eq!(found.tool, "run_command");
    assert_eq!(found.executed_failures, 3);
}

/// The twin, and the counterfactual stated as a test: the SAME three turns as
/// the pre-#1969 writer would have recorded them are invisible to the
/// detector. This is what the session evidence actually contained — 17
/// multi-second failing compiles, all ledgered `ok = 1`.
#[test]
fn the_pre_fix_writer_produced_thrash_no_detector_could_see() {
    let args = serde_json::json!({"command": "cargo check -p thing", "cwd": "/w"});
    let turn = |ms: u64| {
        // `ok = true` — what the prefix test returned for compiler output.
        vec![crate::ToolEvent::from_call(
            "run_command",
            &args,
            true,
            Some(ms),
        )]
    };
    let turns = vec![turn(3226), turn(14885), turn(9051)];
    assert_eq!(
        crate::loop_watch::repeated_failure(&turns),
        None,
        "this must stay invisible: it is the shape the fix exists to end"
    );
}
