use super::*;

use super::test_support::{
    active_prompt_card, assistant_call, run, sys, tool_heavy, tool_result, user, EST,
};
use serde_json::json;

// -- boundary -------------------------------------------------------------

#[test]
fn boundary_head_protects_only_the_active_prompt_pair() {
    let msgs = tool_heavy("the task", 6, 1_000);
    let b = compute_boundary(&msgs, 1_000, None, EST);
    assert_eq!(b.head, 3, "base system + metadata card + exact user prompt");

    let mut unprotected = tool_heavy("historical task", 6, 1_000);
    unprotected.remove(1);
    assert_eq!(
        compute_boundary(&unprotected, 1_000, None, EST).head,
        1,
        "an arbitrary first historical user message is not protected"
    );

    // Multiple system messages all land in the head, followed by the pair.
    let mut msgs2 = vec![
        sys("a"),
        sys("b"),
        active_prompt_card(),
        user("task"),
        user("more"),
    ];
    msgs2.extend(tool_heavy("x", 4, 1_000).split_off(3));
    assert_eq!(compute_boundary(&msgs2, 1_000, None, EST).head, 4);
}

#[test]
fn boundary_tail_is_token_budgeted_with_minimum() {
    // 10 rounds of ~250-token results; budget 4_000 → tail budget 1_000.
    let msgs = tool_heavy("task", 10, 1_000);
    let b = compute_boundary(&msgs, 4_000, None, EST);
    let tail_tokens: usize = msgs[b.tail_start..]
        .iter()
        .map(|m| estimate_value_tokens(m, EST))
        .sum();
    assert!(
        tail_tokens <= 1_500,
        "tail stays near the token budget, got {tail_tokens}"
    );
    assert!(
        msgs.len() - b.tail_start >= TAIL_MIN_MESSAGES,
        "at least the minimum tail"
    );
    assert!(b.tail_start > b.head, "a middle exists to summarize");

    // Huge results: the minimum still applies even over the token budget.
    let msgs = tool_heavy("task", 6, 40_000);
    let b = compute_boundary(&msgs, 4_000, None, EST);
    assert!(msgs.len() - b.tail_start >= TAIL_MIN_MESSAGES);
}

#[test]
fn boundary_anchors_last_user_message_into_tail() {
    // A user interjection deep in the middle, then many tool rounds whose
    // token mass would normally push the tail cut past it.
    let mut msgs = tool_heavy("task", 2, 500);
    msgs.push(user("IMPORTANT FOLLOW-UP: also update the docs"));
    let follow_up = msgs.len() - 1;
    for i in 0..6 {
        msgs.push(assistant_call(
            "read_file",
            json!({"path": format!("f{i}")}),
        ));
        msgs.push(tool_result(&"q".repeat(4_000)));
    }
    let b = compute_boundary(&msgs, 2_000, None, EST);
    assert!(
        b.tail_start <= follow_up,
        "tail (start {}) must include the last user message at {follow_up}",
        b.tail_start
    );
}

/// F1a: the last-user anchor must skip the pipeline's own compaction
/// message — anchoring on it pinned the tail at the marker forever
/// (the middle went empty and nothing could ever shrink again).
#[test]
fn boundary_anchor_skips_compaction_messages() {
    let mut msgs = vec![sys("you are newt"), active_prompt_card(), user("the task")];
    msgs.push(summary_message("## Active Task\nthe task (summarized)"));
    let marker = msgs.len() - 1;
    for i in 0..6 {
        msgs.push(assistant_call(
            "read_file",
            json!({"path": format!("f{i}")}),
        ));
        msgs.push(tool_result(&"q".repeat(4_000)));
    }
    let b = compute_boundary(&msgs, 2_000, None, EST);
    assert!(
        b.tail_start > marker,
        "the tail must not pin to the compaction message at index {marker} \
             (tail_start {})",
        b.tail_start
    );
    // A real user follow-up AFTER the marker still anchors.
    let mut msgs2 = msgs.clone();
    msgs2.push(user("IMPORTANT FOLLOW-UP: also update the docs"));
    let follow_up = msgs2.len() - 1;
    for _ in 0..4 {
        msgs2.push(assistant_call("read_file", json!({"path": "g"})));
        msgs2.push(tool_result(&"q".repeat(4_000)));
    }
    let b2 = compute_boundary(&msgs2, 2_000, None, EST);
    assert!(
        b2.tail_start <= follow_up,
        "a real user message still anchors the tail"
    );
}

