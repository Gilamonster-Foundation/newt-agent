use super::*;

use super::test_support::{
    active_prompt_card, assistant_call, run, run_count_only, sys, tool_result, user, EST,
};
use serde_json::json;

/// The B6 shape with an AGED giant round: one giant tool round that no
/// boundary can split, followed by a newer small round — the final fit
/// pass one-lines the giant (aged) results under budget instead of
/// letting the backend silently truncate the head.
#[tokio::test]
async fn giant_aged_round_is_pruned_aggressively_not_shipped_over_budget() {
    let task = "ACTIVE TASK GAUNTLET-7f3d9c: summarize the three files";
    let mut msgs = vec![sys("you are newt"), active_prompt_card(), user(task)];
    msgs.push(json!({"role": "assistant", "content": "", "tool_calls": [
        {"function": {"name": "read_file", "arguments": {"path": "a.txt"}}},
        {"function": {"name": "read_file", "arguments": {"path": "b.txt"}}},
        {"function": {"name": "read_file", "arguments": {"path": "c.txt"}}},
    ]}));
    for _ in 0..3 {
        msgs.push(tool_result(&"z".repeat(50_000))); // ~12.5k tokens each
    }
    // The newer (fresh) round the model has not seen yet.
    msgs.push(assistant_call("read_file", json!({"path": "d.txt"})));
    msgs.push(tool_result("short fresh result"));
    let mut state = CompressState::new();
    let out = run(&msgs, 3_000, None, None, &mut state).await;
    assert!(
        out.tokens_after <= 3_000,
        "the fit pass must bring ~{} under budget, got {}",
        out.tokens_before,
        out.tokens_after
    );
    assert!(out.fired);
    // The task survives verbatim — the property B6 measured the loss of.
    assert!(out
        .messages
        .iter()
        .any(|m| m["content"].as_str() == Some(task)));
    // Pairing intact: 3 + 1 calls, 4 results (giants one-lined).
    assert_eq!(out.messages[3]["tool_calls"].as_array().unwrap().len(), 3);
    assert_eq!(
        out.messages
            .iter()
            .filter(|m| m["role"].as_str() == Some("tool"))
            .count(),
        4
    );
    // The fresh trailing result is untouched.
    assert_eq!(
        out.messages.last().unwrap()["content"].as_str(),
        Some("short fresh result")
    );
}

/// F1c: under SOFT (count-only / `/compress`) pressure the trailing tool
/// group — the fresh results the model has not seen yet — is NEVER
/// pruned, even when protecting it means the assembled list misses the
/// aim-to-halve target. (The old `keep_last: 0` fit pass one-lined the
/// freshest results pre-dispatch from the second compression of a
/// session on — the model could never read anything.) The HARD-budget
/// variant of this exact shape is #285's within-group reclaim, pinned by
/// `oversized_group_reclaims_within_keeping_newest_whole` below.
#[tokio::test]
async fn fresh_trailing_tool_group_survives_the_aggressive_pass() {
    let task = "ACTIVE TASK GAUNTLET-7f3d9c: summarize the three files";
    let big = "z".repeat(50_000);
    let mut msgs = vec![sys("you are newt"), user(task)];
    msgs.push(json!({"role": "assistant", "content": "", "tool_calls": [
        {"function": {"name": "read_file", "arguments": {"path": "a.txt"}}},
        {"function": {"name": "read_file", "arguments": {"path": "b.txt"}}},
        {"function": {"name": "read_file", "arguments": {"path": "c.txt"}}},
    ]}));
    for _ in 0..3 {
        msgs.push(tool_result(&big));
    }
    let mut state = CompressState::new();
    let out = run_count_only(&msgs, 3_000, None, None, &mut state).await;
    // All three fresh results reach the model byte-identical; the
    // over-target result is the accepted trade for a soft budget (a
    // missed aim-to-halve is not a correctness problem).
    let results: Vec<&str> = out
        .messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("tool"))
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(results.len(), 3);
    for r in results {
        assert_eq!(r, big, "fresh trailing tool results must never be pruned");
    }
    assert!(
        out.tokens_after > 3_000,
        "this shape is genuinely incompressible without destroying fresh results"
    );
}

