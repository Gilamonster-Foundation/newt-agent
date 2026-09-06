use super::*;

use super::test_support::{
    active_prompt_card, assistant_call, recording_summarizer, run, sys, tool_result, user, EST,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// bug/steering-regressions REGRESSION (live drives 2026-07-26/27, gpt-4.1
/// + Qwen3-Coder both): the operator states the REAL task, the harness
/// decision-surface asks for confirmation, and the operator's next turn is
/// pure ceremony ("1: proceed"). That NEW turn's active prompt — and thus
/// the protected active-prompt card — is the ceremony text, while the real
/// task is now just a prior-turn user message in the summarizable middle.
/// Mid-turn compaction then evicts the actual goal; the model keeps
/// working, on nothing ("context summarized: 13,628 → 11,805" was followed
/// by hunting hallucinated files in the live gpt-4.1 drive). The task must
/// survive compaction VERBATIM even when the current turn's active prompt
/// is a bare go-ahead.
#[tokio::test]
async fn prior_turn_task_survives_compaction_when_active_prompt_is_ceremony() {
    let real_task = "STEER-TASK-7c41: extract one cohesive #[cfg(test)] module \
             from newt-core/src/agentic/mod.rs into a sibling file by pure code \
             motion, keep the build green, then open exactly one PR.";
    let ceremony = "1: proceed";
    let mut msgs = vec![
        sys("you are newt"),
        user(real_task),
        serde_json::json!({
            "role": "assistant",
            "content": "I need these decisions locked before I can execute. \
                 Reply using an explicit ordinal: 1. Pick the single largest…"
        }),
        user(ceremony),
    ];
    // The long agentic middle: bulky read_file rounds dwarfing the budget.
    for i in 0..12 {
        msgs.push(assistant_call(
            "read_file",
            json!({"path": "newt-core/src/agentic/mod.rs", "offset": i * 500}),
        ));
        msgs.push(tool_result(&"m".repeat(4_000)));
    }
    // Drive the REAL seam the loop uses: receipts through the session
    // prompt store, the ceremony turn recorded as an operator
    // CONTINUATION of the task (exactly what chat.rs does for a pending
    // decision reply), then `active_text()` — the string mod.rs protects.
    let store = crate::agentic::prompt_read::SessionPromptStore::default();
    let task_turn = store
        .begin_prompt(
            "conv-steer",
            crate::prompt::NewPrompt::operator(real_task.as_bytes(), real_task.as_bytes()),
        )
        .expect("task receipt");
    let ceremony_turn = store
        .begin_prompt(
            "conv-steer",
            crate::prompt::NewPrompt::operator_continuation(
                ceremony.as_bytes(),
                ceremony.as_bytes(),
                task_turn.submitted_prompt().id(),
            ),
        )
        .expect("ceremony receipt");
    let active_task =
        crate::agentic::prompt_read::PromptReadContext::new(Some(&ceremony_turn), ceremony, None)
            .active_text();
    let protected = protect_active_prompt_for_compression(&msgs, active_task);
    let before = estimate_tokens(&protected, EST);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "## Summary\nreads happened");
    let mut state = CompressState::new();
    let out = compress(
        CompressRequest {
            rewrites_history: true,
            messages: &protected,
            budget: before / 4,
            max_messages: None,
            replay_protected_tail_len: 0,
            task: active_task,
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
    assert!(out.fired, "the oversized middle must trigger compaction");
    let visible: String = out
        .messages
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        visible.contains("STEER-TASK-7c41"),
        "the REAL task from the prior turn must survive compaction verbatim \
             even when the current turn's active prompt is decision ceremony \
             (\"{ceremony}\") — otherwise the agent keeps working with no goal. \
             Post-compaction visible content:\n{visible}"
    );
}

/// #319 REGRESSION GUARD: an API surface read EARLY then needed LATER is
/// summarized out of the middle (the freshest trailing group + ~budget/4
/// token tail are protected; an older read is not). The summary is prose,
/// so the verbatim signature is gone — but the fix appends a re-read
/// breadcrumb naming the dropped file, so the model is told to RE-READ it
/// rather than hallucinate. This guards that the breadcrumb names the file
/// and carries the directive.
#[tokio::test]
async fn summarized_file_reads_get_a_reread_breadcrumb() {
    let sig = "pub fn connect(&self, url: &str, timeout: Duration) -> Result<Session, ConnErr>";
    let api_body = format!(
        "pub struct ApiClient;\nimpl ApiClient {{\n    {sig} {{ todo!() }}\n}}\n{}",
        "// detail line\n".repeat(200)
    );
    let mut msgs = vec![
        sys("you are newt, a coding agent"),
        active_prompt_card(),
        user("ACTIVE TASK: implement reconnect() on ApiClient using its connect() method"),
        assistant_call("read_file", json!({ "path": "src/api.rs" })),
        tool_result(&api_body), // the API surface, read EARLY
    ];
    // ...then several more rounds of OTHER reads, pushing src/api.rs out of
    // both the freshest trailing group and the token-budgeted tail.
    for i in 0..8 {
        msgs.push(assistant_call(
            "read_file",
            json!({ "path": format!("src/other_{i}.rs") }),
        ));
        msgs.push(tool_result(&format!(
            "// other file {i}\n{}",
            "filler line\n".repeat(150)
        )));
    }
    let before = estimate_tokens(&msgs, EST);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    // The real summarizer returns PROSE, never code — model that.
    let s = recording_summarizer(
        prompts.clone(),
        "## Active Task\nImplement reconnect(). The agent earlier read src/api.rs \
             (defines ApiClient) and several other files.",
    );
    let mut state = CompressState::new();
    let out = run(&msgs, before / 2, None, Some(&*s), &mut state).await;

    let assembled: String = out
        .messages
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    eprintln!(
        "#319: fired={} action={:?}\n{}",
        out.fired,
        out.action,
        &assembled[..assembled.len().min(1200)]
    );
    // The summary fired (the early read did land in the compacted middle).
    assert!(out.fired && out.action == CompressAction::Summarized);
    // The fix: the model is TOLD the file is stale and must be re-read,
    // by name — not left to recall a fabricated signature from prose.
    assert!(
        assembled.contains("src/api.rs"),
        "the dropped file must be named so the model knows to re-read it"
    );
    assert!(
        assembled.contains("RE-READ") && assembled.contains("do NOT recall"),
        "the breadcrumb must carry the re-read / don't-recall directive"
    );
}

/// Working-set protection: the single MOST-RECENT `read_file` result is the
/// file the model is about to act on. If it lands in the summarized middle
/// it degrades to a "RE-READ" breadcrumb — and for a refactor target that
/// loops forever (read → summarized → re-read → summarized), which is the
/// steering-regressions ceiling the gauge surfaced 2026-07-27: a live drive
/// made 9 reads and ZERO edits because every target read was compacted away
/// before an edit could be emitted. The most-recent read must instead be
/// PINNED verbatim into the protected head so the model can edit from it.
#[tokio::test]
async fn most_recent_target_read_is_pinned_and_survives_compaction() {
    let target = "newt-core/src/agentic/mod.rs";
    let marker = "fn WORKING_SET_MARKER_edit_me()";
    let body = format!("{marker} {{\n{}}}\n", "    // body line\n".repeat(120));
    let mut msgs = vec![
        sys("you are newt"),
        active_prompt_card(),
        user("ACTIVE TASK: reduce mod.rs below 5000 lines by pure code motion"),
    ];
    // Older reads of OTHER files — legitimately breadcrumbed, not the
    // working set.
    for i in 0..6 {
        msgs.push(assistant_call(
            "read_file",
            json!({ "path": format!("src/other_{i}.rs") }),
        ));
        msgs.push(tool_result(&format!(
            "// other file {i}\n{}",
            "filler line\n".repeat(120)
        )));
    }
    // The TARGET read — the working set the next edit depends on.
    msgs.push(assistant_call("read_file", json!({ "path": target })));
    msgs.push(tool_result(&body));
    // NON-read bookkeeping AFTER it (plan/git/status): pushes the target
    // read out of the freshest trailing group WITHOUT superseding it as the
    // working set, so on current code it falls into the summarized middle.
    for i in 0..6 {
        msgs.push(assistant_call(
            "run_command",
            json!({ "cmd": format!("git status {i}") }),
        ));
        msgs.push(tool_result(&format!(
            "bookkeeping {i}\n{}",
            "status line\n".repeat(120)
        )));
    }

    let before = estimate_tokens(&msgs, EST);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    // The real summarizer returns PROSE, never the file body.
    let s = recording_summarizer(
        prompts.clone(),
        "## Active Task\nReduce mod.rs by code motion. The agent read several files.",
    );
    let mut state = CompressState::new();
    let out = run(&msgs, before / 3, None, Some(&*s), &mut state).await;

    let assembled: String = out
        .messages
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        out.fired && out.action == CompressAction::Summarized,
        "summary must fire for this to test protection (action={:?})",
        out.action
    );
    assert!(
        assembled.contains(marker),
        "the most-recent target read must be PINNED and survive compaction so \
             the model can edit from it instead of looping on re-reads; assembled:\n{}",
        &assembled[..assembled.len().min(1600)]
    );
}

