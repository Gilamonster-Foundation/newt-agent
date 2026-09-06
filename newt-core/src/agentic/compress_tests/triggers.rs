use super::*;

fn trigger_limits(
    count_threshold: usize,
    token_threshold: Option<usize>,
    send_budget: Option<usize>,
    tool_tokens: usize,
    policy: CompactionTriggerPolicy,
    has_authoritative_headroom: bool,
) -> CompressionTriggerLimits {
    CompressionTriggerLimits {
        count_threshold,
        token_threshold,
        send_budget,
        tool_tokens,
        policy,
        has_authoritative_headroom,
    }
}

// -- trigger ------------------------------------------------------------------

#[test]
fn trigger_fires_on_count_token_or_guard() {
    // Nothing fired.
    assert!(compression_trigger(
        10,
        1_000,
        900,
        trigger_limits(
            40,
            None,
            None,
            100,
            CompactionTriggerPolicy::HeadroomAware,
            false,
        ),
    )
    .is_none());
    // Token threshold (issue #223's crux: count far under threshold).
    // Like the send guard, it is a whole-request ceiling and must reserve
    // the advertised schema overhead before entering message space.
    let token = compression_trigger(
        4,
        60_000,
        59_000,
        trigger_limits(
            40,
            Some(50_000),
            None,
            100,
            CompactionTriggerPolicy::HeadroomAware,
            true,
        ),
    )
    .unwrap();
    assert_eq!(token.budget, 49_900);
    assert!(token.hard_budget);
    assert!(token.token_fired);
    assert_eq!(token.primary_cause, CompressTriggerCause::TokenThreshold);
    // Guard: budget = send_budget − tool schema tokens.
    let guard = compression_trigger(
        4,
        9_000,
        8_600,
        trigger_limits(
            40,
            None,
            Some(8_000),
            500,
            CompactionTriggerPolicy::HeadroomAware,
            true,
        ),
    )
    .unwrap();
    assert_eq!(guard.budget, 7_500);
    assert!(guard.hard_budget);
    assert!(guard.send_budget_fired);
    assert_eq!(guard.primary_cause, CompressTriggerCause::SendBudget);
    // Count only: budget halves the MESSAGE-token figure (NOT the
    // schema-inclusive current figure — the F1 cross-currency bug),
    // max_messages set, and the budget is soft (no anti-thrash).
    let count = compression_trigger(
        41,
        1_000,
        800,
        trigger_limits(
            40,
            None,
            None,
            100,
            CompactionTriggerPolicy::HeadroomAware,
            false,
        ),
    )
    .unwrap();
    assert_eq!(count.budget, 400);
    assert_eq!(count.max_messages, Some(20));
    assert!(!count.hard_budget);
    assert!(count.count_fired);
    assert_eq!(count.primary_cause, CompressTriggerCause::MessageCount);
    // All at once: the tightest token budget wins and stays hard.
    let combined = compression_trigger(
        41,
        60_000,
        59_000,
        trigger_limits(
            40,
            Some(50_000),
            Some(20_000),
            500,
            CompactionTriggerPolicy::MessageCount,
            true,
        ),
    )
    .unwrap();
    assert_eq!(combined.budget, 19_500);
    assert_eq!(combined.max_messages, Some(20));
    assert!(combined.hard_budget);
    assert!(combined.count_fired);
    assert!(combined.token_fired);
    assert!(combined.send_budget_fired);
    assert_eq!(combined.primary_cause, CompressTriggerCause::SendBudget);
    // Under-threshold figures don't fire their triggers.
    assert!(compression_trigger(
        4,
        7_999,
        7_000,
        trigger_limits(
            40,
            Some(50_000),
            Some(8_000),
            0,
            CompactionTriggerPolicy::HeadroomAware,
            true,
        ),
    )
    .is_none());
}

#[test]
fn headroom_aware_defers_count_only_compression_but_legacy_mode_keeps_it() {
    // A known million-token ceiling does not make 41 tiny messages an
    // emergency. The default must preserve the active prompt until real
    // token pressure appears.
    assert!(compression_trigger(
        41,
        1_000,
        800,
        trigger_limits(
            40,
            None,
            Some(1_000_000),
            100,
            CompactionTriggerPolicy::HeadroomAware,
            true,
        ),
    )
    .is_none());

    let legacy = compression_trigger(
        41,
        1_000,
        800,
        trigger_limits(
            40,
            None,
            Some(1_000_000),
            100,
            CompactionTriggerPolicy::MessageCount,
            true,
        ),
    )
    .unwrap();
    assert_eq!(legacy.primary_cause, CompressTriggerCause::MessageCount);
    assert!(legacy.count_fired);
    assert!(legacy.has_authoritative_headroom);

    // A learned `max_ok_input` high-water mark is not a known window, so
    // the fallback count guard remains available to protect that session.
    let unknown_window = compression_trigger(
        41,
        1_000,
        800,
        trigger_limits(
            40,
            None,
            Some(1_000_000),
            100,
            CompactionTriggerPolicy::HeadroomAware,
            false,
        ),
    )
    .unwrap();
    assert_eq!(
        unknown_window.primary_cause,
        CompressTriggerCause::MessageCount
    );
    assert!(!unknown_window.has_authoritative_headroom);

    // Real hard pressure still fires under the default even when the
    // count-only path is deferred.
    let hard = compression_trigger(
        41,
        2_000,
        1_800,
        trigger_limits(
            40,
            Some(1_500),
            Some(1_000_000),
            100,
            CompactionTriggerPolicy::HeadroomAware,
            true,
        ),
    )
    .unwrap();
    assert!(hard.hard_budget);
    assert!(!hard.count_fired);
    assert_eq!(hard.primary_cause, CompressTriggerCause::TokenThreshold);
}

/// Re-homed `trim_to_token_budget_zero_is_noop` (F3): a configured zero
/// token budget means DISABLED — `Some(0)` must not fire (the 18.4
/// regression flipped it to "compress to budget zero every round").
#[test]
fn trigger_zero_token_budget_is_disabled() {
    assert!(compression_trigger(
        4,
        100,
        90,
        trigger_limits(
            40,
            Some(0),
            None,
            0,
            CompactionTriggerPolicy::HeadroomAware,
            false,
        ),
    )
    .is_none());
    assert!(compression_trigger(
        4,
        100,
        90,
        trigger_limits(
            40,
            None,
            Some(0),
            10,
            CompactionTriggerPolicy::HeadroomAware,
            false,
        ),
    )
    .is_none());
    // Zero token budgets stay disabled while a real count trigger fires.
    let count = compression_trigger(
        41,
        100,
        90,
        trigger_limits(
            40,
            Some(0),
            Some(0),
            10,
            CompactionTriggerPolicy::HeadroomAware,
            false,
        ),
    )
    .unwrap();
    assert_eq!(count.budget, 45);
    assert_eq!(count.primary_cause, CompressTriggerCause::MessageCount);
}