// -- trailing-group protection (#270 / #285) -------------------------------

/// #270's root cause, pinned at the derivation: the protected suffix is
/// anchored on the last assistant-with-`tool_calls`, so an interleaved
/// user message (the read-only nudge) or a trailing compaction notice
/// can never truncate it. The old `take_while(role == "tool")` from the
/// end read 0 in both interleaved shapes.
#[test]
fn trailing_group_derivation_survives_interleaved_messages() {
    let mut msgs = vec![sys("you are newt"), user("task")];
    msgs.push(json!({"role": "assistant", "content": "", "tool_calls": [
        {"function": {"name": "read_file", "arguments": {"path": "a.rs"}}},
        {"function": {"name": "read_file", "arguments": {"path": "b.rs"}}},
    ]}));
    msgs.push(tool_result("result a"));
    msgs.push(tool_result("result b"));
    // Normal case: assistant turn + its two results.
    assert_eq!(trailing_tool_group_len(&msgs), 3);
    // The #270 repro: the read-only nudge lands AFTER the fresh results,
    // immediately before the compression call site.
    msgs.push(user(
        "[3 consecutive read-only rounds with no file writes.]",
    ));
    assert_eq!(trailing_tool_group_len(&msgs), 4);
    // A trailing compaction notice doesn't truncate the group either.
    msgs.push(summary_message("reference summary"));
    assert_eq!(trailing_tool_group_len(&msgs), 5);
    // A plain assistant reply (no tool_calls) does not re-anchor.
    msgs.push(json!({"role": "assistant", "content": "thinking…"}));
    assert_eq!(trailing_tool_group_len(&msgs), 6);
    // No assistant ever called a tool → no group.
    assert_eq!(trailing_tool_group_len(&[sys("s"), user("t")]), 0);
    // The loop appends the backend's `message` verbatim and some
    // dialects omit `role` on it — `tool_calls` alone anchors the group.
    let roleless = vec![
        user("task"),
        json!({"content": "", "tool_calls": [
                {"function": {"name": "read_file", "arguments": {"path": "a"}}}]}),
        tool_result("result a"),
    ];
    assert_eq!(trailing_tool_group_len(&roleless), 2);
}

#[test]
fn never_scope_protects_no_reasoning_tail() {
    use crate::model_card::ReasoningReplayScope;
    // A trailing assistant message carrying reasoning — a protectable tail.
    let msgs = vec![
        user("go"),
        json!({"role": "assistant", "reasoning_content": "thinking", "content": "answer"}),
    ];
    // The pure helper sees the tail...
    assert_eq!(reasoning_replay_tail_len(&msgs), 1);
    // ...but a Never-scope endpoint never replays it, so nothing is protected
    // (protecting it wasted compaction budget / blocked a count cap — the bug).
    assert_eq!(
        protected_reasoning_tail_len(&msgs, ReasoningReplayScope::Never),
        0
    );
    // ...while replay-capable scopes still protect it.
    assert_eq!(
        protected_reasoning_tail_len(&msgs, ReasoningReplayScope::CurrentUserTurn),
        1
    );
    assert_eq!(
        protected_reasoning_tail_len(&msgs, ReasoningReplayScope::FullHistory),
        1
    );
}

