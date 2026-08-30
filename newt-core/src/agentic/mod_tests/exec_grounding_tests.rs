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
