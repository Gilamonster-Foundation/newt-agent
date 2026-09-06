use super::*;

use super::test_support::{recording_summarizer, sys, user, EST};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// Default estimation (chars_per_token = 4) for the unit tests.
/// The `[context]` policy a manual-compress test wants: defaults, with the
/// rewrite policy under test.
fn manual_policy(rewrites_history: bool) -> ManualCompressPolicy {
    ManualCompressPolicy {
        est: EST,
        est_cap_floor_chars: 8_192,
        rewrites_history,
    }
}

// -- user-initiated (`/compress`, Step 18.6) ------------------------------

/// Provider-shaped chat history (no tool messages): system, the task,
/// then `turns` user/assistant pairs of `chars` characters each.
fn chat_history(turns: usize, chars: usize) -> Vec<Value> {
    let mut msgs = vec![sys("you are newt"), user("ORIGINAL TASK: port the parser")];
    for i in 0..turns {
        msgs.push(user(&format!("q{i} {}", "u".repeat(chars))));
        msgs.push(json!({"role": "assistant",
                             "content": format!("a{i} {}", "v".repeat(chars))}));
    }
    msgs
}

/// `/compress` compresses with NO token pressure (the user asked): the
/// soft aim-to-halve request fires, the message count shrinks, the
/// marked summary is present, and the run records into the counters.
#[tokio::test]
async fn user_initiated_compresses_without_token_pressure() {
    let msgs = chat_history(10, 400);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "## Active Task\nMANUAL SUMMARY");
    let mut state = CompressState::new();
    let out =
        compress_user_initiated(&msgs, None, Some(&*s), &mut state, manual_policy(true)).await;

    assert!(out.fired);
    assert_eq!(out.how, CompressAction::Summarized.describe());
    assert_eq!(out.messages_before, msgs.len());
    assert_eq!(out.messages_after, out.messages.len());
    assert!(
        out.messages_after < out.messages_before,
        "count must shrink"
    );
    assert!(out.tokens_after < out.tokens_before);
    assert!(
        out.messages.iter().any(|m| is_compaction_message(m)
            && m["content"].as_str().unwrap().contains("MANUAL SUMMARY")),
        "marked summary message must be present"
    );
    // Compatibility mode anchors the most recent real user request, never
    // the first historical ask.
    let p = prompts.lock().unwrap();
    assert!(p[0].contains("q9 "), "{}", p[0]);
    // Fired manual runs feed the effectiveness counters.
    let c = state.counters();
    assert_eq!(c.compressions, 1);
    assert_eq!(c.strikes, 0, "a good reclaim is not a strike");
    assert!(c.last_reclaim.unwrap() > THRASH_MIN_SAVINGS);
    assert!(!c.disabled);
}

#[tokio::test]
async fn manual_compression_explicitly_anchors_b_and_leaks_no_prompt_pair() {
    let mut msgs = vec![
        sys("you are newt"),
        user("TASK-A: inspect ambient servers"),
        json!({"role": "assistant", "content": "A complete"}),
    ];
    for i in 0..10 {
        msgs.push(user(&format!("historical {i} {}", "x".repeat(300))));
        msgs.push(json!({
            "role": "assistant",
            "content": format!("reply {i} {}", "y".repeat(300))
        }));
    }
    let task_b = "TASK-B: implement the durable prompt ledger";
    msgs.push(user(task_b));
    msgs.push(json!({"role": "assistant", "content": "working on B"}));

    let prompts = Arc::new(Mutex::new(Vec::new()));
    let summarizer = recording_summarizer(prompts.clone(), "B SUMMARY");
    let mut state = CompressState::new();
    let out = compress_user_initiated_for_task(
        &msgs,
        task_b,
        None,
        Some(&*summarizer),
        &mut state,
        manual_policy(true),
    )
    .await;

    assert!(out.fired);
    let request = &prompts.lock().unwrap()[0];
    let task_section = request
        .split("## Original Task")
        .nth(1)
        .expect("shared prompt carries an original-task section")
        .split("## Conversation middle")
        .next()
        .unwrap_or_default();
    assert!(task_section.contains(task_b), "{request}");
    assert!(!task_section.contains("TASK-A"), "{request}");
    assert!(out.messages.iter().all(|message| {
        !message["content"]
            .as_str()
            .is_some_and(|text| text.starts_with(ACTIVE_PROMPT_PREFIX))
    }));
}

