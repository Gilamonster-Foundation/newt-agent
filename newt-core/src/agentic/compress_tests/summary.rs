use super::*;

use super::test_support::{
    assistant_call, failing_summarizer, recording_summarizer, tool_heavy, tool_result, user, EST,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The summary request contains the original task verbatim, the lean
/// template sections, and the verbatim-Active-Task rule.
#[tokio::test]
async fn summary_request_carries_task_verbatim_and_template() {
    let task = "ACTIVE TASK GAUNTLET-7f3d9c: read ten files then report";
    let mut msgs = tool_heavy(task, 6, 4_000);
    msgs[2] = user(task);
    let before = estimate_tokens(&msgs, EST);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "SUMMARY");
    let mut state = CompressState::new();
    let out = compress(
        CompressRequest {
            rewrites_history: true,
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
            compaction_store: None,
            compaction_stage: None,
        },
        Some(&*s),
        &mut state,
    )
    .await;
    assert_eq!(out.action, CompressAction::Summarized);

    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    let p = &prompts[0];
    assert!(p.contains(task), "original task must appear verbatim: {p}");
    for section in [
        "## Active Task",
        "## Completed Actions",
        "## In Progress",
        "## Key Decisions",
        "## Relevant Files",
        "## Critical Context",
    ] {
        assert!(p.contains(section), "missing template section {section}");
    }
    assert!(p.contains("copied verbatim"), "verbatim-Active-Task rule");
    assert!(p.contains("[REDACTED]"), "redaction preamble present");
}

/// Summary hygiene: harness loop guidance and the model's echoes of it
/// are demoted to a one-line note in the summarizer INPUT — they are
/// process correction, not task state, and a 0.5B summarizer readily
/// echoes them into "## In Progress" (the 2026-07-08 ornith:35b stall's
/// summary contained "I keep describing … but never call tools").
#[test]
fn render_message_demotes_loop_guidance_and_narration_echo() {
    // A harness rescue nudge (tagged at its push site).
    let nudge = json!({
        "role": "user",
        "content": format!(
            "{LOOP_GUIDANCE_PREFIX} You described what you were about to \
             do but did not call any tool, so nothing actually happened."
        )
    });
    let r = render_message(&nudge);
    assert!(r.contains("omitted"), "{r}");
    assert!(!r.contains("did not call any tool"), "{r}");

    // The post-compaction continuation directive is likewise harness meta.
    let directive = json!({
        "role": "user",
        "content": format!("{CONTINUATION_PREFIX} You are mid-task…")
    });
    let r = render_message(&directive);
    assert!(r.contains("omitted"), "{r}");
    assert!(!r.contains("mid-task"), "{r}");

    // The model echoing the correction back is the other half of the pair.
    let echo = json!({
        "role": "assistant",
        "content": "The user is telling me I keep describing what I'm \
                    about to do but never call tools. I need to stop \
                    describing and start acting."
    });
    let r = render_message(&echo);
    assert!(r.contains("omitted"), "{r}");
    assert!(!r.contains("keep describing"), "{r}");

    // Analytical no-tool assistant content is task state — flows through.
    let analysis = json!({
        "role": "assistant",
        "content": "I found the issue: an extra closing brace at line 490."
    });
    let r = render_message(&analysis);
    assert!(r.contains("extra closing brace"), "{r}");

    // A tool-calling assistant message is never demoted, whatever it says.
    let acting = json!({
        "role": "assistant",
        "content": "I did not call any tool yet — doing it now.",
        "tool_calls": [{"function": {"name": "read_file", "arguments": {"path": "x"}}}]
    });
    let r = render_message(&acting);
    assert!(r.contains("read_file"), "{r}");
    assert!(r.contains("doing it now"), "{r}");

    // A plain operator interjection is untouched.
    let operator = json!({
        "role": "user",
        "content": "IMPORTANT: also update the docs"
    });
    let r = render_message(&operator);
    assert!(r.contains("update the docs"), "{r}");
}

/// The summarizer prompt carries the no-process-commentary rule (the
/// prompt-level half of the hygiene; the input filter above is the
/// deterministic half).
#[test]
fn summary_prompt_excludes_process_commentary() {
    let p = summary_prompt_for("task", "body", None, None, 1_200, ConvShape::Coding);
    assert!(
        p.contains("Do NOT include commentary about the assistant's own behavior"),
        "{p}"
    );
    assert!(p.contains("record only task state"), "{p}");
}

