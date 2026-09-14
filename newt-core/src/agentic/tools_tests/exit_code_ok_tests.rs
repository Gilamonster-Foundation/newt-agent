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

// -----------------------------------------------------------------------
// #1972: the lifecycle tool's honest no-op degrade must not ledger `ok =
// true` either — the sibling gap to #1969 (a different string shape, same
// "did real work happen?" question the `ok` bit answers).
// -----------------------------------------------------------------------

/// Red-first: before #1972, `unconfigured_phase_message`'s plain no-op text
/// carried no failure-shaped prefix `tool_result_ok` recognized, so it
/// ledgered `ok = true` — indistinguishable from a phase that actually ran.
#[test]
fn lifecycle_unconfigured_noop_is_not_ledgered_as_success() {
    let msg = crate::tooling::unconfigured_phase_message(crate::tooling::Phase::Test, &[]);
    assert!(
        !tool_result_ok(&msg),
        "an honest no-op must not ledger as a claimable success: {msg}"
    );
    assert!(
        !msg.starts_with("error:"),
        "and it must not be misrepresented as a failure either: {msg}"
    );
}

/// Same, for the actionable (nested-project-named) shape — it is still a
/// no-op (the phase did not run), even though it is more useful than the
/// plain wording.
#[test]
fn lifecycle_actionable_noop_is_not_ledgered_as_success() {
    let msg = crate::tooling::unconfigured_phase_message(
        crate::tooling::Phase::Test,
        &[std::path::PathBuf::from("agent-voice")],
    );
    assert!(
        !tool_result_ok(&msg),
        "naming a candidate directory is still not a completed run: {msg}"
    );
}

/// The twin that stops "everything lifecycle-shaped is a no-op now": a
/// resolved command's `list` output (a real, claimable answer) stays
/// `ok = true`.
#[test]
fn lifecycle_resolved_list_output_is_still_a_success() {
    assert!(tool_result_ok("lifecycle test → cargo test"));
}

// ---------------------------------------------------------------------------
// #2315: the structured execution class. Each class comes from the envelope
// branch that renders the result, never from the rendered text, and the
// `ok` bit above is left exactly as it was.
// ---------------------------------------------------------------------------

use crate::ExecOutcome;

fn confined_with(
    cmd: &str,
    envelope: serde_json::Value,
    caveats: &crate::caveats::Caveats,
) -> (String, ExecOutcome) {
    super::shell::confined_result(cmd, &envelope, caveats, false, render)
}

fn confined(cmd: &str, envelope: serde_json::Value) -> (String, ExecOutcome) {
    confined_with(cmd, envelope, &crate::caveats::Caveats::top())
}

fn host(cmd: &str, envelope: serde_json::Value) -> (String, ExecOutcome) {
    super::shell::host_result(cmd, &envelope, render)
}

fn envelope(exit_code: i64, stdout: &str, stderr: &str, timed_out: bool) -> serde_json::Value {
    serde_json::json!({"exit_code": exit_code, "stdout": stdout, "stderr": stderr,
        "timed_out": timed_out})
}

/// The four failure shapes the issue names each get a DISTINCT class, and
/// every one still ledgers `ok = false` exactly as before.
#[test]
fn failure_shapes_get_distinct_classes_and_the_ok_bit_is_unchanged() {
    let denied = serde_json::json!({"exit_code": 126, "stdout": "", "stderr": "",
        "denied": true, "denials": [{"kind": "exec", "target": "curl",
        "reason": "exec of curl is not permitted"}]});
    let cases = [
        (
            confined("cargo test", failing_compile_envelope()),
            ExecOutcome::Failed,
        ),
        (
            confined("cargo test", envelope(124, "partial", "", true)),
            ExecOutcome::TimedOut,
        ),
        (confined("curl example.test", denied), ExecOutcome::Denied),
        (
            confined(
                "definitely-absent-2315 --version",
                envelope(
                    127,
                    "",
                    "command not found: definitely-absent-2315\n",
                    false,
                ),
            ),
            ExecOutcome::Unavailable,
        ),
    ];
    for ((text, class), expected) in &cases {
        assert_eq!(class, expected, "{text}");
        assert!(
            !tool_result_ok(text),
            "the ok bit changed for {class:?}: {text}"
        );
    }
    assert!(cases[3].0 .0.contains(super::ABSENT_BINARY_MARKER));
}

