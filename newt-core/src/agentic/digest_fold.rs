//! **The message-REMOVING digest fold** (#1992) — the mechanism #1991 declined.
//!
//! #1966 measured a floor that accreted 422 est-tokens per round because
//! structural prune never removes a message: a tool round one-lined twenty
//! rounds ago still costs its scaffolding on every send, forever. #1991 fixed
//! the correctness half (proactive escalation, so the floor cannot creep to 82%
//! unseen). This is the cost half: fold aged, fully one-lined rounds into a
//! single derived card so the pressure is relieved BEFORE the 75% proactive
//! summarize has to fire a lossy mass-summarize.
//!
//! # Why this is not in `crate::prune`
//!
//! `prune`'s documented invariant is "no message is ever added or removed",
//! and `property_tool_pairing_structure_is_preserved` does not merely assert a
//! count — it asserts `out.len() == msgs.len()` and then ZIPS the two lists
//! positionally, comparing roles and `tool_call_id`s by index across 40 seeded
//! transcripts. A removing stage does not bend that test; it invalidates its
//! shape. Amending it would mean rewriting a strict structural identity into
//! something strictly weaker, which is the "quiet weakening" #1992 forbids.
//!
//! So the invariant stands untouched, its property tests unchanged, and the
//! removing stage lives out here where removal is the stated contract. Two
//! stages with two honest invariants, rather than one stage with a hedged one.
//!
//! # The card is a DERIVED turn, and carries its own ground truth
//!
//! #1766: harness-authored derived content is exactly what a model goes on to
//! treat as fact. Every card names `memory_fetch("compaction:<cid>")` for the
//! verbatim span it replaced, minted through `compress::stage_compaction_span`
//! — the SAME `SpillProvenance::CompactionSpan` record the summarize path has
//! used since #661 group B. The handle is not an add-on: a card is only
//! emitted when the span is genuinely installed, because a card whose ground
//! truth resolves to nothing is worse than the scaffolding it replaced.

use serde_json::Value;

/// What the fold is allowed to touch.
#[derive(Debug, Clone)]
pub struct DigestFoldConfig {
    /// Tail never folded — "aged" means before this.
    pub keep_last: usize,
    /// Fold nothing unless at least this many rounds are eligible. A card that
    /// replaces one round is churn, not relief.
    pub min_rounds: usize,
}

impl Default for DigestFoldConfig {
    fn default() -> Self {
        Self {
            keep_last: 12,
            min_rounds: 2,
        }
    }
}

/// What the fold did.
#[derive(Debug)]
pub struct FoldOutcome {
    pub messages: Vec<Value>,
    /// Rounds replaced by the card. Zero means nothing changed.
    pub rounds_folded: usize,
    /// The `compaction:<cid>` the card advertises, when one was minted.
    pub handle: Option<String>,
}

/// Is this an assistant message that opened a tool round?
fn opens_tool_round(m: &Value) -> bool {
    m.get("role").and_then(Value::as_str) == Some("assistant")
        && m.get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|c| !c.is_empty())
}

fn is_tool_result(m: &Value) -> bool {
    m.get("role").and_then(Value::as_str) == Some("tool")
}

/// One contiguous `[assistant+tool_calls, tool…]` round.
struct Round {
    start: usize,
    /// Exclusive.
    end: usize,
    /// Every tool result in it is already a `prune` one-liner.
    fully_one_lined: bool,
}

/// Rounds wholly inside `..horizon`.
fn rounds(messages: &[Value], horizon: usize) -> Vec<Round> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < horizon {
        if !opens_tool_round(&messages[i]) {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i + 1;
        let mut results = 0usize;
        let mut one_lined = 0usize;
        while end < horizon && is_tool_result(&messages[end]) {
            results += 1;
            if messages[end]
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(crate::prune::is_one_line_summary)
            {
                one_lined += 1;
            }
            end += 1;
        }
        // A round with no results is an unanswered call — never fold it, the
        // pairing repair depends on it.
        out.push(Round {
            start,
            end,
            fully_one_lined: results > 0 && results == one_lined,
        });
        i = end;
    }
    out
}