#[test]
fn middle_shape_detects_coding_vs_general() {
    // A4 (#661): a middle that issued tool calls is Coding; pure prose is General.
    let coding = vec![serde_json::json!({
        "role": "assistant",
        "tool_calls": [{"function": {"name": "edit_file", "arguments": "{}"}}],
    })];
    assert_eq!(middle_shape(&coding), ConvShape::Coding);
    let general = vec![
        serde_json::json!({"role": "user", "content": "what is a monad?"}),
        serde_json::json!({"role": "assistant", "content": "a monoid in ..."}),
    ];
    assert_eq!(middle_shape(&general), ConvShape::General);
}

#[test]
fn general_shape_swaps_the_section_template() {
    // A4 (#661): the General template drops file/action-centric slots for prose,
    // but both shapes keep the load-bearing Active Task / Critical Context.
    let coding = summary_prompt_for("t", "body", None, None, 600, ConvShape::Coding);
    assert!(coding.contains("## Completed Actions") && coding.contains("## Relevant Files"));
    let general = summary_prompt_for("t", "body", None, None, 600, ConvShape::General);
    assert!(general.contains("## Discussion") && general.contains("## Open Questions"));
    assert!(
        !general.contains("## Relevant Files"),
        "no file-centric slot for a Q&A middle"
    );
    assert!(general.contains("## Active Task") && general.contains("## Critical Context"));
    assert!(general.starts_with("You are compressing the middle of a conversation."));
}

/// F5: the rendered middle fed to the summarizer is capped in TOTAL —
/// the most recent middle survives, the oldest is dropped with an
/// explicit omission line (per-message caps alone don't bound a
/// 50-message middle).
#[test]
fn summary_request_caps_total_middle_size() {
    let middle: Vec<Value> = (0..50)
        .map(|i| tool_result(&format!("MSG{i} {}", "m".repeat(1_900))))
        .collect();
    let capped = summary_request("the task", &middle, 8_192, None, ConvShape::Coding);
    assert!(
        capped.chars().count() < 12_000,
        "total must be capped, got {}",
        capped.chars().count()
    );
    assert!(capped.contains("older message(s) omitted"), "{capped:.200}");
    assert!(capped.contains("MSG49 "), "most recent middle kept");
    assert!(!capped.contains("MSG0 "), "oldest middle dropped");
    assert!(capped.contains("the task"), "task always present");

    // Uncapped baseline for contrast: same middle, no cap.
    let uncapped = summary_request("the task", &middle, usize::MAX, None, ConvShape::Coding);
    assert!(uncapped.chars().count() > 90_000);
    assert!(!uncapped.contains("older message(s) omitted"));
}

// -- chunked / hierarchical summarization (Step 24.4, #559) -------------------

