use super::*;

use super::test_support::{
    assistant_call, failing_summarizer, recording_summarizer, run, run_count_only, sys, tool_heavy,
    tool_result, user, EST,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// -- pipeline order -------------------------------------------------------

/// Under budget → untouched, no anti-thrash accounting.
#[tokio::test]
async fn within_budget_is_a_noop() {
    let msgs = tool_heavy("task", 2, 100);
    let mut state = CompressState::new();
    let out = run(&msgs, 100_000, None, None, &mut state).await;
    assert_eq!(out.action, CompressAction::Fit);
    assert!(!out.fired);
    assert_eq!(out.messages, msgs);
    assert_eq!(state.attempts, 0, "a no-op never counts as a compression");
}

/// Prune-first short-circuit: when the structural passes reclaim enough,
/// the summarizer is never invoked (zero LLM cost).
#[tokio::test]
async fn prune_short_circuits_when_sufficient() {
    // 14 messages: 2 aged huge identical results (dedupe + one-liner
    // fodder) + 10 protected-tail fillers.
    let big = "y".repeat(8_000);
    let mut msgs = vec![
        sys("you are newt"),
        user("task"),
        assistant_call("run_command", json!({"command": "cargo test"})),
        tool_result(&big),
        assistant_call("run_command", json!({"command": "cargo test"})),
        tool_result(&big),
    ];
    for i in 0..10 {
        msgs.push(user(&format!("filler {i}")));
    }
    let before = estimate_tokens(&msgs, EST);
    let budget = before - 1_000; // prune reclaims ~4k tokens — plenty
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "SUMMARY");
    let mut state = CompressState::new();
    let out = run(&msgs, budget, None, Some(&*s), &mut state).await;
    assert_eq!(out.action, CompressAction::Pruned);
    assert!(out.fired);
    assert!(out.tokens_after <= budget);
    assert_eq!(out.messages.len(), msgs.len(), "prune never drops messages");
    assert!(
        prompts.lock().unwrap().is_empty(),
        "summarizer must not be called when pruning suffices"
    );
}

/// Prune insufficient → the middle is summarized; head + tail survive
/// verbatim, markers wrap the summary, the old placeholder is gone.
#[tokio::test]
async fn summarizes_middle_with_markers_when_prune_insufficient() {
    let msgs = tool_heavy("ACTIVE TASK GAUNTLET-7f3d9c: do the thing", 6, 4_000);
    let before = estimate_tokens(&msgs, EST);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "## Active Task\nGAUNTLET summary");
    let mut state = CompressState::new();
    let out = run(&msgs, before / 3, None, Some(&*s), &mut state).await;

    assert_eq!(out.action, CompressAction::Summarized);
    assert!(out.fired);
    assert!(out.tokens_after < before);
    // Head anchored verbatim.
    assert_eq!(out.messages[0], msgs[0]);
    assert_eq!(out.messages[1], msgs[1]);
    assert_eq!(out.messages[2], msgs[2]);
    // The summary message carries both markers and the summary body.
    let summary = out.messages[3]["content"].as_str().unwrap();
    assert!(summary.starts_with(SUMMARY_PREFIX), "{summary}");
    assert!(summary.contains("GAUNTLET summary"), "{summary}");
    assert!(summary.contains(SUMMARY_END_MARKER), "{summary}");
    // The old amputation placeholder must be gone from this path.
    assert!(
        !out.messages.iter().any(|m| m["content"]
            .as_str()
            .is_some_and(|c| c.contains("earlier tool-call messages omitted"))),
        "the old placeholder-discard line must not appear"
    );
}

/// No summarizer → static fallback marker with the exact removed count.
#[tokio::test]
async fn no_summarizer_uses_static_fallback_marker() {
    let msgs = tool_heavy("task", 6, 4_000);
    let before = estimate_tokens(&msgs, EST);
    let mut state = CompressState::new();
    let out = run(&msgs, before / 3, None, None, &mut state).await;
    assert_eq!(out.action, CompressAction::StaticFallback);
    let summary = out.messages[3]["content"].as_str().unwrap();
    assert!(summary.starts_with(SUMMARY_PREFIX), "{summary}");
    assert!(summary.contains(SUMMARY_END_MARKER), "{summary}");
    // middle = messages [2, tail_start): compute the expected count from
    // the output shape (protected pair head + marker + tail).
    let removed = msgs.len() - (out.messages.len() - 1);
    assert!(
        summary.contains(&format!(
            "Summary generation was unavailable. {removed} message(s) were removed."
        )),
        "{summary}"
    );
}

