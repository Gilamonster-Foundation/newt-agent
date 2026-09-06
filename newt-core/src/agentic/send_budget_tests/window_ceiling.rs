use super::*;

/// The Chat Completions generation policy and input budget share one model
/// window. These fixtures cover every cognition output allowance at the
/// two local qualification windows from the Nemotron review.
#[test]
fn cognition_output_is_reserved_from_32k_and_65k_context_windows() {
    let capability = ChatCompletionsCapability {
        cognition: Some(true),
        ..Default::default()
    };
    let cases = [
        (Cognition::Glancing, 26_214, 52_428),
        (Cognition::Pondering, 26_214, 52_428),
        (Cognition::Deliberating, 22_768, 52_428),
        (Cognition::Contemplating, 16_768, 49_536),
    ];

    for (cognition, expected_32k, expected_65k) in cases {
        let policy =
            GenerationPolicy::resolve(Some(cognition), capability, ReasoningReplayScope::Never);
        assert_eq!(
            num_ctx_input_ceiling(Some(32_768), 80, policy.max_output_tokens),
            Some(expected_32k),
            "32K {cognition}"
        );
        assert_eq!(
            num_ctx_input_ceiling(Some(65_536), 80, policy.max_output_tokens),
            Some(expected_65k),
            "65K {cognition}"
        );
    }
}
#[test]
fn recovered_full_window_reapplies_output_reserve_without_failing_open() {
    assert_eq!(
        recovered_input_budget(32_768, 80, Some(16_000), Some(49_536)),
        16_768,
        "a 32K recovered window reserves contemplating's 16K output"
    );
    assert_eq!(
        recovered_input_budget(8_000, 80, Some(10_000), Some(22_768)),
        0,
        "output consuming the recovered window remains an authoritative zero"
    );
}
/// THE B6 first-turn hole: a fresh capability cache (no `max_ok_input`,
/// no `safe_context`) used to mean NO budget at all even though the
/// request itself carried `options.num_ctx = 4096`. The ceiling must now
/// arm the trigger on turn 1 — as a HARD budget (anti-thrash semantics).
#[test]
fn first_turn_fresh_cache_trigger_sees_the_num_ctx_ceiling() {
    let budget = initial_send_budget(None, None, num_ctx_input_ceiling(Some(4096), 80, None));
    assert_eq!(budget, Some(3276), "80% of 4096 — reply headroom reserved");
    // The measured B6 shape: ~41k estimated tokens, 3 messages, no
    // count/token thresholds in reach — pre-fix this returned None and
    // the request sailed into the 4k window with zero events.
    let trigger = compression_trigger(
        3,
        41_355,
        39_900,
        CompressionTriggerLimits {
            count_threshold: 40,
            token_threshold: None,
            send_budget: budget,
            tool_tokens: 1_432,
            policy: CompactionTriggerPolicy::HeadroomAware,
            has_authoritative_headroom: true,
        },
    )
    .expect("the ceiling must fire the trigger on the first turn");
    assert!(trigger.hard_budget, "a real token budget, not a soft halve");
    assert_eq!(
        trigger.budget,
        3_276 - 1_432,
        "budget lands in message space: ceiling minus tool-schema tokens"
    );
    assert_eq!(trigger.max_messages, None, "no count firing here");
}
/// Absent `num_ctx` → exactly the cached-numbers budget (no ceiling).
/// CONTRACT CHANGED in Phase 20 (docs/design/model-self-tuning.md §2.1):
/// the cached figure is now `max(max_ok_input, safe_context)` — the
/// high-water mark is a floor of proven-good, not a ceiling, so it must
/// never pull the budget BELOW the believed-safe window.
#[test]
fn absent_num_ctx_leaves_the_budget_unchanged() {
    assert_eq!(initial_send_budget(None, None, None), None);
    assert_eq!(initial_send_budget(Some(2_000), None, None), Some(2_000));
    assert_eq!(initial_send_budget(None, Some(5_000), None), Some(5_000));
    assert_eq!(
        initial_send_budget(Some(2_000), Some(5_000), None),
        Some(5_000),
        "an HWM below safe_context is a floor, not a cap — safe_context wins"
    );
    // And with no budget at all, the trigger stays silent regardless of size.
    assert_eq!(
        compression_trigger(
            3,
            41_355,
            39_900,
            CompressionTriggerLimits {
                count_threshold: 40,
                token_threshold: None,
                send_budget: None,
                tool_tokens: 1_432,
                policy: CompactionTriggerPolicy::HeadroomAware,
                has_authoritative_headroom: false,
            },
        ),
        None
    );
}
/// Phase 20 §2.1 — the max(proven, believed) contract, all three shapes:
/// HWM below the claim, HWM above the claim (proven beyond it), and the
/// post-cw-400 shape where `safe_context` was reined to the authoritative
/// cap so `max()` still lands on the authoritative number.
#[test]
fn cached_budget_is_max_of_proven_and_believed() {
    // The motivating failure: max_ok_input ratcheted to 6,068 (largest
    // prompt SEEN) while safe_context believed 80% of a 32k window safe.
    // Pre-fix the 6,068 won and refused sends the backend accepted.
    assert_eq!(
        initial_send_budget(Some(6_068), Some(26_214), None),
        Some(26_214),
        "HWM below safe_context → safe_context"
    );
    // Proven beyond the claim: an accepted 8,734-token prompt outranks a
    // conservative claim-derived window.
    assert_eq!(
        initial_send_budget(Some(8_734), Some(6_553), None),
        Some(8_734),
        "HWM above safe_context (proven beyond the claim) → HWM"
    );
    // cw-400-reined shape (#223): the 400 set max_ok_input to 80% of the
    // endpoint's reported hard limit (authoritative, may be HIGH) and
    // reined safe_context down to equal-or-lower — max() must land on
    // the authoritative cap, not regress to the VRAM-capped figure.
    assert_eq!(
        initial_send_budget(Some(800_000), Some(64_000), None),
        Some(800_000),
        "post-cw-400: max_ok_input is the authoritative cap"
    );
    assert_eq!(
        initial_send_budget(Some(800_000), Some(800_000), None),
        Some(800_000)
    );
}
/// The ceiling composes with existing budgets via `min` — whichever is
/// tighter wins, in both directions.
#[test]
fn ceiling_composes_with_cached_budgets_via_min() {
    // Cached cap tighter than the ceiling: cached wins (mid-loop B5
    // behavior is untouched by #282).
    assert_eq!(
        initial_send_budget(
            Some(2_135),
            None,
            num_ctx_input_ceiling(Some(4_096), 80, None),
        ),
        Some(2_135)
    );
    // Ceiling tighter than the cached cap: the B6 shape — bootstrap
    // safe_context 104,857 vs forced num_ctx 4,096.
    assert_eq!(
        initial_send_budget(
            None,
            Some(104_857),
            num_ctx_input_ceiling(Some(4_096), 80, None),
        ),
        Some(3_276)
    );
    assert_eq!(
        initial_send_budget(
            Some(104_857),
            Some(104_857),
            num_ctx_input_ceiling(Some(4_096), 80, None),
        ),
        Some(3_276)
    );
}
/// A declared window with no room for input remains authoritative. `None`
/// alone means unknown; zero must not erase the ceiling and fail open.
#[test]
fn zero_remaining_input_budget_does_not_fail_open() {
    assert_eq!(num_ctx_input_ceiling(None, 80, Some(16_000)), None);
    assert_eq!(num_ctx_input_ceiling(Some(0), 80, None), Some(0));
    assert_eq!(num_ctx_input_ceiling(Some(1), 80, None), Some(0));
    assert_eq!(
        num_ctx_input_ceiling(Some(4_096), 80, Some(4_096)),
        Some(0),
        "the output reserve consumes the whole known window"
    );
    assert_eq!(initial_send_budget(None, None, Some(0)), Some(0));
    assert_eq!(
        initial_send_budget(Some(2_000), None, Some(0)),
        Some(0),
        "an authoritative zero ceiling must shadow cached evidence"
    );
}
#[test]
fn programmatic_percentage_values_use_config_normalization() {
    assert_eq!(num_ctx_input_ceiling(Some(10_000), 0, None), Some(8_000));
    assert_eq!(num_ctx_input_ceiling(Some(10_000), 100, None), Some(8_000));
    assert_eq!(
        num_ctx_input_ceiling(Some(10_000), u32::MAX, None),
        Some(8_000)
    );
}