#[test]
fn reasoning_replay_tail_keeps_all_same_turn_tool_rounds_atomic() {
    let mut msgs = vec![
        sys("you are newt"),
        user("an older turn"),
        json!({"role": "assistant", "content": "older answer"}),
        user("the current task"),
    ];
    let first_reasoning = msgs.len();
    msgs.push(json!({
        "role": "assistant",
        "content": "<think>first private plan</think>",
        "reasoning_content": "first split plan",
        "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "a.rs"}}}]
    }));
    msgs.push(tool_result("first result"));
    // An unprefixed harness nudge currently looks like an ordinary user
    // message to the generic boundary logic.
    msgs.push(user("[Plan progress: 0/2 done. Keep working this step.]"));
    msgs.push(json!({
        "role": "assistant",
        "content": "",
        "reasoning_content": "second split plan",
        "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "b.rs"}}}]
    }));
    msgs.push(tool_result("second result"));

    let replay_protected_tail_len = reasoning_replay_tail_len(&msgs);
    let boundary =
        compute_boundary_with_protected_tail(&msgs, 100, Some(4), EST, replay_protected_tail_len);
    assert!(
        boundary.tail_start <= first_reasoning,
        "compression must not split the current-turn reasoning transcript (tail_start {})",
        boundary.tail_start
    );
    assert_eq!(
        trailing_tool_group_len_with_protected_tail(&msgs, replay_protected_tail_len,),
        msgs.len() - first_reasoning,
        "the aggressive pass must protect every reasoning-bearing tool round"
    );
    assert_eq!(
        compression_message_count(&msgs, replay_protected_tail_len),
        first_reasoning + 1,
        "count pressure must treat the replay transcript as one atomic item"
    );
}

#[test]
fn inline_reasoning_does_not_enable_generic_compression_protection() {
    let mut ordinary = vec![sys("you are newt"), user("the current task")];
    ordinary.push(json!({"role": "assistant", "content": "visible plan"}));
    for i in 0..8 {
        ordinary.push(user(&format!("follow-up {i}")));
        ordinary.push(json!({"role": "assistant", "content": format!("answer {i}")}));
    }
    let mut inline = ordinary.clone();
    inline[2]["content"] = json!("<think>private plan</think>visible plan");

    assert_eq!(
        compute_boundary(&inline, 100, Some(4), EST).tail_start,
        compute_boundary(&ordinary, 100, Some(4), EST).tail_start,
        "generic compression must not infer endpoint capabilities from message text"
    );
}

#[tokio::test]
async fn count_only_compression_preserves_the_full_reasoning_replay_tail() {
    let first_result = format!("FIRST-RESULT:{}", "x".repeat(600));
    let mut msgs = vec![sys("you are newt"), user("the current task")];
    msgs.push(json!({
        "role": "assistant",
        "content": "",
        "reasoning_content": "read every file before deciding",
        "tool_calls": [{"function": {
            "name": "read_file",
            "arguments": {"path": "first.rs"}
        }}]
    }));
    msgs.push(tool_result(&first_result));
    for i in 0..6 {
        msgs.push(assistant_call(
            "read_file",
            json!({"path": format!("later-{i}.rs")}),
        ));
        msgs.push(tool_result(&format!("later result {i}")));
    }

    let mut state = CompressState::new();
    let out = compress(
        CompressRequest {
            rewrites_history: true,
            messages: &msgs,
            budget: usize::MAX,
            max_messages: Some(4),
            replay_protected_tail_len: reasoning_replay_tail_len(&msgs),
            task: "the current task",
            hard_budget: false,
            authoritative: false,
            focus: None,
            est: EST,
            summary_input_cap_floor_chars: 8_192,
            compaction_store: None,
            compaction_stage: None,
        },
        None,
        &mut state,
    )
    .await;
    assert!(
        out.messages
            .iter()
            .any(|message| message["content"].as_str() == Some(first_result.as_str())),
        "count-only structural pruning must not rewrite an explicitly replayed tool result"
    );
}