/// Summarizer failure → static marker; the pipeline never errors out.
#[tokio::test]
async fn summarizer_failure_falls_back_to_static_marker() {
    let msgs = tool_heavy("task", 6, 4_000);
    let before = estimate_tokens(&msgs, EST);
    let calls = Arc::new(AtomicUsize::new(0));
    let s = failing_summarizer(calls.clone());
    let mut state = CompressState::new();
    let out = run(&msgs, before / 3, None, Some(&*s), &mut state).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "summarizer was attempted");
    assert_eq!(out.action, CompressAction::StaticFallback);
    let summary = out.messages[3]["content"].as_str().unwrap();
    assert!(summary.contains("Summary generation was unavailable."));
}

/// An empty/whitespace summary counts as a failure (static marker).
#[tokio::test]
async fn empty_summary_falls_back_to_static_marker() {
    let msgs = tool_heavy("task", 6, 4_000);
    let before = estimate_tokens(&msgs, EST);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "  \n ");
    let mut state = CompressState::new();
    let out = run(&msgs, before / 3, None, Some(&*s), &mut state).await;
    assert_eq!(out.action, CompressAction::StaticFallback);
}

/// The count trigger (`max_messages`) forces the summary stage even when
/// tokens already fit — pruning can never reduce the message count.
#[tokio::test]
async fn max_messages_forces_summary_stage() {
    let msgs = tool_heavy("task", 8, 50); // small payloads: tokens fit
    let before = estimate_tokens(&msgs, EST);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "SUMMARY");
    let mut state = CompressState::new();
    let out = run_count_only(&msgs, before + 1_000, Some(8), Some(&*s), &mut state).await;
    assert_eq!(out.action, CompressAction::Summarized);
    assert!(out.messages.len() < msgs.len());
}

/// F1 (the headline regression): a SECOND compression of an already-
/// compressed conversation must still shrink it. The bug anchored the
/// boundary on the first pass's own summary message, the middle went
/// empty, the count never dropped, and the fit pass destroyed every
/// fresh tool result pre-dispatch from then on.
#[tokio::test]
async fn second_compression_still_shrinks_and_keeps_fresh_results() {
    let fresh = format!("9:{}", "x".repeat(4_000));
    let msgs = tool_heavy("fix the failing test", 10, 4_000);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let s = recording_summarizer(prompts.clone(), "SUMMARY ONE");
    let mut state = CompressState::new();
    let budget = estimate_tokens(&msgs, EST) / 2;
    let first = run_count_only(&msgs, budget, Some(8), Some(&*s), &mut state).await;
    assert!(first.messages.len() < msgs.len(), "first pass shrinks");
    assert!(first.messages.iter().any(is_compaction_message));

    // Six more rounds land on top of the compressed list.
    let mut grown = first.messages.clone();
    for i in 10..16 {
        grown.push(assistant_call(
            "read_file",
            json!({"path": format!("src/file_{i}.rs")}),
        ));
        grown.push(tool_result(&format!("{i}:{}", "x".repeat(4_000))));
    }
    let grown_fresh = grown.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string();
    let budget2 = estimate_tokens(&grown, EST) / 2;
    let second = run_count_only(&grown, budget2, Some(8), Some(&*s), &mut state).await;
    assert!(
        second.messages.len() < grown.len(),
        "second compression must still shrink ({} -> {})",
        grown.len(),
        second.messages.len()
    );
    assert!(
        second.messages.len() <= 10,
        "count goal must stay reachable, got {}",
        second.messages.len()
    );
    // The freshest tool result reaches the model intact, both passes.
    assert_eq!(
        first.messages.last().unwrap()["content"].as_str(),
        Some(fresh.as_str()),
        "first pass fresh result intact"
    );
    assert_eq!(
        second.messages.last().unwrap()["content"].as_str(),
        Some(grown_fresh.as_str()),
        "second pass fresh result intact"
    );
    // Count-only passes never feed anti-thrash (F2).
    assert!(!state.disabled);
    assert_eq!(state.attempts, 0);
}

