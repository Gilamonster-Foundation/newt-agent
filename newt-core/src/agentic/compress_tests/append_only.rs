use super::*;

use super::test_support::{recording_summarizer, sys, tool_heavy, user, EST};
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn append_only_refuses_rather_than_rewriting() {
    let task = "do the task";
    let msgs = tool_heavy(task, 6, 4_000);
    let before = estimate_tokens(&msgs, EST);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "SUMMARY");

    let base = |rewrites_history: bool| CompressRequest {
        messages: &msgs,
        budget: before / 3,
        max_messages: None,
        replay_protected_tail_len: 0,
        task,
        hard_budget: true,
        authoritative: true,
        focus: None,
        est: EST,
        summary_input_cap_floor_chars: 8_192,
        rewrites_history,
        compaction_store: None,
        compaction_stage: None,
    };

    // standard: rewrites, and the summarizer is consulted.
    let mut st = CompressState::new();
    let standard = compress(base(true), Some(&*s), &mut st).await;
    assert_eq!(standard.action, CompressAction::Summarized);
    assert!(standard.tokens_after < standard.tokens_before);
    assert!(
        !prompts.lock().unwrap().is_empty(),
        "standard must summarize"
    );

    // append-only: refuses, changes nothing, and never calls the model.
    prompts.lock().unwrap().clear();
    let mut st2 = CompressState::new();
    let append = compress(base(false), Some(&*s), &mut st2).await;
    assert_eq!(append.action, CompressAction::Refused);
    assert!(!append.fired, "an append-only pass does not fire");
    assert_eq!(
        append.messages, msgs,
        "append-only must hand the transcript back byte-identical"
    );
    assert_eq!(append.tokens_after, append.tokens_before);
    assert!(
        prompts.lock().unwrap().is_empty(),
        "append-only must not consult the summarizer at all"
    );
}

/// Append-only must not turn a SOFT trigger into a fatal turn. The count /
/// VRAM guard (`hard_budget: false`) never had standing to refuse a send
/// (F2), and a budget resting on the proven-good high-water mark alone
/// (`authoritative: false`) is the Step 20.3 fail-open case — refusing there
/// discards the acceptance evidence that raises the HWM.
///
/// This matters more under append-only than under `standard`, not less: the
/// transcript never shrinks, so the first refusal is also every later turn's
/// and the session is wedged until `/new`. Dispatching rewrites nothing, so
/// failing open honours the preset exactly.
///
/// Regression: with the guard ungated on `hard_budget && authoritative` both
/// cases return `Refused` and the callers bail the turn.
#[tokio::test]
async fn append_only_fails_open_on_soft_and_non_authoritative_triggers() {
    let task = "do the task";
    let msgs = tool_heavy(task, 6, 4_000);
    let before = estimate_tokens(&msgs, EST);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "SUMMARY");

    let req = |hard_budget: bool, authoritative: bool| CompressRequest {
        messages: &msgs,
        budget: before / 3,
        max_messages: None,
        replay_protected_tail_len: 0,
        task,
        hard_budget,
        authoritative,
        focus: None,
        est: EST,
        summary_input_cap_floor_chars: 8_192,
        rewrites_history: false,
        compaction_store: None,
        compaction_stage: None,
    };

    for (hard_budget, authoritative, label) in [
        (false, false, "count-only trigger"),
        (false, true, "soft trigger, authoritative budget"),
        (true, false, "hard trigger, high-water-mark budget"),
    ] {
        let mut st = CompressState::new();
        let out = compress(req(hard_budget, authoritative), Some(&*s), &mut st).await;
        assert_eq!(
            out.action,
            CompressAction::DispatchedOverBudget,
            "{label} must fail open, not refuse"
        );
        assert_eq!(out.refusal, None, "{label} is not a refusal");
        assert_eq!(out.messages, msgs, "{label} must not rewrite");
        assert!(!out.fired, "{label} does not fire");
    }
    assert!(
        prompts.lock().unwrap().is_empty(),
        "no fail-open path may consult the summarizer"
    );

    // Only the authoritative hard ceiling refuses — and it says WHY, so the
    // caller cannot report it as anti-thrash.
    let mut st = CompressState::new();
    let out = compress(req(true, true), Some(&*s), &mut st).await;
    assert_eq!(out.action, CompressAction::Refused);
    assert_eq!(out.refusal, Some(RefusalReason::AppendOnly));
    assert_eq!(out.messages, msgs);
}

/// The append-only fail-open explains itself once, and never borrows the
/// anti-thrash notice — which would tell the operator compression had been
/// "ineffective twice in a row" when the latch was never even consulted.
#[tokio::test]
async fn append_only_notice_is_its_own_and_fires_once() {
    let task = "do the task";
    let msgs = tool_heavy(task, 6, 4_000);
    let before = estimate_tokens(&msgs, EST);
    let req = || CompressRequest {
        messages: &msgs,
        budget: before / 3,
        max_messages: None,
        replay_protected_tail_len: 0,
        task,
        hard_budget: false,
        authoritative: false,
        focus: None,
        est: EST,
        summary_input_cap_floor_chars: 8_192,
        rewrites_history: false,
        compaction_store: None,
        compaction_stage: None,
    };
    let mut st = CompressState::new();
    let first = compress(req(), None, &mut st).await;
    let notice = first.notice.expect("the first fail-open explains itself");
    assert!(
        notice.contains("append-only"),
        "notice must name the cause: {notice}"
    );
    assert!(
        !notice.contains("ineffective"),
        "must not borrow the anti-thrash message: {notice}"
    );
    assert!(
        compress(req(), None, &mut st).await.notice.is_none(),
        "the notice is one-time"
    );
}

/// A transcript that already fits is untouched under append-only — refusal is
/// for the over-budget case, not a blanket stop.
///
/// Note this one deliberately exits at the `Fit` guard ABOVE the append-only
/// branch: what it pins is that ordering, not the branch itself. The branch
/// is covered by the two tests above and by
/// `append_only_refuses_rather_than_rewriting`.
#[tokio::test]
async fn append_only_leaves_a_fitting_transcript_alone() {
    let msgs = vec![sys("s"), user("short")];
    let mut st = CompressState::new();
    let out = compress(
        CompressRequest {
            messages: &msgs,
            budget: 100_000,
            max_messages: None,
            replay_protected_tail_len: 0,
            task: "t",
            hard_budget: true,
            authoritative: true,
            focus: None,
            est: EST,
            summary_input_cap_floor_chars: 8_192,
            rewrites_history: false,
            compaction_store: None,
            compaction_stage: None,
        },
        None,
        &mut st,
    )
    .await;
    assert_eq!(out.action, CompressAction::Fit);
    assert_eq!(out.messages, msgs);
}
