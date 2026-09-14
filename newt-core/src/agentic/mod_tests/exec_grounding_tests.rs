use super::*;

#[test]
fn blocker_detector_requires_tool_name_and_denial_language() {
    assert!(looks_like_unverified_run_command_blocker(
        "I hit a capability wall: run_command is permission-denied (exec not granted)."
    ));
    assert!(!looks_like_unverified_run_command_blocker(
        "The build is blocked, but I have not tested the shell yet."
    ));
    assert!(!looks_like_unverified_run_command_blocker(
        "run_command completed successfully."
    ));
}

#[test]
fn only_an_actual_denial_result_grounds_a_denial_claim() {
    assert!(run_command_result_is_denial(
        "run_command",
        false,
        "error: exec of cargo is not within the granted authority"
    ));
    assert!(!run_command_result_is_denial(
        "run_command",
        true,
        "command completed successfully"
    ));
    assert!(!run_command_result_is_denial(
        "read_file",
        false,
        "capability denied"
    ));
}

#[test]
fn grounding_requires_advertisement_no_attempt_and_a_spare_round() {
    let tools = serde_json::json!([{
        "type": "function",
        "function": {"name": "run_command"}
    }]);
    let claim = "run_command is permission denied; I cannot run the build";
    assert!(should_ground_unverified_run_command_blocker(
        claim, &tools, false, true, 0, true
    ));
    assert!(!should_ground_unverified_run_command_blocker(
        claim, &tools, false, true, 0, false
    ));
}

/// #2304: the confined lane's refusal of a binary the host HAS coaches an
/// `exec:<abs>` grant, so it is a grant gap — a blocker report grounded in it
/// must not draw the "claim is unsupported" nudge. The not-installed variant
/// is not a grant gap (no grant can supply it) and grounds nothing.
#[test]
fn an_absent_binary_grant_gap_grounds_a_denial_claim() {
    let on_host = format!(
        "error: cargo: {}.\n  granted host binaries: (none)\n  \
         ask the operator for exec:/usr/bin/cargo, or run the host lane.",
        tools::ABSENT_BINARY_MARKER
    );
    assert!(run_command_result_is_denial("run_command", false, &on_host));
    let not_installed = format!(
        "error: cargo: {}, and {}.\n  granted host binaries: (none)",
        tools::ABSENT_BINARY_MARKER,
        tools::NOT_ON_HOST_MARKER
    );
    assert!(!run_command_result_is_denial(
        "run_command",
        false,
        &not_installed
    ));
}

/// #2304: `exec not granted` is model-prose vocabulary that no tool emits, so
/// a result carrying it is not newt's own denial.
#[test]
fn model_prose_denial_wording_is_not_a_denial_result() {
    assert!(!run_command_result_is_denial(
        "run_command",
        false,
        "error: command exited 1\nexec not granted"
    ));
}