/// The #270 repro through the whole pipeline: an over-budget session
/// whose fresh trailing group (two unseen results) is followed by the
/// read-only nudge's user message. Pre-fix the aggressive pass saw zero
/// trailing tools, floored `keep_last` at 2 ([UNSEEN2, nudge]), and
/// one-lined UNSEEN1 pre-dispatch — the probe measured 7,213 → 2,207
/// tokens with UNSEEN1 (8 KB) destroyed. Post-fix the whole group
/// survives byte-identical.
#[tokio::test]
async fn nudge_after_fresh_group_does_not_defeat_the_protection() {
    let task = "ACTIVE TASK GAUNTLET-7f3d9c: read both files then report";
    let unseen1 = format!("1:{}", "u".repeat(8_000));
    let unseen2 = format!("2:{}", "v".repeat(8_000));
    let mut msgs = vec![sys("you are newt"), user(task)];
    // Aged mass for the earlier passes to reclaim.
    for i in 0..6 {
        msgs.push(assistant_call(
            "read_file",
            json!({"path": format!("aged_{i}.rs")}),
        ));
        msgs.push(tool_result(&format!("{i}:{}", "a".repeat(8_000))));
    }
    // The fresh group: one assistant turn, two unseen results…
    msgs.push(json!({"role": "assistant", "content": "", "tool_calls": [
        {"function": {"name": "read_file", "arguments": {"path": "unseen1.rs"}}},
        {"function": {"name": "read_file", "arguments": {"path": "unseen2.rs"}}},
    ]}));
    msgs.push(tool_result(&unseen1));
    msgs.push(tool_result(&unseen2));
    // …then the read-only nudge, exactly where the loop injects it.
    msgs.push(user(
        "[3 consecutive read-only rounds with no file writes. \
             Stop exploring. Call edit_file or write_file now.]",
    ));
    let mut state = CompressState::new();
    // Soft (count-only) pressure: the F1c protection is absolute here —
    // the assembled list stays over the aim-to-halve target rather than
    // destroy an unseen result.
    let out = run_count_only(&msgs, 2_000, None, None, &mut state).await;
    assert!(out.fired);
    let tool_contents: Vec<&str> = out
        .messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("tool"))
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(
        tool_contents.contains(&unseen1.as_str()),
        "#270: UNSEEN1 must survive the nudge-truncated derivation \
             (got tool contents {:?})",
        tool_contents
            .iter()
            .map(|c| c.chars().take(40).collect::<String>())
            .collect::<Vec<_>>()
    );
    assert!(
        tool_contents.contains(&unseen2.as_str()),
        "UNSEEN2 must survive too"
    );
    // The nudge itself still reaches the model (nothing silently drops).
    assert!(out.messages.iter().any(|m| m["content"]
        .as_str()
        .is_some_and(|c| c.contains("read-only rounds"))));
    println!(
        "#270 repro trace: {} -> {} est. tokens (target {}), group intact",
        out.tokens_before, out.tokens_after, 2_000
    );
}

/// Same shape with a trailing compaction notice instead of the nudge —
/// the other interleaved-message family `is_compaction_message` covers.
#[tokio::test]
async fn compaction_notice_after_fresh_group_does_not_defeat_the_protection() {
    let task = "ACTIVE TASK GAUNTLET-7f3d9c: read both files then report";
    let unseen1 = format!("1:{}", "u".repeat(8_000));
    let unseen2 = format!("2:{}", "v".repeat(8_000));
    let mut msgs = vec![sys("you are newt"), user(task)];
    for i in 0..6 {
        msgs.push(assistant_call(
            "read_file",
            json!({"path": format!("aged_{i}.rs")}),
        ));
        msgs.push(tool_result(&format!("{i}:{}", "a".repeat(8_000))));
    }
    msgs.push(json!({"role": "assistant", "content": "", "tool_calls": [
        {"function": {"name": "read_file", "arguments": {"path": "unseen1.rs"}}},
        {"function": {"name": "read_file", "arguments": {"path": "unseen2.rs"}}},
    ]}));
    msgs.push(tool_result(&unseen1));
    msgs.push(tool_result(&unseen2));
    msgs.push(summary_message("## Active Task\nreference summary"));
    let mut state = CompressState::new();
    let out = run_count_only(&msgs, 2_000, None, None, &mut state).await;
    let tool_contents: Vec<&str> = out
        .messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("tool"))
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(tool_contents.contains(&unseen1.as_str()), "UNSEEN1 intact");
    assert!(tool_contents.contains(&unseen2.as_str()), "UNSEEN2 intact");
}