/// Red (#2875 part 2): when the summarizer stalls and the static fallback
/// marker is used, the marker must still name the files written and the
/// commits made in the removed span, so the model is not left blind to them.
#[tokio::test]
async fn fallback_marker_names_touched_files_and_commits() {
    let big = "z".repeat(4_000);
    let mut msgs = vec![
        sys("you are newt"),
        user("task"),
        assistant_call(
            "edit_file",
            json!({"path": "src/a.rs", "old_string": "a", "new_string": "b"}),
        ),
        tool_result("ok"),
        assistant_call("run_command", json!({"command": "git commit -m x"})),
        tool_result("committed"),
    ];
    for i in 0..10 {
        msgs.push(user(&format!("filler {i} {big}")));
        msgs.push(assistant_call("run_command", json!({"command": "ls"})));
        msgs.push(tool_result(&big));
    }
    let before = estimate_tokens(&msgs, EST);
    let calls = Arc::new(AtomicUsize::new(0));
    let s = failing_summarizer(calls.clone());
    let mut state = CompressState::new();
    let out = run(&msgs, before / 3, None, Some(&*s), &mut state).await;
    assert_eq!(
        out.action,
        CompressAction::StaticFallback,
        "{:?}",
        out.action
    );
    let summary = out
        .messages
        .iter()
        .filter_map(|m| m["content"].as_str())
        .find(|c| c.contains("Summary generation was unavailable."))
        .unwrap_or_else(|| panic!("no marker in {:#?}", out.messages));
    assert!(summary.contains("src/a.rs"), "names the file: {summary}");
    assert!(
        summary.contains("git commit -m x"),
        "names the commit: {summary}"
    );
}

/// Red (#2875 r2, finding 2): the fallback digest must pass through the same
/// redaction as the summarizer input — a credential in a command prefix must
/// not reach the marker.
#[tokio::test]
async fn fallback_marker_redacts_secrets_in_commands() {
    let token = format!("ghp_{}", "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8");
    let mut msgs = vec![sys("you are newt"), user("task")];
    msgs.push(assistant_call(
        "run_command",
        json!({"command": format!("GH_TOKEN={token} git commit -m fix")}),
    ));
    msgs.push(tool_result("committed"));
    for i in 0..10 {
        msgs.push(user(&format!("filler {i} {}", "z".repeat(4_000))));
        msgs.push(assistant_call("run_command", json!({"command": "ls"})));
        msgs.push(tool_result(&"z".repeat(4_000)));
    }
    let before = estimate_tokens(&msgs, EST);
    let calls = Arc::new(AtomicUsize::new(0));
    let s = failing_summarizer(calls);
    let mut state = CompressState::new();
    let out = run(&msgs, before / 3, None, Some(&*s), &mut state).await;
    let marker = out
        .messages
        .iter()
        .filter_map(|m| m["content"].as_str())
        .find(|c| c.contains("Summary generation was unavailable."))
        .unwrap_or_else(|| panic!("no marker"));
    assert!(!marker.contains(&token), "secret leaked: {marker}");
}

/// Red (#2875 r2, finding 3): one oversized commit message must not inflate
/// the marker. Calls the digest builder directly (compress_tests is a child of
/// compress, so the private fn is reachable). Covers ASCII and multibyte input;
/// each entry and the whole digest are capped by character, never splitting a
/// char.
#[test]
fn fallback_marker_caps_oversized_digest() {
    let huge = "m".repeat(200_000);
    let multibyte = "日本語😀é".repeat(50_000);
    let removed = vec![
        assistant_call(
            "run_command",
            json!({"command": format!("git commit -m {huge}")}),
        ),
        assistant_call(
            "run_command",
            json!({"command": format!("git commit -m {multibyte}")}),
        ),
        assistant_call("edit_file", json!({"path": format!("src/{multibyte}.rs")})),
    ];
    let marker = static_fallback_text(&removed);
    assert!(marker.contains("Summary generation was unavailable."));
    let chars = marker.chars().count();
    assert!(chars <= 2_000 + 1, "marker is {chars} chars");
    assert!(marker.contains("git commit -m"), "commit entry missing");
}

/// Red (#2875 r2, finding 5): Git global options must not hide a commit.
#[test]
fn fallback_marker_lists_git_option_commit() {
    let removed = vec![assistant_call(
        "run_command",
        json!({"command": "git -C repo commit -m fix"}),
    )];
    let marker = static_fallback_text(&removed);
    assert!(
        marker.contains("git -C repo commit -m fix"),
        "commit missing: {marker}"
    );
}