/// Twin for the timeout: exit 124 without the flag is a check that failed on
/// its own terms (GNU `timeout` exits 124), not a harness timeout.
#[test]
fn exit_124_without_the_timeout_flag_is_failed() {
    assert_eq!(
        confined("timeout 1 cargo test", envelope(124, "", "", false)).1,
        ExecOutcome::Failed
    );
    assert_eq!(
        host("cargo test", envelope(124, "", "", false)).1,
        ExecOutcome::Failed
    );
    assert_eq!(
        host("cargo test", envelope(124, "", "", true)).1,
        ExecOutcome::TimedOut
    );
}

/// Trap: a second classifier over the rendered text. A successful command
/// whose own output starts with `error:` is the case where the text and the
/// envelope disagree; the class follows the envelope while `ok` keeps its
/// documented prefix misread.
#[test]
fn the_class_follows_the_envelope_not_the_rendered_text() {
    let (text, class) = confined(
        "cat log",
        envelope(0, "error: quoted from a log\n", "", false),
    );
    assert!(!tool_result_ok(&text), "{text}");
    assert_eq!(class, ExecOutcome::Passed);
    let (text, class) = host("cat log", envelope(0, "fine\n", "", false));
    assert!(tool_result_ok(&text));
    assert_eq!(class, ExecOutcome::Passed);
}

/// The host lane has no absent-binary refusal, so resolution decides: a
/// program that does not resolve is unavailable, and its twin, a program that
/// exists and exits 127, failed. Both render and ledger exactly as before.
#[cfg(unix)]
#[test]
fn host_lane_127_is_unavailable_only_when_the_program_does_not_resolve() {
    let (text, class) = host(
        "definitely-absent-2315 --version",
        envelope(127, "", "sh: 1: definitely-absent-2315: not found\n", false),
    );
    assert_eq!(class, ExecOutcome::Unavailable, "{text}");
    assert!(text.starts_with("error: command exited 127"), "{text}");
    let (text, class) = host("sh -c 'exit 127'", envelope(127, "", "", false));
    assert_eq!(class, ExecOutcome::Failed, "{text}");
    assert!(!tool_result_ok(&text));
    assert_eq!(
        host("cargo test", failing_compile_envelope()).1,
        ExecOutcome::Failed
    );
}

/// The kernel refusing a program outside the fs-read grant is a denial.
#[cfg(unix)]
#[test]
fn a_kernel_refused_binary_is_denied() {
    let no_reads = crate::caveats::Caveats {
        fs_read: crate::caveats::Scope::none(),
        ..crate::caveats::Caveats::top()
    };
    let (text, class) = confined_with(
        "sh -c true",
        envelope(
            126,
            "",
            "failed to execute command 'sh': Permission denied\n",
            false,
        ),
        &no_reads,
    );
    assert_eq!(class, ExecOutcome::Denied, "{text}");
    assert!(text.starts_with("capability denied:"), "{text}");
}

/// Non-shell events serialize without the field, so their persisted bytes are
/// unchanged; a shell event names its class in snake_case.
#[test]
fn only_shell_events_carry_the_execution_field() {
    let plain = crate::ToolEvent::from_call("read_file", &serde_json::json!({}), true, None);
    assert!(!serde_json::to_string(&plain).unwrap().contains("execution"));
    let shell = crate::ToolEvent {
        execution: Some(ExecOutcome::TimedOut),
        ..plain
    };
    assert_eq!(
        serde_json::to_value(&shell).unwrap()["execution"],
        "timed_out"
    );
}