/// #285 mechanism, pinned at the helper: within-group reclaim fires ONLY
/// when the group by itself exceeds the budget left after everything
/// before it; one-lines oldest-first; stops as soon as the list fits;
/// the newest member is never a candidate.
#[test]
fn within_group_reclaim_fires_only_when_group_alone_exceeds() {
    let big = "z".repeat(20_000); // ~5k tokens
    let small = "s".repeat(1_200); // ~300 tokens
    let group = |contents: &[&str]| -> Vec<Value> {
        let mut msgs = vec![sys("you are newt"), user("task")];
        msgs.push(json!({"role": "assistant", "content": "", "tool_calls":
            contents.iter().enumerate().map(|(i, _)| json!(
                {"function": {"name": "read_file",
                              "arguments": {"path": format!("f{i}.txt")}}}
            )).collect::<Vec<_>>()
        }));
        msgs.extend(contents.iter().map(|c| tool_result(c)));
        msgs
    };

    // Under-budget group: untouched, returns false (the F1c property).
    let mut fits = group(&[&small, &small, &small]);
    let before = fits.clone();
    assert!(!reclaim_within_trailing_group(&mut fits, 10_000, EST, 0));
    assert_eq!(fits, before, "a group within its share is never touched");

    // No group at all: no-op.
    let mut no_group = vec![sys("s"), user(&big)];
    assert!(!reclaim_within_trailing_group(&mut no_group, 100, EST, 0));

    // Single-member group over budget: the newest IS the only member —
    // untouched, truthful over-budget residual (clipping inside one
    // result is out of scope).
    let mut single = group(&[&big]);
    let before = single.clone();
    assert!(!reclaim_within_trailing_group(&mut single, 1_000, EST, 0));
    assert_eq!(single, before);

    // Oversized group, early stop: one-lining the OLDEST member alone
    // fits the budget — the middle and newest members stay whole.
    let mut early = group(&[&big, &small, &small]);
    assert!(reclaim_within_trailing_group(&mut early, 1_500, EST, 0));
    let results: Vec<&str> = early
        .iter()
        .filter(|m| m["role"].as_str() == Some("tool"))
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(
        results[0].starts_with("[read_file] read 'f0.txt'"),
        "oldest one-lined with the re-read affordance: {}",
        results[0]
    );
    assert_eq!(results[1], small, "middle untouched after early stop");
    assert_eq!(results[2], small, "newest untouched");
    assert!(estimate_tokens(&early, EST) <= 1_500, "the list now fits");

    // Newest alone exceeds the budget: all older members one-lined, the
    // newest still whole, the list honestly stays over.
    let mut residual = group(&[&small, &small, &big]);
    assert!(reclaim_within_trailing_group(&mut residual, 1_000, EST, 0));
    let results: Vec<&str> = residual
        .iter()
        .filter(|m| m["role"].as_str() == Some("tool"))
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(results[0].starts_with("[read_file] read 'f0.txt'"));
    assert!(results[1].starts_with("[read_file] read 'f1.txt'"));
    assert_eq!(results[2], big, "the newest member is never a candidate");
    assert!(
        estimate_tokens(&residual, EST) > 1_000,
        "single-result-too-big: truthfully still over budget"
    );
}

