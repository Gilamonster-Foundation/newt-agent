use super::*;

// -- /compress (Step 18.6, #247) ------------------------------------------

#[test]
fn compress_commands_parse_expected_focus() {
    assert_eq!(parse_compress_command("/compress").unwrap(), None);
    assert_eq!(parse_compress_command("/compress   ").unwrap(), None);
    assert_eq!(
        parse_compress_command("/compress auth token handling").unwrap(),
        Some("auth token handling".into())
    );
    // The focus is opaque free text: FTS5-hostile operators and a
    // secret-looking string parse fine — redaction is the pipeline's
    // job, not the parser's.
    assert_eq!(
        parse_compress_command("/compress AND \"NEAR/2\" sk-aaaaaaaaaaaaaaaaaaaaaaaa1234").unwrap(),
        Some("AND \"NEAR/2\" sk-aaaaaaaaaaaaaaaaaaaaaaaa1234".into())
    );
    // `/compressx` is some other (unknown) command, not `/compress x`.
    assert!(parse_compress_command("/compressx").is_err());
    assert!(parse_compress_command("/memory").is_err());
}

/// A session memory with `turns` fat user/assistant turns — enough
/// summarizable middle for the pipeline to fire without token pressure.
async fn compressible_memory(turns: usize) -> newt_core::MemoryManager {
    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::RollingWindow::new(50));
    memory
        .sync_all_with_active_task(
            "ORIGINAL TASK: port the parser",
            "starting on it",
            &newt_core::TurnMetrics::default(),
            "ORIGINAL TASK: port the parser",
        )
        .await;
    for i in 0..turns {
        memory
            .sync_all_with_active_task(
                &format!("question {i} {}", "u".repeat(300)),
                &format!("answer {i} {}", "v".repeat(300)),
                &newt_core::TurnMetrics::default(),
                &format!("question {i} {}", "u".repeat(300)),
            )
            .await;
    }
    memory
}

/// The command's real parts end to end: wire view → shared pipeline →
/// honesty feedback whose numbers match the actual outcome → write-back,
/// so the NEXT turn really sends the compressed working set.
#[tokio::test]
async fn manual_compress_shrinks_session_and_notice_is_truthful() {
    let mut memory = compressible_memory(12).await;
    let system = "you are newt";
    let wire = session_wire_view(&memory, system);
    assert!(
        wire.last().is_some_and(|m| m["role"] == "assistant"),
        "the empty task slot must be popped from the wire view"
    );
    let before_len = wire.len();

    let summarizer: newt_core::Summarizer =
        Box::new(|_req: String| -> newt_core::SummarizeFuture {
            Box::pin(async { Ok("## Active Task\nMANUAL SUMMARY".to_string()) })
        });
    let mut state = newt_core::CompressState::new();
    let outcome = newt_core::compress_user_initiated(
        &wire,
        None,
        Some(&*summarizer),
        &mut state,
        newt_core::ManualCompressPolicy {
            est: Default::default(),
            est_cap_floor_chars: 8_192,
            rewrites_history: true,
        },
    )
    .await;

    assert!(outcome.fired);
    assert_eq!(outcome.messages_before, before_len);
    assert!(outcome.messages_after < outcome.messages_before);
    assert!(outcome.tokens_after < outcome.tokens_before);

    // The notice numbers are the outcome's numbers — no independent
    // arithmetic that could drift from what actually happened.
    let msg = compress_feedback_message(&outcome);
    assert!(
        msg.contains(&format!(
            "context compressed: {} → {} messages, ~{} → ~{} est. tokens",
            outcome.messages_before,
            outcome.messages_after,
            outcome.tokens_before,
            outcome.tokens_after
        )),
        "got: {msg}"
    );
    assert!(msg.contains("prune + summary"), "got: {msg}");
    assert!(!msg.contains("note: no token savings"), "got: {msg}");

    // Write-back through the existing replace seam: the next build is
    // the compressed set (marker included), not the raw history.
    memory.restore_turns(&wire_messages_to_turns(&outcome.messages));
    let next = memory.build_messages(system, "next task");
    assert!(
        next.len() < before_len,
        "next turn must send the compressed set"
    );
    assert!(next.iter().any(
        |m| m.content.starts_with(newt_core::agentic::SUMMARY_PREFIX)
            && m.content.contains("MANUAL SUMMARY")
    ));
    // The fired manual run shows up in the /memory counters.
    assert_eq!(state.counters().compressions, 1);
}

/// No-op honesty: an incompressible session reports "no compression
/// possible" and never claims savings.
#[tokio::test]
async fn manual_compress_noop_reports_no_compression_possible() {
    let mut memory = newt_core::MemoryManager::new();
    memory.add_provider(newt_core::RollingWindow::new(50));
    memory
        .sync_all_with_active_task("hi", "hello", &newt_core::TurnMetrics::default(), "hi")
        .await;
    let wire = session_wire_view(&memory, "you are newt");
    let mut state = newt_core::CompressState::new();
    let outcome = newt_core::compress_user_initiated(
        &wire,
        None,
        None,
        &mut state,
        newt_core::ManualCompressPolicy {
            est: Default::default(),
            est_cap_floor_chars: 8_192,
            rewrites_history: true,
        },
    )
    .await;

    assert!(!outcome.fired);
    let msg = compress_feedback_message(&outcome);
    assert!(msg.contains("no compression possible"), "got: {msg}");
    assert!(
        !msg.contains("context compressed"),
        "must not claim savings that didn't happen: {msg}"
    );
    assert_eq!(state.counters().compressions, 0);
}