#[test]
fn chunk_strings_groups_consecutive_within_cap() {
    let parts: Vec<String> = ["aaa", "bbb", "ccc", "ddddddd"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // cap 6: aaa+bbb=6 ok; +ccc would be 9>6 → new chunk; ccc(3)+ddddddd(7)=10
    // >6 → new chunk; ddddddd alone is its own over-cap chunk.
    assert_eq!(
        chunk_strings(&parts, 6),
        vec![
            "aaabbb".to_string(),
            "ccc".to_string(),
            "ddddddd".to_string()
        ]
    );
    // Everything fits → a single chunk.
    assert_eq!(chunk_strings(&parts, 1_000).len(), 1);
}

/// The append-only preset must actually never rewrite. Same over-budget
/// input, two presets: `standard` compacts it, `append-only` refuses it and
/// hands the messages back **byte-identical**.
///
/// Without this the preset is a config knob that does nothing, which is worse
/// than not shipping it — an operator would believe their transcript was
/// untouched while it was being rewritten underneath them.
/// #1780: an over-cap prior summary must re-enter the summarizer with its
/// recovery handle and re-read breadcrumb INTACT.
///
/// These are appended LAST to a summary body, so head-first truncation removed
/// exactly the two affordances that exist for recovering what the summary
/// dropped — turning an addressed elision into an unmarked gap the model has no
/// way to notice. This test fails on the pre-fix code.
#[test]
fn an_over_cap_summary_keeps_its_recovery_handle() {
    let handle = "bafyr4ideadbeefcafe";
    let body = format!(
        "{SUMMARY_PREFIX}\n## Active Task\nfix the parser\n{}\n\n\
             [the full verbatim text of this compacted span is retrievable with \
             memory_fetch(\"compaction:{handle}\")]\n{SUMMARY_END_MARKER}",
        "padding that pushes this summary well past the input cap. ".repeat(120),
    );
    assert!(
        body.chars().count() > SUMMARY_INPUT_MSG_CAP,
        "fixture must exceed the cap or it proves nothing"
    );

    let rendered = render_message(&json!({ "role": "user", "content": body }));

    assert!(
        rendered.contains(handle),
        "the compaction handle was truncated out of the summarizer input"
    );
    assert!(
        rendered.contains("memory_fetch"),
        "the recovery directive was truncated out"
    );
    assert!(
        rendered.contains("## Active Task"),
        "the head must survive too — the task lives there"
    );
    assert!(
        rendered.contains("elided"),
        "the cut must be marked, not silent"
    );
}

/// The tail-preserving path is for compaction summaries only; everything else
/// keeps the cheaper head-first behaviour.
#[test]
fn ordinary_messages_still_truncate_head_first() {
    let body = format!(
        "ORDINARY {}TAILMARKER",
        "x".repeat(SUMMARY_INPUT_MSG_CAP * 2)
    );
    let rendered = render_message(&json!({ "role": "user", "content": body }));
    assert!(rendered.contains("ORDINARY"));
    assert!(
        !rendered.contains("TAILMARKER"),
        "a non-summary message should not have gained tail preservation"
    );
}

#[tokio::test]
async fn summarize_middle_single_request_when_it_fits() {
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "SUMMARY");
    let middle = vec![user("alpha"), user("beta")];
    let out = summarize_middle(&*s, "do the task", &middle, 100_000, None).await;
    assert_eq!(out.as_deref(), Some("SUMMARY"));
    assert_eq!(prompts.lock().unwrap().len(), 1, "fits → one request");
}

#[tokio::test]
async fn summarize_middle_chunks_and_reduces_when_over_cap() {
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "PART");
    // Six ~1000-char messages (~6k rendered) against a 2,500-char cap →
    // several bounded chunks + a reduce pass, covering the WHOLE middle.
    let big = "x".repeat(1_000);
    let middle: Vec<Value> = (0..6).map(|_| user(&big)).collect();
    let out = summarize_middle(&*s, "do the task", &middle, 2_500, None).await;
    assert_eq!(out.as_deref(), Some("PART"), "result is the reduce output");
    let p = prompts.lock().unwrap();
    assert!(
        p.len() > 1,
        "over-cap middle is chunked: {} requests",
        p.len()
    );
    assert!(
        p.iter().any(|r| r.contains("[part 1/")),
        "chunks carry part labels"
    );
    assert!(
        p.iter().any(|r| r.contains("consolidate")),
        "a reduce/consolidation pass ran"
    );
    // Every request stays bounded (cap + prompt-template overhead) — the
    // whole point: no single request can OOM the summarizer.
    assert!(
        p.iter().all(|r| r.chars().count() < 2_500 + 2_000),
        "each request stays under the cap (+ template)"
    );
}

#[tokio::test]
async fn summarize_middle_all_chunks_fail_degrades_to_none() {
    let calls = Arc::new(AtomicUsize::new(0));
    let s = failing_summarizer(calls.clone());
    let big = "x".repeat(1_000);
    let middle: Vec<Value> = (0..6).map(|_| user(&big)).collect();
    let out = summarize_middle(&*s, "task", &middle, 2_500, None).await;
    assert!(out.is_none(), "all chunks failing → None (→ static marker)");
    assert!(
        calls.load(Ordering::SeqCst) >= 3,
        "every chunk was attempted, got {}",
        calls.load(Ordering::SeqCst)
    );
}

// -- rendering ---------------------------------------------------------------

#[test]
fn render_message_includes_calls_and_caps_content() {
    let m = assistant_call("read_file", json!({"path": "src/lib.rs"}));
    let line = render_message(&m);
    assert!(line.starts_with("[assistant] called read_file("), "{line}");
    assert!(line.contains("src/lib.rs"), "{line}");

    let long = tool_result(&"w".repeat(10_000));
    let line = render_message(&long);
    assert!(
        line.chars().count() < SUMMARY_INPUT_MSG_CAP + 50,
        "{}",
        line.len()
    );
    assert!(line.contains('…'));
}