/// #285 through the whole pipeline (the B6 residual measured in #284's
/// gauntlet): ONE round's tool group alone exceeds a HARD budget. The
/// F1c protection yields within the group: a.txt / b.txt one-lined
/// (each naming its file for re-read), c.txt — the newest — byte-
/// identical. Here even c.txt alone exceeds the budget, so the outcome
/// honestly stays over (the loop's notice reports real numbers) rather
/// than clipping inside the result.
#[tokio::test]
async fn oversized_group_reclaims_within_keeping_newest_whole() {
    let task = "ACTIVE TASK GAUNTLET-7f3d9c: summarize the three files";
    let big = "z".repeat(50_000); // ~12.5k tokens each
    let mut msgs = vec![sys("you are newt"), user(task)];
    msgs.push(json!({"role": "assistant", "content": "", "tool_calls": [
        {"function": {"name": "read_file", "arguments": {"path": "a.txt"}}},
        {"function": {"name": "read_file", "arguments": {"path": "b.txt"}}},
        {"function": {"name": "read_file", "arguments": {"path": "c.txt"}}},
    ]}));
    for _ in 0..3 {
        msgs.push(tool_result(&big));
    }
    let mut state = CompressState::new();
    let out = run(&msgs, 3_000, None, None, &mut state).await;
    assert!(out.fired);
    let results: Vec<&str> = out
        .messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("tool"))
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(results.len(), 3, "pairing intact — nothing dropped");
    assert!(
        results[0].starts_with("[read_file] read 'a.txt'"),
        "oldest one-lined, file named for re-read: {}",
        results[0]
    );
    assert!(
        results[1].starts_with("[read_file] read 'b.txt'"),
        "older one-lined in order: {}",
        results[1]
    );
    assert_eq!(results[2], big, "newest result reaches the model whole");
    // The task survives verbatim (the property B6 measured the loss of).
    assert!(out
        .messages
        .iter()
        .any(|m| m["content"].as_str() == Some(task)));
    // Honesty: the newest alone is ~12.5k tokens against a 3k budget —
    // the outcome reports genuinely over, never a silent fit claim.
    assert!(out.tokens_after > 3_000);
    assert!(
        out.tokens_after < out.tokens_before / 2,
        "but the reclaim was real: {} -> {}",
        out.tokens_before,
        out.tokens_after
    );
    println!(
        "#285 scenario trace: {} -> {} est. tokens (budget 3000), \
             a/b one-lined, c whole",
        out.tokens_before, out.tokens_after
    );
}

/// #285 boundary: when the group fits a HARD budget once everything
/// outside it is reclaimed, within-group reclaim must NOT fire — the
/// dispatch lands under budget with every fresh result intact.
#[tokio::test]
async fn under_budget_group_is_untouched_under_hard_pressure() {
    let task = "ACTIVE TASK GAUNTLET-7f3d9c: read both files then report";
    let unseen1 = format!("1:{}", "u".repeat(8_000)); // ~2k tokens
    let unseen2 = format!("2:{}", "v".repeat(8_000));
    let mut msgs = vec![sys("you are newt"), user(task)];
    for i in 0..6 {
        msgs.push(assistant_call(
            "read_file",
            json!({"path": format!("aged_{i}.rs")}),
        ));
        msgs.push(tool_result(&format!("{i}:{}", "a".repeat(8_000))));
    }
    msgs.push(json!({"role": "assistant", "content": "", "tool_calls": [
        {"function": {"name": "read_file", "arguments": {"path": "unseen1.rs"}}},
        {"function": {"name": "read_file", "arguments": {"path": "unseen2.rs"}}},
    ]}));
    msgs.push(tool_result(&unseen1));
    msgs.push(tool_result(&unseen2));
    msgs.push(user(
        "[3 consecutive read-only rounds with no file writes.]",
    ));
    let mut state = CompressState::new();
    // 6,000-token hard budget: the ~4.2k-token group fits once the aged
    // middle is summarized away.
    let out = run(&msgs, 6_000, None, None, &mut state).await;
    assert!(out.fired);
    assert!(
        out.tokens_after <= 6_000,
        "must land under the hard budget ({} -> {})",
        out.tokens_before,
        out.tokens_after
    );
    let tool_contents: Vec<&str> = out
        .messages
        .iter()
        .filter(|m| m["role"].as_str() == Some("tool"))
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert!(tool_contents.contains(&unseen1.as_str()), "UNSEEN1 whole");
    assert!(tool_contents.contains(&unseen2.as_str()), "UNSEEN2 whole");
}