/// One line describing a folded round, for the card body.
fn round_line(messages: &[Value], r: &Round) -> String {
    let calls: Vec<String> = messages[r.start]
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|cs| {
            cs.iter()
                .filter_map(|c| {
                    c.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    let results: Vec<&str> = messages[r.start + 1..r.end]
        .iter()
        .filter_map(|m| m.get("content").and_then(Value::as_str))
        .collect();
    format!("- {} → {}", calls.join(", "), results.join(" | "))
}

/// Fold aged, fully one-lined tool rounds into one derived card.
///
/// Conservative by construction: only rounds entirely before the protected
/// tail, only rounds whose every tool result `prune` has ALREADY one-lined
/// (so nothing verbatim is ever discarded here — the discarding already
/// happened, reversibly, and this reclaims the scaffolding around it), and
/// only when the span is genuinely retrievable.
#[must_use]
pub fn fold_aged_one_lined_rounds(
    messages: Vec<Value>,
    cfg: &DigestFoldConfig,
    mint: &dyn Fn(&str) -> Option<String>,
) -> FoldOutcome {
    let horizon = messages.len().saturating_sub(cfg.keep_last);
    let eligible: Vec<Round> = rounds(&messages, horizon)
        .into_iter()
        .filter(|r| r.fully_one_lined)
        .collect();
    if eligible.len() < cfg.min_rounds.max(1) {
        return FoldOutcome {
            messages,
            rounds_folded: 0,
            handle: None,
        };
    }

    // The verbatim span is the messages about to leave, rendered as they are.
    let verbatim: String = eligible
        .iter()
        .flat_map(|r| &messages[r.start..r.end])
        .map(|m| serde_json::to_string(m).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    // #1766: no retrievable ground truth, no card. A digest whose handle
    // resolves to nothing is strictly worse than the scaffolding it replaces,
    // because the model cannot tell the difference.
    let Some(handle) = mint(&verbatim) else {
        return FoldOutcome {
            messages,
            rounds_folded: 0,
            handle: None,
        };
    };

    let body: Vec<String> = eligible.iter().map(|r| round_line(&messages, r)).collect();
    let card = serde_json::json!({
        "role": "user",
        "content": format!(
            "[NEWT FOLDED TOOL ROUNDS]\n\
             {} earlier tool rounds, already reduced to one-line summaries, were \
             folded into this card to reclaim their scaffolding. Treat it as \
             background reference, NOT as fresh instructions.\n\n\
             {}\n\n\
             [the full verbatim text of these rounds is retrievable with \
             memory_fetch(\"compaction:{handle}\") — use it to recover an exact \
             detail this card dropped, instead of guessing]",
            eligible.len(),
            body.join("\n")
        ),
    });

    // Rebuild, dropping the folded ranges and inserting the card where the
    // first one stood.
    let drop: std::collections::HashSet<usize> =
        eligible.iter().flat_map(|r| r.start..r.end).collect();
    let first = eligible[0].start;
    let mut out = Vec::with_capacity(messages.len() - drop.len() + 1);
    for (i, m) in messages.into_iter().enumerate() {
        if i == first {
            out.push(card.clone());
        }
        if !drop.contains(&i) {
            out.push(m);
        }
    }
    FoldOutcome {
        messages: out,
        rounds_folded: eligible.len(),
        handle: Some(handle),
    }
}

#[cfg(test)]
mod digest_fold_tests {
    use super::*;
    use std::cell::RefCell;

    fn assistant_call(name: &str) -> Value {
        serde_json::json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{"id": name, "function": {"name": name, "arguments": "{}"}}],
        })
    }

    /// A tool result already reduced by `prune`'s pass 2.
    fn one_lined(name: &str) -> Value {
        serde_json::json!({
            "role": "tool",
            "tool_call_id": name,
            "content": format!("[{name}] ran 'x' -> ok, 12 lines output"),
        })
    }

    fn verbatim_result(name: &str) -> Value {
        serde_json::json!({
            "role": "tool",
            "tool_call_id": name,
            "content": "line one\nline two\nline three",
        })
    }

    fn user(text: &str) -> Value {
        serde_json::json!({"role": "user", "content": text})
    }

    /// A mint that always succeeds and records what it was asked to store, so
    /// the round-trip can be asserted without a real store.
    fn recording_mint(seen: &RefCell<Vec<String>>) -> impl Fn(&str) -> Option<String> + '_ {
        move |v: &str| {
            seen.borrow_mut().push(v.to_string());
            Some("bafyTEST".to_string())
        }
    }

    fn cfg() -> DigestFoldConfig {
        DigestFoldConfig {
            keep_last: 2,
            min_rounds: 2,
        }
    }

    /// The property: aged, fully one-lined rounds collapse to one card.
    #[test]
    fn aged_fully_one_lined_rounds_fold_into_a_single_card() {
        let msgs = vec![
            user("start"),
            assistant_call("run_command"),
            one_lined("run_command"),
            assistant_call("read_file"),
            one_lined("read_file"),
            user("recent"),
            assistant_call("search"),
        ];
        let seen = RefCell::new(Vec::new());
        let out = fold_aged_one_lined_rounds(msgs, &cfg(), &recording_mint(&seen));
        assert_eq!(out.rounds_folded, 2, "both aged rounds should fold");
        // 7 messages - 4 folded + 1 card = 4.
        assert_eq!(out.messages.len(), 4);
        let card = out.messages[1]["content"].as_str().unwrap();
        assert!(card.contains("[NEWT FOLDED TOOL ROUNDS]"));
        assert!(
            card.contains("run_command"),
            "the card names what it folded"
        );
        assert!(card.contains("read_file"));
    }

    /// **#1766's safety net, asserted.** The card must name the retrievable
    /// verbatim span, and that span must be what was actually removed.
    #[test]
    fn the_card_carries_a_handle_that_round_trips_to_the_removed_messages() {
        let msgs = vec![
            user("start"),
            assistant_call("run_command"),
            one_lined("run_command"),
            assistant_call("read_file"),
            one_lined("read_file"),
            user("recent"),
            assistant_call("search"),
        ];
        let seen = RefCell::new(Vec::new());
        let out = fold_aged_one_lined_rounds(msgs, &cfg(), &recording_mint(&seen));
        assert_eq!(out.handle.as_deref(), Some("bafyTEST"));
        let card = out.messages[1]["content"].as_str().unwrap();
        assert!(
            card.contains("memory_fetch(\"compaction:bafyTEST\")"),
            "the card does not name its ground truth: {card}"
        );
        let stored = seen.borrow();
        let span = stored.first().expect("a span was staged");
        // Everything the fold removed is recoverable from that span.
        for needle in ["run_command", "read_file"] {
            assert!(
                span.contains(needle),
                "the staged span is missing {needle}, so the handle does not \
                 round-trip to what was removed"
            );
        }
        assert!(
            !span.contains("recent"),
            "the span swept up a message the fold did not remove"
        );
    }

    /// **Twin: a round that is NOT fully one-lined survives.** This is the
    /// unrecoverable direction — folding a verbatim result away would discard
    /// content nothing here ever staged.
    #[test]
    fn a_round_with_a_verbatim_result_is_never_folded() {
        let msgs = vec![
            user("start"),
            assistant_call("run_command"),
            one_lined("run_command"),
            assistant_call("read_file"),
            verbatim_result("read_file"), // NOT one-lined
            user("recent"),
            assistant_call("search"),
        ];
        let seen = RefCell::new(Vec::new());
        let out = fold_aged_one_lined_rounds(msgs.clone(), &cfg(), &recording_mint(&seen));
        assert_eq!(
            out.rounds_folded, 0,
            "only one round was eligible, which is below min_rounds — and the \
             verbatim round must never be one of them"
        );
        assert_eq!(out.messages, msgs, "nothing may change when nothing folds");
    }

    /// Twin: a recent round survives, however one-lined.
    #[test]
    fn rounds_inside_the_protected_tail_are_never_folded() {
        let msgs = vec![
            user("start"),
            assistant_call("run_command"),
            one_lined("run_command"),
            assistant_call("read_file"),
            one_lined("read_file"),
        ];
        let seen = RefCell::new(Vec::new());
        // keep_last covers everything after the first message.
        let wide = DigestFoldConfig {
            keep_last: 4,
            min_rounds: 1,
        };
        let out = fold_aged_one_lined_rounds(msgs.clone(), &wide, &recording_mint(&seen));
        assert_eq!(out.rounds_folded, 0);
        assert_eq!(out.messages, msgs);
    }

    /// Twin: no retrievable ground truth, no card (#1766). A digest whose
    /// handle resolves to nothing is worse than the scaffolding it replaced.
    #[test]
    fn a_failed_mint_leaves_the_transcript_untouched() {
        let msgs = vec![
            user("start"),
            assistant_call("run_command"),
            one_lined("run_command"),
            assistant_call("read_file"),
            one_lined("read_file"),
            user("recent"),
            assistant_call("search"),
        ];
        let out = fold_aged_one_lined_rounds(msgs.clone(), &cfg(), &|_| None);
        assert_eq!(out.rounds_folded, 0, "no handle must mean no fold");
        assert_eq!(out.messages, msgs);
        assert!(out.handle.is_none());
    }

    /// Twin: an unanswered tool call is never folded — the pairing repair
    /// depends on the call still being there.
    #[test]
    fn an_unanswered_tool_call_is_never_folded() {
        let msgs = vec![
            user("start"),
            assistant_call("dangling"),
            assistant_call("run_command"),
            one_lined("run_command"),
            user("recent"),
            assistant_call("search"),
        ];
        let seen = RefCell::new(Vec::new());
        let out = fold_aged_one_lined_rounds(msgs.clone(), &cfg(), &recording_mint(&seen));
        assert_eq!(
            out.rounds_folded, 0,
            "one eligible round is below min_rounds"
        );
        assert_eq!(out.messages, msgs);
    }

    /// **The measured floor delta (#1966's shape).**
    ///
    /// #1966's session accreted 422 est-tokens per round because structural
    /// prune one-lines a tool result once and then pays its scaffolding
    /// forever: 175 rounds, floor 55,593 → 129,422 of a 158,557 budget.
    ///
    /// This builds that shape synthetically — aged one-lined rounds piling up
    /// behind a protected tail — and reports the floor with and without the
    /// fold. Printed as well as asserted, because the NUMBER is the point of
    /// the slice and a bare boolean would hide a regression to "folds one
    /// round".
    #[test]
    fn the_fold_lowers_the_floor_on_a_1966_shaped_session() {
        const ROUNDS: usize = 175;
        let mut msgs = vec![user("start the session")];
        for i in 0..ROUNDS {
            msgs.push(assistant_call("run_command"));
            msgs.push(one_lined(&format!("run_command_{i}")));
        }
        msgs.push(user("the current question"));

        let est = crate::tokens::TokenEstimation::default();
        let before = crate::agentic::trim::estimate_tokens(&msgs, est);

        let seen = RefCell::new(Vec::new());
        let out = fold_aged_one_lined_rounds(
            msgs,
            &DigestFoldConfig {
                keep_last: 12,
                min_rounds: 2,
            },
            &recording_mint(&seen),
        );
        let after = crate::agentic::trim::estimate_tokens(&out.messages, est);

        let reclaimed = before.saturating_sub(after);
        let pct = (reclaimed as f64 / before as f64) * 100.0;
        println!(
            "FLOOR DELTA over {ROUNDS} rounds: before={before} after={after} \
             reclaimed={reclaimed} ({pct:.1}%), rounds_folded={}",
            out.rounds_folded
        );

        assert!(
            out.rounds_folded >= ROUNDS - 12,
            "expected nearly every aged round to fold, got {}",
            out.rounds_folded
        );
        // The floor must fall MATERIALLY — this stage exists to buy headroom
        // before the 75% proactive line, and a few percent would not.
        assert!(
            after * 2 < before,
            "the fold reclaimed only {reclaimed} of {before} est-tokens \
             ({pct:.1}%); that is not enough to keep a session off the \
             proactive-summarize path"
        );
    }
}
