use super::*;

use super::test_support::{
    active_prompt_card, assistant_call, recording_summarizer, run, run_count_only, sys, tool_heavy,
    tool_result, user, EST,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// Hard-budget invocation on a NON-authoritative budget (Step 20.3): the
/// proven-good HWM alone, no believed ceiling. The disabled-and-over case
/// fails open (`DispatchedOverBudget`) instead of refusing.
async fn run_non_authoritative(
    messages: &[Value],
    budget: usize,
    max_messages: Option<usize>,
    summarizer: Option<&SummarizeFn>,
    state: &mut CompressState,
) -> CompressOutcome {
    compress(
        CompressRequest {
            rewrites_history: true,
            messages,
            budget,
            max_messages,
            replay_protected_tail_len: 0,
            task: "fix the failing test",
            hard_budget: true,
            authoritative: false,
            focus: None,
            est: EST,
            summary_input_cap_floor_chars: 8_192,
            compaction_store: None,
            compaction_stage: None,
        },
        summarizer,
        state,
    )
    .await
}

/// F2: count-only invocations neither feed anti-thrash (poor reclaims
/// never latch) nor consult it (a latched switch must not kill the
/// VRAM guard or convert it into a refused send).
#[tokio::test]
async fn count_only_never_feeds_or_consults_anti_thrash() {
    // Poor-reclaim count-only shape: small messages, so replacing the
    // middle with the marker reclaims (well) under 10%.
    let mut msgs = vec![sys("you are newt"), user("task")];
    for i in 0..10 {
        msgs.push(user(&format!("note {i}")));
    }
    let mut state = CompressState::new();
    for _ in 0..4 {
        let budget = estimate_tokens(&msgs, EST) / 2;
        let out = run_count_only(&msgs, budget, Some(6), None, &mut state).await;
        assert_ne!(out.action, CompressAction::Refused);
    }
    assert!(!state.disabled, "count-only passes must never latch");
    assert_eq!(state.attempts, 0, "count-only passes must never record");

    // A latched state must not block the VRAM guard.
    let mut latched = CompressState::new();
    latched.disabled = true;
    latched.notified = true;
    let budget = estimate_tokens(&msgs, EST) / 2;
    let out = run_count_only(&msgs, budget, Some(6), None, &mut latched).await;
    assert_ne!(out.action, CompressAction::Refused);
    assert!(
        out.messages.len() < msgs.len(),
        "the VRAM guard must stay alive while anti-thrash is latched"
    );
}

// -- #1966: proactive floor escalation + floor-trend tracking ---------------

/// One growing-transcript round: append a fresh, aged-shaped tool round
/// (read_file + a >200-char result, just over `summarize_min_chars`, so
/// pass 2 one-lines it once it ages out of `keep_last`).
fn push_tool_round(msgs: &mut Vec<Value>, round: usize) {
    msgs.push(assistant_call(
        "read_file",
        json!({"path": format!("src/f{round}.rs")}),
    ));
    msgs.push(tool_result(&format!("{round}:{}", "x".repeat(300))));
}

/// Red-first reproduction of #1966's mechanism: under a hard,
/// authoritative budget with `max_messages: None` (mirroring
/// `headroom_aware`'s suppression of the count trigger whenever an
/// authoritative ceiling is known — `compression_trigger`'s
/// `count_fired` is unreachable in that combination), `crate::prune`
/// never removes a message, so a growing transcript's post-prune floor
/// accretes round over round instead of shrinking back to a constant —
/// the exact "converges to an incompressible floor" shape the issue's
/// live session evidenced (422 est-tokens/round accretion, 82% of
/// budget reached over 175 rounds before a late, lossy mass-summarize).
///
/// Before the fix, `compress` only escalates past a successful prune
/// once the POST-PRUNE floor itself strictly exceeds the budget
/// (`crate::agentic::compress`'s `over()` gate) — reactive, at the wall.
/// After the fix, `CompressState::floor_trend`'s `latest` reading at the
/// escalating round (recorded from the same internal post-prune value
/// `compress` gates on, BEFORE the proactive check can override the
/// settle) must show the floor was still within budget (`< 1.0`) yet
/// past the proactive threshold — proving the escalation was PROACTIVE,
/// not merely the pre-existing reactive path relabeled.
#[tokio::test]
async fn repeated_pruned_rounds_under_headroom_aware_proactively_escalate() {
    let budget = 3_000usize;
    let mut msgs = vec![
        sys("you are newt"),
        active_prompt_card(),
        user("fix the bug"),
    ];
    let mut state = CompressState::new();
    let mut escalated: Option<(usize, FloorTrend)> = None;

    for round in 0..80 {
        push_tool_round(&mut msgs, round);
        // Captured BEFORE this round's call: `record_floor` only fires
        // from inside the `!over(after_prune, ...)` branch, so a PURELY
        // REACTIVE escalation (the pre-fix path — `after_prune` itself
        // exceeds budget) skips it and leaves the trend unchanged from
        // this snapshot. Comparing against it is what makes the
        // "escalated this round from a genuine settle" proof below
        // resistant to a disabled/no-op proactive check.
        let trend_before = state.floor_trend();
        let out = run(&msgs, budget, None, None, &mut state).await;
        assert_ne!(
            out.action,
            CompressAction::Refused,
            "round {round}: must never refuse — nothing here is irreducible"
        );
        msgs = out.messages.clone();
        if matches!(
            out.action,
            CompressAction::Summarized | CompressAction::StaticFallback
        ) {
            escalated = Some((round, trend_before));
            break;
        }
        assert!(
            matches!(out.action, CompressAction::Fit | CompressAction::Pruned),
            "round {round}: expected Fit-or-Pruned before escalation, got {:?}",
            out.action
        );
    }

    let (round, trend_before) = escalated.expect(
        "the pipeline never escalated past structural-pruned-only across 80 \
             rounds of a growing transcript — the accreting floor should have \
             crossed the proactive threshold well before this",
    );
    let trend = state.floor_trend();
    assert_ne!(
        trend, trend_before,
        "round {round}: no NEW floor reading landed on the escalating round \
             itself — `record_floor` only fires from a genuine settle-then-maybe-\
             override branch, so an unchanged trend here means this escalation \
             was the pre-existing REACTIVE (over-budget) path, which skips that \
             branch entirely, not a proactive one: {trend:?}"
    );
    let latest = trend.latest.expect(
        "a floor reading must have been recorded for the round that escalated \
             (`compress` records it before the proactive check can override the \
             settle) — got no reading at all",
    );
    assert!(
        latest < 1.0,
        "round {round}: escalation must be PROACTIVE — the internal post-prune \
             floor fraction that triggered it ({latest}) must still be within \
             budget (< 1.0); a value >= 1.0 would mean this was only the \
             pre-existing REACTIVE over-budget path: {trend:?}"
    );
    assert!(
        latest >= PROACTIVE_SUMMARIZE_FLOOR_FRACTION,
        "round {round}: the recorded floor fraction ({latest}) must have \
             crossed the proactive threshold ({PROACTIVE_SUMMARIZE_FLOOR_FRACTION}) \
             to explain why this round escalated: {trend:?}"
    );
    assert!(
        trend.rising(),
        "the floor trend across the settled prune-only rounds leading up to \
             escalation must show a RISE — the accretion the issue's evidence \
             found invisible: {trend:?}"
    );
}

/// One growing-transcript round with LARGE (5,000-char) results — big
/// enough that structural prune is exercised repeatedly within a modest
/// budget (unlike [`push_tool_round`]'s smaller results, which a huge
/// budget would let sail past the entry check without ever attempting a
/// prune at all — see the twin below).
fn push_big_tool_round(msgs: &mut Vec<Value>, round: usize) {
    msgs.push(assistant_call(
        "read_file",
        json!({"path": format!("src/f{round}.rs")}),
    ));
    msgs.push(tool_result(&format!("{round}:{}", "x".repeat(5_000))));
}

/// Twin of the reproduction above: a HEALTHY session whose structural
/// prune is genuinely exercised (large per-round results force the raw
/// estimate over budget repeatedly, unlike a budget so generous prune
/// never engages at all — that would pass vacuously without touching
/// the code path under test), but whose settled floor stays well under
/// the proactive threshold throughout. Must never escalate; proves the
/// new trigger is a genuine threshold check, not an eager
/// summarize-on-any-prune.
#[tokio::test]
async fn a_healthy_session_with_a_stable_floor_never_proactively_escalates() {
    let budget = 20_000usize;
    let mut msgs = vec![
        sys("you are newt"),
        active_prompt_card(),
        user("fix the bug"),
    ];
    let mut state = CompressState::new();
    let mut prune_engaged = false;

    for round in 0..80 {
        push_big_tool_round(&mut msgs, round);
        let out = run(&msgs, budget, None, None, &mut state).await;
        assert!(
            matches!(out.action, CompressAction::Fit | CompressAction::Pruned),
            "round {round}: a healthy, far-under-threshold floor must never \
                 escalate to summarize, got {:?}",
            out.action
        );
        prune_engaged |= out.action == CompressAction::Pruned;
        msgs = out.messages.clone();
    }
    assert!(
        prune_engaged,
        "structural prune was never exercised across 80 rounds — this twin \
             proves nothing about the proactive threshold if the code path it \
             gates was never reached"
    );
}

// -- #1966: `CompressState::floor_trend` ------------------------------------

#[test]
fn floor_trend_starts_unrecorded_and_is_never_rising() {
    let state = CompressState::new();
    let trend = state.floor_trend();
    assert_eq!(trend.previous, None);
    assert_eq!(trend.latest, None);
    assert!(!trend.rising());
}

#[test]
fn floor_trend_detects_a_rise_across_two_recordings() {
    let mut state = CompressState::new();
    state.record_floor(2_500, 10_000); // 0.25 — exact in f32
    state.record_floor(7_500, 10_000); // 0.75 — exact in f32
    let trend = state.floor_trend();
    assert_eq!(trend.previous, Some(0.25));
    assert_eq!(trend.latest, Some(0.75));
    assert!(trend.rising(), "{trend:?}");
}

/// Twin: a held or shrinking floor must never report as rising.
#[test]
fn floor_trend_does_not_report_rising_when_the_floor_holds_or_shrinks() {
    let mut state = CompressState::new();
    state.record_floor(7_500, 10_000);
    state.record_floor(7_500, 10_000); // holds
    assert!(
        !state.floor_trend().rising(),
        "holding: {:?}",
        state.floor_trend()
    );

    state.record_floor(2_500, 10_000); // shrinks
    assert!(
        !state.floor_trend().rising(),
        "shrinking: {:?}",
        state.floor_trend()
    );
}

#[test]
fn record_floor_is_a_defensive_noop_on_a_zero_budget() {
    let mut state = CompressState::new();
    state.record_floor(1_000, 0);
    assert_eq!(
        state.floor_trend(),
        FloorTrend {
            previous: None,
            latest: None
        }
    );
}

// -- anti-thrash ------------------------------------------------------------

/// Two consecutive <10% reclaims disable compression, the user is
/// notified exactly once, and further over-budget calls are refused.
#[tokio::test]
async fn anti_thrash_disables_notifies_once_then_refuses() {
    // Incompressible over-budget input: user messages only (nothing for
    // prune), head+tail protection covering everything (no middle).
    let mut msgs = vec![sys("small protected system"), user("task")];
    for i in 0..3 {
        msgs.push(user(&format!("note {i} {}", "x".repeat(4_000))));
    }
    let mut state = CompressState::new();

    let first = run(&msgs, 100, None, None, &mut state).await;
    assert_ne!(first.action, CompressAction::Refused);
    assert!(first.notice.is_none(), "one poor pass is not yet thrash");

    let second = run(&msgs, 100, None, None, &mut state).await;
    let notice = second.notice.expect("second poor pass must notify");
    assert!(notice.contains("disabled for this session"), "{notice}");

    let third = run(&msgs, 100, None, None, &mut state).await;
    assert_eq!(third.action, CompressAction::Refused);
    assert!(!third.fired);
    assert!(
        third.notice.is_none(),
        "the notice must be delivered exactly once"
    );

    // Under-budget calls still pass through untouched while disabled.
    let ok = run(&msgs, 100_000, None, None, &mut state).await;
    assert_eq!(ok.action, CompressAction::Fit);
}

/// Step 20.3 — the fail-open path. With anti-thrash latched and the
/// context over a NON-authoritative budget (the proven-good HWM alone, no
/// believed window — the cloud / gpt-4.1 case), the send must NOT be
/// refused. Refusing there is the death spiral: it discards the very
/// acceptance evidence that would raise the HWM. Instead the messages pass
/// through unchanged as `DispatchedOverBudget` so the caller dispatches and
/// the backend rules.
#[tokio::test]
async fn non_authoritative_budget_fails_open_instead_of_refusing() {
    let mut msgs = vec![sys("small protected system"), user("task")];
    for i in 0..3 {
        msgs.push(user(&format!("note {i} {}", "x".repeat(4_000))));
    }
    let mut state = CompressState::new();

    // Two incompressible poor passes latch anti-thrash (same as the
    // refuse test), but on a non-authoritative budget.
    let first = run_non_authoritative(&msgs, 100, None, None, &mut state).await;
    assert_ne!(first.action, CompressAction::Refused);
    let _second = run_non_authoritative(&msgs, 100, None, None, &mut state).await;
    assert!(state.disabled, "two poor passes must latch the breaker");

    // The latched, over-budget third call FAILS OPEN — never Refused.
    let third = run_non_authoritative(&msgs, 100, None, None, &mut state).await;
    assert_eq!(third.action, CompressAction::DispatchedOverBudget);
    assert!(!third.fired, "messages pass through unchanged");
    assert_eq!(third.messages.len(), msgs.len(), "nothing dropped");
    let notice = third.notice.expect("fail-open is surfaced once");
    assert!(notice.contains("no authoritative window"), "{notice}");

    // And the fail-open notice fires exactly once.
    let fourth = run_non_authoritative(&msgs, 100, None, None, &mut state).await;
    assert_eq!(fourth.action, CompressAction::DispatchedOverBudget);
    assert!(fourth.notice.is_none(), "notice delivered exactly once");
}

/// Step 20.3 — the authoritative budget still refuses (B6 preserved): a
/// declared/believed window or cw-400 cap must stop a send the backend
/// would silently head-truncate. Only the lone HWM fails open.
#[tokio::test]
async fn authoritative_budget_still_refuses_when_latched() {
    let mut msgs = vec![sys("small protected system"), user("task")];
    for i in 0..3 {
        msgs.push(user(&format!("note {i} {}", "x".repeat(4_000))));
    }
    let mut state = CompressState::new();
    run(&msgs, 100, None, None, &mut state).await;
    run(&msgs, 100, None, None, &mut state).await;
    assert!(state.disabled);
    let third = run(&msgs, 100, None, None, &mut state).await;
    assert_eq!(
        third.action,
        CompressAction::Refused,
        "an authoritative ceiling must still refuse, not truncate"
    );
}

#[tokio::test]
async fn authoritative_budget_refuses_irreducible_prompt_before_any_summary() {
    let exact = format!("GIANT-EXACT-PROMPT {}", "z".repeat(20_000));
    let messages = vec![sys("base"), active_prompt_card(), user(&exact)];
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let summarizer = recording_summarizer(prompts.clone(), "must not run");
    let mut state = CompressState::new();
    let out = compress(
        CompressRequest {
            rewrites_history: true,
            messages: &messages,
            budget: 128,
            max_messages: None,
            replay_protected_tail_len: 0,
            task: &exact,
            hard_budget: true,
            authoritative: true,
            focus: None,
            est: EST,
            summary_input_cap_floor_chars: 8_192,
            compaction_store: None,
            compaction_stage: None,
        },
        Some(&*summarizer),
        &mut state,
    )
    .await;

    assert_eq!(out.action, CompressAction::Refused);
    assert_eq!(out.messages, messages, "exact prompt is never truncated");
    assert!(prompts.lock().unwrap().is_empty(), "no summary dispatch");
}

/// #6 (D, #661): the complement of the test above — when the middle IS
/// reducible (small head+tail, large summarizable middle), a latched
/// authoritative over-budget call performs a forced static-marker compaction
/// that fits, instead of the dead-end Refused. Refusal is reserved for the
/// truly-irreducible (head+tail alone over budget) case.
#[tokio::test]
async fn latched_authoritative_compacts_to_marker_instead_of_refusing() {
    let mut msgs = vec![sys("sys"), user("task")];
    for i in 0..24 {
        msgs.push(user(&format!("middle note {i} {}", "m".repeat(200))));
    }
    msgs.push(user("recent tail"));
    let mut state = CompressState::new();
    state.latch_disabled_for_tests();
    let budget = 300; // far below the whole conversation; head+tail+marker fit
    let out = run(&msgs, budget, None, None, &mut state).await;
    assert_ne!(
        out.action,
        CompressAction::Refused,
        "a reducible middle must compact to a marker, not dead-end"
    );
    assert!(
        out.tokens_after <= budget,
        "forced marker compaction must fit the budget ({} > {budget})",
        out.tokens_after
    );
    assert!(out.fired, "the marker compaction changed the working set");
}

/// Effective compressions never trip the anti-thrash switch.
#[tokio::test]
async fn effective_compressions_do_not_disable() {
    let mut state = CompressState::new();
    for _ in 0..4 {
        let msgs = tool_heavy("task", 6, 4_000);
        let before = estimate_tokens(&msgs, EST);
        let out = run(&msgs, before / 3, None, None, &mut state).await;
        assert_ne!(out.action, CompressAction::Refused);
        assert!(out.notice.is_none());
    }
    assert!(!state.disabled);
}

/// A good pass between two poor ones resets the "twice in a row" window.
#[test]
fn thrash_window_requires_consecutive_poor_savings() {
    let mut state = CompressState::new();
    state.record(1_000, 990, 500); // poor
    state.record(1_000, 400, 500); // good
    state.record(1_000, 990, 500); // poor
    assert!(!state.disabled, "non-consecutive poor passes never disable");
    state.record(1_000, 950, 500); // poor — now two in a row
    assert!(state.disabled);
}

#[test]
fn budget_aware_gap_progress_is_not_a_strike() {
    // #661 regression: a pass reclaiming <10% RELATIVE but shrinking the
    // over-budget GAP meaningfully is EFFECTIVE — the old relative-only gate
    // disabled compression on a tight budget exactly when it mattered.
    let mut state = CompressState::new();
    // 1000→920 against budget 800: relative 8% (<10%), but gap 200→120 (−40%).
    state.record(1_000, 920, 800);
    state.record(1_000, 920, 800);
    assert!(
        !state.is_disabled(),
        "gap-shrinking passes must not latch the disable"
    );
    // A genuinely useless pass (no fit, no gap progress, no abs floor, <10%)
    // still strikes twice and latches.
    let mut dead = CompressState::new();
    dead.record(1_000, 995, 500);
    dead.record(1_000, 996, 500);
    assert!(dead.is_disabled(), "truly ineffective passes still latch");
}

/// Counters snapshot: a pure projection of the recorded state.
#[test]
fn counters_snapshot_projects_state() {
    let mut state = CompressState::new();
    let c = state.counters();
    assert_eq!((c.compressions, c.strikes, c.disabled), (0, 0, false));
    assert_eq!(c.last_reclaim, None);

    state.record(1_000, 400, 500); // good: 60% reclaim
    let c = state.counters();
    assert_eq!((c.compressions, c.strikes, c.disabled), (1, 0, false));
    assert!((c.last_reclaim.unwrap() - 0.6).abs() < 0.01);

    state.record(1_000, 990, 500); // poor — one strike
    let c = state.counters();
    assert_eq!((c.compressions, c.strikes, c.disabled), (2, 1, false));

    state.record(1_000, 950, 500); // poor — two in a row latches
    let c = state.counters();
    assert_eq!((c.compressions, c.strikes, c.disabled), (3, 2, true));
    assert!(c.last_reclaim.unwrap() < THRASH_MIN_SAVINGS);
}

/// A single poor FIRST attempt is one strike, not two: the [1.0, 1.0]
/// sentinel in the unused slot must never read as a recorded strike.
#[test]
fn counters_first_poor_attempt_is_one_strike() {
    let mut state = CompressState::new();
    state.record(1_000, 990, 500);
    assert_eq!(state.counters().strikes, 1);
}
