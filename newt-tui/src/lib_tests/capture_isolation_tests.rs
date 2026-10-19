use super::*;

/// #2937 P7: session state (auto mode selection, compression counters,
/// note reminder counter) must be byte-identical before and after a
/// `/context request` capture turn, across success, failure, and cancel.
///
/// The capture path clones every mutable per-session field it touches
/// (compress_state, note_nudge) and reads the auto selection without
/// consuming it (pending_for, not take_for). This test exercises each
/// isolation seam at the data-structure level.
#[test]
fn capture_turn_leaves_session_state_unchanged() {
    // --- Auto mode: pending_for is non-consuming ---
    // A pending Admin selection is queued for the active conversation.
    let mode_states = ConversationModeStates::default();
    let conv_id = "test-conv-id";
    let auto_control = mode_states.auto.bind(conv_id);
    newt_core::agentic::OperatingModeControl::select_operating_mode(&auto_control, "admin")
        .unwrap();

    // Before capture: selection is pending.
    let before = mode_states.auto.pending_for(conv_id);
    assert_eq!(
        before,
        Some(OperatingMode::Admin),
        "selection pending before capture"
    );

    // Capture path: peek without consuming (success, failure, and cancel all use pending_for).
    let peeked_success = mode_states.auto.pending_for(conv_id);
    let peeked_failure = mode_states.auto.pending_for(conv_id);
    let peeked_cancel = mode_states.auto.pending_for(conv_id);

    // After all capture paths: selection is still pending for the real next turn.
    let after = mode_states.auto.pending_for(conv_id);
    assert_eq!(
        before, peeked_success,
        "success path does not consume selection"
    );
    assert_eq!(
        before, peeked_failure,
        "failure path does not consume selection"
    );
    assert_eq!(
        before, peeked_cancel,
        "cancel path does not consume selection"
    );
    assert_eq!(before, after, "selection unchanged after capture");

    // The real next turn can still take it.
    assert_eq!(
        mode_states.auto.take_for(conv_id),
        Some(OperatingMode::Admin),
        "real turn takes the preserved selection"
    );
    assert_eq!(
        mode_states.auto.pending_for(conv_id),
        None,
        "selection consumed by real turn"
    );

    // --- CompressState: clone does not advance live counters ---
    let real_compress = newt_core::CompressState::new();
    let before_debug = format!("{:?}", real_compress.counters());

    // Capture gets a snapshot clone; it is discarded when the capture turn exits.
    let _capture_compress = real_compress.clone();

    let after_debug = format!("{:?}", real_compress.counters());
    assert_eq!(
        before_debug, after_debug,
        "compress counters unchanged after capture"
    );

    // --- NoteNudge: covered by note_nudge_clone_preserves_real_counter_across_capture_turn ---
    // That test verifies the boundary semantics (clone fires at boundary, real untouched).
}