/// Fired-but-no-token-savings gets the explicit hermes honesty note
/// instead of an implied win.
#[test]
fn compress_feedback_flags_fired_without_token_savings() {
    let outcome = newt_core::ManualCompressOutcome {
        messages: Vec::new(),
        fired: true,
        messages_before: 10,
        messages_after: 6,
        tokens_before: 800,
        tokens_after: 850,
        how: "prune + summary",
        notice: None,
    };
    let msg = compress_feedback_message(&outcome);
    assert!(msg.contains("10 → 6 messages"), "got: {msg}");
    assert!(msg.contains("note: no token savings"), "got: {msg}");
}

/// A secret typed into the focus never reaches the summarizer request —
/// the focus rides the same redaction the rendered middle gets.
#[tokio::test]
async fn compress_focus_secret_never_reaches_summarizer() {
    let memory = compressible_memory(12).await;
    let wire = session_wire_view(&memory, "you are newt");
    let prompts = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = prompts.clone();
    let summarizer: newt_core::Summarizer =
        Box::new(move |req: String| -> newt_core::SummarizeFuture {
            let seen = seen.clone();
            Box::pin(async move {
                seen.lock().unwrap().push(req);
                Ok("SUMMARY".to_string())
            })
        });
    let mut state = newt_core::CompressState::new();
    let secret = "sk-aaaaaaaaaaaaaaaaaaaaaaaa1234";
    let focus = format!("the login flow around {secret}");
    let outcome = newt_core::compress_user_initiated(
        &wire,
        Some(&focus),
        Some(&*summarizer),
        &mut state,
        newt_core::ManualCompressPolicy {
            est: Default::default(),
            est_cap_floor_chars: 8_192,
            rewrites_history: true,
        },
    )
    .await;
    assert!(outcome.fired, "the summarizer path must have run");

    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    assert!(
        prompts[0].contains("emphasize anything about"),
        "{}",
        prompts[0]
    );
    assert!(prompts[0].contains("the login flow"), "{}", prompts[0]);
    assert!(
        !prompts[0].contains(secret),
        "focus secret leaked into the summarizer request"
    );
    assert!(prompts[0].contains("[REDACTED]"));
}

#[test]
fn memory_compress_section_renders_states() {
    // Fresh session: nothing recorded, enabled, no reclaim figure.
    let fresh = memory_compress_section(&newt_core::CompressCounters {
        compressions: 0,
        strikes: 0,
        disabled: false,
        last_reclaim: None,
    });
    assert!(fresh.contains("compressions this session: 0"), "{fresh}");
    assert!(!fresh.contains("last reclaimed"), "{fresh}");
    assert!(fresh.contains("strikes: 0/2"), "{fresh}");
    assert!(fresh.contains("auto-compression: enabled"), "{fresh}");
    assert!(
        !fresh.contains("/new resets it"),
        "the reset hint shows only when latched: {fresh}"
    );

    // Post-compression: count + last reclaim percentage surface.
    let post = memory_compress_section(&newt_core::CompressCounters {
        compressions: 2,
        strikes: 1,
        disabled: false,
        last_reclaim: Some(0.07),
    });
    assert!(post.contains("compressions this session: 2"), "{post}");
    assert!(post.contains("(last reclaimed 7%)"), "{post}");
    assert!(post.contains("strikes: 1/2"), "{post}");
    assert!(post.contains("auto-compression: enabled"), "{post}");

    // Latched: disabled status with the truthful "/new resets it" hint
    // (true since #267's F4 — `handle_new_conversation` resets the state).
    let latched = memory_compress_section(&newt_core::CompressCounters {
        compressions: 3,
        strikes: 2,
        disabled: true,
        last_reclaim: Some(0.04),
    });
    assert!(latched.contains("strikes: 2/2"), "{latched}");
    assert!(latched.contains("auto-compression: disabled"), "{latched}");
    assert!(latched.contains("/new resets it"), "{latched}");

    // A negative reclaim (the pass GREW the estimate) is never clamped
    // into a "0% reclaimed" savings claim.
    let grew = memory_compress_section(&newt_core::CompressCounters {
        compressions: 1,
        strikes: 1,
        disabled: false,
        last_reclaim: Some(-0.06),
    });
    assert!(grew.contains("grew the estimate 6%"), "{grew}");
    assert!(!grew.contains("last reclaimed"), "{grew}");
}

#[test]
fn wire_messages_to_turns_pairs_and_lone_sides() {
    let compaction = format!("{}\nsummary body", newt_core::agentic::SUMMARY_PREFIX);
    let wire = vec![
        serde_json::json!({"role": "system", "content": "you are newt"}),
        serde_json::json!({"role": "user", "content": "the task"}),
        serde_json::json!({"role": "user", "content": compaction}),
        serde_json::json!({"role": "user", "content": "q1"}),
        serde_json::json!({"role": "assistant", "content": "a1"}),
    ];
    let turns = wire_messages_to_turns(&wire);
    // System dropped; task and compaction stand alone; q1/a1 pair up —
    // and the compaction is never mistaken for q-awaiting-reply.
    assert_eq!(turns.len(), 3);
    assert_eq!((&*turns[0].user, &*turns[0].assistant), ("the task", ""));
    assert_eq!(
        (&*turns[1].user, &*turns[1].assistant),
        (compaction.as_str(), "")
    );
    assert_eq!((&*turns[2].user, &*turns[2].assistant), ("q1", "a1"));
    // Token columns stay absent: these are no longer measured turns.
    assert!(turns
        .iter()
        .all(|t| t.tokens_in.is_none() && t.tokens_out.is_none()));
}