#[tokio::test]
async fn manual_compression_never_strips_a_prefix_colliding_system_prompt_or_live_ask() {
    let task = "CURRENT live operator ask";
    let collision = format!("{ACTIVE_PROMPT_PREFIX}\nconfigured system text, not a card");
    let mut msgs = vec![sys(&collision)];
    for i in 0..10 {
        msgs.push(user(&format!("historical {i} {}", "x".repeat(300))));
        msgs.push(json!({
            "role": "assistant",
            "content": format!("reply {i} {}", "y".repeat(300))
        }));
    }
    msgs.push(user(task));

    let summarizer = recording_summarizer(Arc::new(Mutex::new(Vec::new())), "SUMMARY");
    let mut state = CompressState::new();
    let out = compress_user_initiated_for_task(
        &msgs,
        task,
        None,
        Some(&*summarizer),
        &mut state,
        manual_policy(true),
    )
    .await;

    assert!(out.fired);
    assert!(out.messages.iter().any(|message| {
        message["role"] == "system" && message["content"].as_str() == Some(collision.as_str())
    }));
    assert!(out
        .messages
        .iter()
        .any(|message| { message["role"] == "user" && message["content"].as_str() == Some(task) }));
}

/// The `/compress <focus>` topic reaches the summarizer as emphasis
/// guidance — with a credential typed into the focus REDACTED before the
/// request is assembled (the same pass the rendered middle gets).
#[tokio::test]
async fn user_initiated_focus_is_threaded_and_redacted() {
    let msgs = chat_history(10, 400);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "SUMMARY");
    let mut state = CompressState::new();
    let secret = "sk-aaaaaaaaaaaaaaaaaaaaaaaa1234";
    let focus = format!("the auth flow around {secret} handling");
    let out = compress_user_initiated(
        &msgs,
        Some(&focus),
        Some(&*s),
        &mut state,
        manual_policy(true),
    )
    .await;
    assert!(out.fired);

    let p = prompts.lock().unwrap();
    assert_eq!(p.len(), 1);
    assert!(
        p[0].contains("emphasize anything about"),
        "focus guidance line missing: {}",
        p[0]
    );
    assert!(p[0].contains("the auth flow around"), "{}", p[0]);
    assert!(
        !p[0].contains(secret),
        "a secret typed into the focus must never reach the summarizer"
    );
    assert!(p[0].contains("[REDACTED]"));
}

/// No focus ⇒ no emphasis guidance in the request (the loop's automatic
/// requests must be byte-identical to pre-18.6 ones).
#[tokio::test]
async fn no_focus_means_no_guidance_line() {
    let msgs = chat_history(10, 400);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "SUMMARY");
    let mut state = CompressState::new();
    compress_user_initiated(&msgs, None, Some(&*s), &mut state, manual_policy(true)).await;
    assert!(!prompts.lock().unwrap()[0].contains("emphasize anything about"));
}

/// An incompressible working set is a honest no-op: nothing fired,
/// nothing recorded — repeated `/compress` on a tiny session must never
/// strike out auto-compression for later.
#[tokio::test]
async fn user_initiated_noop_records_nothing() {
    let msgs = vec![sys("you are newt"), user("task"), user("note")];
    let mut state = CompressState::new();
    for _ in 0..3 {
        let out = compress_user_initiated(&msgs, None, None, &mut state, manual_policy(true)).await;
        assert!(!out.fired, "nothing to reclaim — must not fire");
        assert_eq!(out.messages, msgs);
        assert_eq!(out.tokens_before, out.tokens_after);
        assert!(out.notice.is_none());
    }
    let c = state.counters();
    assert_eq!(c.compressions, 0, "no-op runs never count");
    assert_eq!(c.strikes, 0);
    assert!(!c.disabled);
    assert_eq!(c.last_reclaim, None);
}

/// `/compress` still runs after anti-thrash latched auto-compression off
/// — the latch gates the automatic hard-budget guard, not an explicit
/// user ask (the soft request never consults it).
#[tokio::test]
async fn user_initiated_runs_while_latched() {
    let msgs = chat_history(10, 400);
    let mut state = CompressState::new();
    state.latch_disabled_for_tests();
    let out = compress_user_initiated(&msgs, None, None, &mut state, manual_policy(true)).await;
    assert!(out.fired, "an explicit ask must bypass the latch");
    assert_eq!(out.how, CompressAction::StaticFallback.describe());
    assert!(state.is_disabled(), "the latch itself stays set");
}