/// The pin tracks the LATEST read: when the model moves on to a second
/// file, that file becomes the working set and the earlier one reverts to a
/// re-read breadcrumb. One card per round — a stale pin must not stick.
#[tokio::test]
async fn working_set_pin_tracks_the_latest_read_not_the_first() {
    let first = "src/first.rs";
    let second = "src/second.rs";
    let first_marker = "fn FIRST_FILE_MARKER()";
    let second_marker = "fn SECOND_FILE_MARKER()";
    let mut msgs = vec![
        sys("you are newt"),
        active_prompt_card(),
        user("ACTIVE TASK: refactor two files"),
    ];
    // Read the FIRST file, then bury it under bookkeeping.
    msgs.push(assistant_call("read_file", json!({ "path": first })));
    msgs.push(tool_result(&format!(
        "{first_marker} {{\n{}}}\n",
        "    // a\n".repeat(60)
    )));
    for i in 0..4 {
        msgs.push(assistant_call(
            "run_command",
            json!({ "cmd": format!("git a{i}") }),
        ));
        msgs.push(tool_result(&format!("bk {i}\n{}", "x\n".repeat(120))));
    }
    // Then read the SECOND file — the new working set — and bury it too.
    msgs.push(assistant_call("read_file", json!({ "path": second })));
    msgs.push(tool_result(&format!(
        "{second_marker} {{\n{}}}\n",
        "    // b\n".repeat(60)
    )));
    for i in 0..4 {
        msgs.push(assistant_call(
            "run_command",
            json!({ "cmd": format!("git b{i}") }),
        ));
        msgs.push(tool_result(&format!("bk2 {i}\n{}", "y\n".repeat(120))));
    }

    let before = estimate_tokens(&msgs, EST);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "## Active Task\nrefactor two files.");
    let mut state = CompressState::new();
    let out = run(&msgs, before / 3, None, Some(&*s), &mut state).await;

    let assembled: String = out
        .messages
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(out.fired && out.action == CompressAction::Summarized);
    // The latest read is pinned verbatim…
    assert!(
        assembled.contains(second_marker),
        "the latest read (second.rs) must be the pinned working set"
    );
    // …and its file must not also be told to re-read itself.
    assert!(
        !assembled.contains(&format!("- {second}")),
        "the pinned file must be excluded from the re-read breadcrumb"
    );
    // The earlier file is no longer the working set: its body is gone and it
    // is named in the breadcrumb to re-read instead.
    assert!(
        !assembled.contains(first_marker),
        "the superseded file's body must not linger as a second pin"
    );
}

#[tokio::test]
async fn knowledge_base_stable_base_survives_compression() {
    // #661 group E: the knowledge_base technique (FfiSurfaceProvider) injects
    // the authoritative import surface into the FROZEN system prompt. head_len
    // always protects leading system messages, so that stable base is NEVER
    // summarized — the summarizer has less to preserve, and the model keeps an
    // exact import surface to ground against. This guards that invariant
    // against a future boundary change that might evict the system prompt.
    let kb = "## Authoritative import surface\n\
                  from newt_agent._newt_agent.core import Router  # real path, not a guess";
    let mut msgs = vec![sys(kb), user("task")];
    for i in 0..24 {
        msgs.push(user(&format!("middle note {i} {}", "m".repeat(200))));
    }
    msgs.push(user("recent tail"));
    let mut state = CompressState::new();
    let out = run(&msgs, 300, None, None, &mut state).await;
    assert!(out.fired, "a large conversation should compress");
    assert!(
        out.messages.iter().any(|m| m["role"] == "system"
            && m["content"]
                .as_str()
                .is_some_and(|c| c.contains("from newt_agent._newt_agent.core import Router"))),
        "the knowledge_base import surface must survive compression VERBATIM \
             (the protected head — the stable base E relies on)"
    );
}