/// Red (#2875 r2, finding 5): a write whose tool result reports failure must
/// not be listed as a file touched.
#[test]
fn fallback_marker_omits_failed_write() {
    let removed = vec![
        assistant_call("edit_file", json!({"path": "src/a.rs"})),
        tool_result("error: permission denied"),
    ];
    let marker = static_fallback_text(&removed);
    assert!(
        !marker.contains("src/a.rs"),
        "failed write reported as touched: {marker}"
    );
}

/// The marker text out of a `run()` whose summarizer always fails.
async fn fallback_marker_of(msgs: &[Value]) -> String {
    let before = estimate_tokens(msgs, EST);
    let s = failing_summarizer(Arc::new(AtomicUsize::new(0)));
    let mut state = CompressState::new();
    let out = run(msgs, before / 3, None, Some(&*s), &mut state).await;
    out.messages
        .iter()
        .filter_map(|m| m["content"].as_str())
        .find(|c| c.contains("Summary generation was unavailable."))
        .unwrap_or_else(|| panic!("no marker in {:#?}", out.messages))
        .to_string()
}

/// `filler` rounds that push the earlier span out of the protected tail.
fn push_filler(msgs: &mut Vec<Value>, n: usize) {
    for i in 0..n {
        msgs.push(user(&format!("filler {i} {}", "z".repeat(4_000))));
        msgs.push(assistant_call("run_command", json!({"command": "ls"})));
        msgs.push(tool_result(&"z".repeat(4_000)));
    }
}

/// Red (#2875 r2 round 3, finding 1): prune pass 2 one-lines aged results over
/// 200 chars as `[edit_file] edited '<p>' -> error, …` before the digest reads
/// them, and a fence denial starts `capability denied:`. A denied write must
/// not be listed under "Files touched".
#[tokio::test]
async fn fallback_marker_omits_write_denied_by_the_fence_after_prune() {
    let denial = format!("capability denied: write to src/a.rs. {}", "x".repeat(300));
    let mut msgs = vec![
        sys("you are newt"),
        user("task"),
        assistant_call("edit_file", json!({"path": "src/a.rs"})),
        tool_result(&denial),
    ];
    push_filler(&mut msgs, 10);
    let marker = fallback_marker_of(&msgs).await;
    assert!(
        !marker.lines().any(|l| l.starts_with("Files touched")),
        "denied write listed: {marker}"
    );
}

/// Red (#2875 r2 round 3, finding 3): collection keeps the NEWEST entries, so
/// a commit made after more than DIGEST_CAP other commands still reaches the
/// marker.
#[test]
fn fallback_marker_keeps_a_late_commit_after_many_commands() {
    let mut removed = Vec::new();
    for i in 0..25 {
        removed.push(assistant_call(
            "run_command",
            json!({"command": format!("ls {i}")}),
        ));
        removed.push(tool_result("a b c"));
    }
    removed.push(assistant_call(
        "run_command",
        json!({"command": "git -C repo commit -m x"}),
    ));
    removed.push(tool_result("committed"));
    let marker = static_fallback_text(&removed);
    assert!(
        marker.contains("git -C repo commit -m x"),
        "late commit dropped: {marker}"
    );
    // Distinct commands: the oldest were evicted, the newest kept.
    assert!(!marker.contains("ls 0;"), "oldest not evicted: {marker}");
    assert!(marker.contains("ls 24;"), "newest evicted: {marker}");
}

/// A re-touched path moves to the back, so it survives eviction of older paths.
#[test]
fn fallback_marker_keeps_a_retouched_file_among_the_newest() {
    let mut removed = vec![assistant_call("edit_file", json!({"path": "src/first.rs"}))];
    removed.push(tool_result("ok"));
    for i in 0..19 {
        removed.push(assistant_call(
            "edit_file",
            json!({"path": format!("src/f{i}.rs")}),
        ));
        removed.push(tool_result("ok"));
    }
    removed.push(assistant_call("edit_file", json!({"path": "src/first.rs"})));
    removed.push(tool_result("ok"));
    removed.push(assistant_call("edit_file", json!({"path": "src/last.rs"})));
    removed.push(tool_result("ok"));
    let marker = static_fallback_text(&removed);
    assert!(
        marker.contains("src/first.rs"),
        "retouched dropped: {marker}"
    );
    assert!(!marker.contains("src/f0.rs"), "oldest kept: {marker}");
}