/// The loop's post-compaction continuation directive is user-role but
/// pipeline-owned: like the summary marker (F1a) it must never anchor
/// the tail, or from the second compression on the boundary pins to the
/// harness's own act-now message instead of the operator's real ask.
#[test]
fn boundary_anchor_skips_continuation_directive() {
    let mut msgs = vec![sys("you are newt"), active_prompt_card(), user("the task")];
    msgs.push(user(&format!(
        "{CONTINUATION_PREFIX} You are mid-task: continue with a tool call."
    )));
    let directive = msgs.len() - 1;
    for i in 0..6 {
        msgs.push(assistant_call(
            "read_file",
            json!({"path": format!("f{i}")}),
        ));
        msgs.push(tool_result(&"q".repeat(4_000)));
    }
    assert!(is_compaction_message(&msgs[directive]));
    assert!(is_continuation_message(&msgs[directive]));
    let b = compute_boundary(&msgs, 2_000, None, EST);
    assert!(
        b.tail_start > directive,
        "the tail must not pin to the continuation directive at index \
             {directive} (tail_start {})",
        b.tail_start
    );
}

/// A `[loop-guidance]` rescue nudge is likewise harness-owned: pinning
/// the tail to the harness's own correction would demote the OPERATOR's
/// most recent real ask into the summarizable middle.
#[test]
fn boundary_anchor_skips_loop_guidance_nudges() {
    let mut msgs = vec![sys("you are newt"), user("the task")];
    msgs.push(user("IMPORTANT FOLLOW-UP: also update the docs"));
    let operator_ask = msgs.len() - 1;
    for i in 0..3 {
        msgs.push(assistant_call(
            "read_file",
            json!({"path": format!("f{i}")}),
        ));
        msgs.push(tool_result(&"q".repeat(4_000)));
    }
    msgs.push(user(&format!(
        "{LOOP_GUIDANCE_PREFIX} You described what you were about to do \
             but did not call any tool…"
    )));
    let nudge = msgs.len() - 1;
    for i in 0..4 {
        msgs.push(assistant_call(
            "read_file",
            json!({"path": format!("g{i}")}),
        ));
        msgs.push(tool_result(&"q".repeat(4_000)));
    }
    assert!(is_compaction_message(&msgs[nudge]));
    let b = compute_boundary(&msgs, 2_000, None, EST);
    assert!(
        b.tail_start <= operator_ask,
        "the anchor must skip the harness nudge at {nudge} and protect \
             the operator's ask at {operator_ask} (tail_start {})",
        b.tail_start
    );
}

/// F1d: when the anchored last-user message sits deep before many tool
/// rounds (the multi-turn shape), the count ceiling still caps the
/// tail — otherwise `max_messages` is unreachable and the count
/// trigger re-fires (and re-summarizes) every round.
#[test]
fn boundary_count_cap_holds_after_the_anchor() {
    let mut msgs = vec![
        sys("you are newt"),
        user("turn 1"),
        json!({"role": "assistant", "content": "reply 1"}),
        user("turn 2"),
        json!({"role": "assistant", "content": "reply 2"}),
        user("the current task"),
    ];
    let task_idx = msgs.len() - 1;
    for i in 0..12 {
        msgs.push(assistant_call(
            "read_file",
            json!({"path": format!("f{i}")}),
        ));
        msgs.push(tool_result(&"q".repeat(2_000)));
    }
    let b = compute_boundary(&msgs, 4_000, Some(10), EST);
    let assembled = b.head + 1 + (msgs.len() - b.tail_start);
    assert!(
        assembled <= 12,
        "the anchor must not defeat the count goal (assembled {assembled})"
    );
    assert!(
        b.tail_start > task_idx,
        "the cut advanced past the deep anchor (tail_start {})",
        b.tail_start
    );
    // Without a count ceiling the anchor still wins.
    let b_token = compute_boundary(&msgs, 4_000, None, EST);
    assert!(b_token.tail_start <= task_idx);
}

#[test]
fn boundary_never_splits_a_tool_pair() {
    for budget in [1_000usize, 2_000, 4_000, 8_000, 16_000] {
        let msgs = tool_heavy("task", 8, 2_000);
        let b = compute_boundary(&msgs, budget, None, EST);
        assert_ne!(
            msgs[b.tail_start]["role"].as_str(),
            Some("tool"),
            "budget {budget}: tail must not start inside a result group"
        );
    }
}

/// End-to-end through `compress`: with the cut landing between a call
/// and its results, the assembled output has no orphan halves.
#[tokio::test]
async fn compress_output_has_no_orphan_tool_pairs() {
    let msgs = tool_heavy("task", 8, 2_000);
    let mut state = CompressState::new();
    let out = run(&msgs, 2_500, None, None, &mut state).await;
    // Every assistant tool_calls group must be followed by exactly its
    // results (positional Ollama dialect: count successor tool messages).
    let m = &out.messages;
    for (i, msg) in m.iter().enumerate() {
        if let Some(tcs) = msg["tool_calls"].as_array() {
            let mut following = 0;
            for next in &m[i + 1..] {
                if next["role"].as_str() == Some("tool") {
                    following += 1;
                } else {
                    break;
                }
            }
            assert_eq!(
                following,
                tcs.len(),
                "message {i}: {} tool_calls need {} contiguous results",
                tcs.len(),
                tcs.len()
            );
        }
    }
}