/// Guard (not red — the code already passes): a registered session secret in a
/// command must be value-filtered out of the marker. Mirrors
/// `redact_secrets_value_filters_a_registered_session_secret`.
#[test]
fn fallback_marker_value_filters_a_registered_secret() {
    let canary = "NEWT-CANARY-9f3a2b7c1d4e";
    let mut f = crate::ocap::DisclosureFilter::new();
    f.register(canary);
    let _g = crate::ocap::scoped_session_disclosure(f);
    let removed = vec![
        assistant_call(
            "run_command",
            json!({"command": format!("curl -H 'X: {canary}' https://example.test")}),
        ),
        tool_result("ok"),
    ];
    let marker = static_fallback_text(&removed);
    assert!(marker.contains("curl"), "command missing: {marker}");
    assert!(
        !marker.contains(canary),
        "registered secret leaked: {marker}"
    );
}

/// Guard (#2875 r2 round 3): the whole digest is capped at DIGEST_TOTAL_CAP
/// characters even when every entry sits at the per-entry cap.
#[test]
fn fallback_marker_total_digest_is_capped() {
    let mut removed = Vec::new();
    for i in 0..20 {
        removed.push(assistant_call(
            "run_command",
            json!({"command": format!("cmd{i} {}", "q".repeat(400))}),
        ));
        removed.push(tool_result("ok"));
        removed.push(assistant_call(
            "edit_file",
            json!({"path": format!("src/{i}/{}.rs", "p".repeat(400))}),
        ));
        removed.push(tool_result("ok"));
    }
    let marker = static_fallback_text(&removed);
    let chars = marker.chars().count();
    assert!(chars <= DIGEST_TOTAL_CAP + 1, "marker is {chars} chars");
    assert!(marker.ends_with('…'), "total cap not applied: {marker}");
}

/// Red (#2875 r2 round 3): `(failed)` and the exit code come AFTER the excerpt,
/// so a long command still shows its outcome. #2875 asks for exit codes; the
/// result text carries `error: command exited N`.
#[test]
fn fallback_marker_shows_outcome_and_exit_code_after_a_long_command() {
    let removed = vec![
        assistant_call(
            "run_command",
            json!({"command": format!("cargo test {}", "x".repeat(300))}),
        ),
        tool_result("error: command exited 101\nerror[E0308]: mismatched types"),
        assistant_call("run_command", json!({"command": "make"})),
        tool_result("error: command exited 2"),
    ];
    let marker = static_fallback_text(&removed);
    assert!(
        marker.contains("… (failed, exit 101)"),
        "long command outcome lost: {marker}"
    );
    assert!(marker.contains("make (failed, exit 2)"), "{marker}");
}

/// Red (#2875 r2 round 4, finding 1): the whole-digest cap must drop the OLDEST
/// entries, not truncate the tail. Twenty distinct ~110-char commands overflow
/// the cap, and the commit — the newest — must survive.
#[test]
fn fallback_marker_total_cap_keeps_the_newest_command() {
    let mut removed = Vec::new();
    for i in 0..19 {
        let cmd = format!(
            "cargo test -p some-crate --features f{i} -- --test-threads=1 {}",
            "p".repeat(50)
        );
        removed.push(assistant_call("run_command", json!({ "command": cmd })));
        removed.push(tool_result("ok"));
    }
    let commit = format!("git -C repo commit -m {}", "m".repeat(85));
    removed.push(assistant_call("run_command", json!({ "command": commit })));
    removed.push(tool_result("committed"));
    let marker = static_fallback_text(&removed);
    assert!(marker.chars().count() <= 2_000 + 1, "marker over cap");
    assert!(
        marker.contains("git -C repo commit"),
        "newest commit cut: {marker}"
    );
    assert!(
        !marker.contains("--features f0 "),
        "oldest command kept: {marker}"
    );
}

/// Red (#2875 r2 round 4, finding 3): prune pass 1 rewrites an aged repeat of an
/// identical long result to `[duplicate of message N: …]`. A retried, identical,
/// long fence denial must not turn the write into a touched file.
#[tokio::test]
async fn fallback_marker_omits_a_retried_long_denial_collapsed_to_a_duplicate_marker() {
    let denial = format!("capability denied: write to src/a.rs. {}", "x".repeat(300));
    let mut msgs = vec![sys("you are newt"), user("task")];
    for _ in 0..2 {
        msgs.push(assistant_call("edit_file", json!({"path": "src/a.rs"})));
        msgs.push(tool_result(&denial));
    }
    push_filler(&mut msgs, 10);
    let marker = fallback_marker_of(&msgs).await;
    assert!(
        !marker.lines().any(|l| l.starts_with("Files touched")),
        "retried denial listed: {marker}"
    );
}
