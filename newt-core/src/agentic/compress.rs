//! Compression v2 — summarize, don't discard (Step 18.4, issue #247).
//!
//! The shared pipeline behind both agentic loops' context-pressure triggers
//! (the mid-loop count/token trim and the pre-send `send_budget` guard).
//! Before this step those sites amputated the conversation middle into a
//! one-line placeholder; the context baseline measured the consequence
//! (`docs/testing/results/context-baseline-f0f4f6e.md` B6: 9/10 silently
//! wrong answers under truncation — the task itself was discarded).
//!
//! Pipeline order (design: `docs/design/context-memory-hermes-learnings.md`
//! §Phase 18):
//!
//! 1. **Structural prune** — [`crate::prune`]'s three passes (Step 18.3),
//!    zero LLM cost. Recheck the budget; most invocations end here.
//! 2. **Boundary computation** — head = system prompt + the original task
//!    (anchored verbatim, so the task can never be summarized away); tail
//!    protected by a TOKEN budget, not a message count (a count-based tail
//!    with a few huge tool results defeats the pipeline); the most recent
//!    user message is anchored into the tail (hermes #10896 — otherwise the
//!    current request "effectively disappears from the active context"); the
//!    cut is aligned past tool_call/result pairs so no orphan halves are
//!    *created* (prevention, not just post-repair).
//! 3. **LLM summary** of the middle via the injected summarizer, using the
//!    `Summarizing` provider's lean section template plus the
//!    verbatim-Active-Task rule, with [`redact_secrets`] applied to the
//!    summarizer input — summaries persist and re-inject for the life of a
//!    conversation, so credentials must never enter one.
//! 4. **Assembly** — the summary message carries the
//!    `[CONTEXT COMPACTION — REFERENCE ONLY]` prefix and the
//!    `--- END OF CONTEXT SUMMARY ---` end marker (weak local models read a
//!    verbatim task quote as fresh input without them — hermes #11475 /
//!    #14521), then [`super::trim::repair_orphaned_tool_calls`] runs as the
//!    post-hoc safety net, and a final aggressive prune (`keep_last: 0`)
//!    fits a still-over result structurally rather than letting the backend
//!    truncate it silently (the B6 failure shape: one giant tool round that
//!    no boundary can split).
//!
//! **No summarizer** (eval / headless / `None`) or a failed summarizer →
//! the static fallback marker ("Summary generation was unavailable. N
//! message(s) were removed.") — the old placeholder-discard survives as
//! exactly this path and only this path. A summarizer failure never aborts
//! the turn.
//!
//! **Anti-thrash** ([`CompressState`], hoisted from the `Summarizing`
//! provider to this shared path): when two consecutive compressions each
//! reclaim <10%, auto-compression is disabled for the session and the user
//! is told once; the budget guard stays hard — further over-budget rounds
//! are refused rather than silently truncated.
//!
//! Summary *continuity* (prev-summary chaining, restore rehydration) is
//! Step 18.5 — the seam is the summary message this module inserts.

use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use crate::prune::{prune, PruneConfig};

use super::trim::{estimate_tokens, estimate_value_tokens, repair_orphaned_tool_calls};

/// Future returned by an injected [`SummarizeFn`].
pub type SummarizeFuture = Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send>>;

/// The summarizer injected into the agentic loop (`ChatCtx::summarizer`):
/// given the assembled (already-redacted) summary request, returns the
/// summary text. Mirrors the `Summarizing` provider's `with_summarizer`
/// injection, but async — the loop calls it mid-flight.
pub type SummarizeFn = dyn Fn(String) -> SummarizeFuture + Send + Sync;

/// Owning form of [`SummarizeFn`] for callers that build one per session.
pub type Summarizer = Box<SummarizeFn>;

/// Prefix marker on every compaction message. Weak local models otherwise
/// treat the summary's verbatim task quote as a fresh instruction.
pub const SUMMARY_PREFIX: &str = "[CONTEXT COMPACTION — REFERENCE ONLY]";

/// End marker terminating every compaction message.
pub const SUMMARY_END_MARKER: &str = "--- END OF CONTEXT SUMMARY ---";

/// True when `m` is a compaction message this pipeline previously inserted
/// (LLM summary and the static fallback both carry [`SUMMARY_PREFIX`]).
/// Every user-role scan in the pipeline must consult this: anchoring the
/// boundary on the pipeline's own marker was the F1 self-poisoning bug —
/// from the second compression of a session on, the tail pinned to the
/// previous summary, the middle went empty, the message count could never
/// shrink, and the aggressive fit pass destroyed every fresh tool result
/// before the model saw it.
pub(crate) fn is_compaction_message(m: &Value) -> bool {
    m["content"]
        .as_str()
        .is_some_and(|c| c.starts_with(SUMMARY_PREFIX))
}

/// String form of [`is_compaction_message`] for callers holding plain text
/// instead of wire messages — the `Summarizing` provider's history entries
/// and restored turn records (Step 18.5, #247).
pub(crate) fn is_compaction_text(content: &str) -> bool {
    content.starts_with(SUMMARY_PREFIX)
}

/// Hard minimum number of tail messages kept verbatim (hermes's floor) —
/// even when the token-budgeted walk would protect fewer.
const TAIL_MIN_MESSAGES: usize = 3;

/// Per-message cap (chars) on content rendered into the summary request.
const SUMMARY_INPUT_MSG_CAP: usize = 2_000;

/// Reclaim fraction below which a compression counts as ineffective.
const THRASH_MIN_SAVINGS: f32 = 0.10;

// ---------------------------------------------------------------------------
// Anti-thrash state
// ---------------------------------------------------------------------------

/// Session-scoped compression accounting (anti-thrash). Owned by the caller
/// across turns (the TUI keeps one per session, like `NoteNudge`) and lent
/// to the loop per call; headless callers may pass `None` and get a fresh
/// per-turn state.
#[derive(Debug)]
pub struct CompressState {
    /// Reclaim fractions of the last two attempted compressions.
    last_savings: [f32; 2],
    attempts: usize,
    disabled: bool,
    notified: bool,
}

impl Default for CompressState {
    fn default() -> Self {
        Self::new()
    }
}

impl CompressState {
    pub fn new() -> Self {
        Self {
            last_savings: [1.0, 1.0],
            attempts: 0,
            disabled: false,
            notified: false,
        }
    }

    /// Record one attempted compression's before/after estimate. Two
    /// consecutive sub-10% reclaims disable auto-compression for the session.
    fn record(&mut self, tokens_before: usize, tokens_after: usize) {
        let saved = if tokens_before > 0 {
            1.0 - (tokens_after as f32 / tokens_before as f32)
        } else {
            0.0
        };
        self.last_savings = [self.last_savings[1], saved];
        self.attempts += 1;
        if self.attempts >= 2 && self.last_savings.iter().all(|&s| s < THRASH_MIN_SAVINGS) {
            self.disabled = true;
        }
    }

    /// True once anti-thrash has disabled auto-compression for this
    /// conversation (read by callers surfacing state and by the
    /// conversation-boundary reset tests).
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Re-arm after a conversation boundary. The anti-thrash notice promises
    /// "start a new conversation to reset" — the TUI makes that true by
    /// calling this from `/new` and `/conversation restore` (F4).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Latch the disabled state as if anti-thrash had fired — for tests that
    /// assert conversation-boundary resets without driving two poor passes.
    #[doc(hidden)]
    pub fn latch_disabled_for_tests(&mut self) {
        self.disabled = true;
        self.notified = true;
    }

    /// Read-only counters snapshot for display surfaces (`/memory`,
    /// Step 18.6, #247). Pure projection of existing state — no new
    /// accounting lives here.
    pub fn counters(&self) -> CompressCounters {
        // Strikes: how many of the most recent recorded compressions were
        // consecutively ineffective (<10% reclaim) — 0, 1, or 2; two is the
        // latch condition. The [1.0, 1.0] sentinel never counts because a
        // slot only holds a real figure once an attempt recorded into it.
        let strikes = if self.attempts == 0 || self.last_savings[1] >= THRASH_MIN_SAVINGS {
            0
        } else if self.attempts >= 2 && self.last_savings[0] < THRASH_MIN_SAVINGS {
            2
        } else {
            1
        };
        CompressCounters {
            compressions: self.attempts,
            strikes,
            disabled: self.disabled,
            last_reclaim: (self.attempts > 0).then_some(self.last_savings[1]),
        }
    }

    /// One-time user-facing notice, produced when anti-thrash disables
    /// compression. Subsequent calls return `None`.
    fn take_notice(&mut self) -> Option<String> {
        if self.disabled && !self.notified {
            self.notified = true;
            Some(
                "context compression reclaimed <10% twice in a row — auto-compression \
                 is disabled for this session; start a new conversation to reset"
                    .to_string(),
            )
        } else {
            None
        }
    }
}

/// Read-only snapshot of a session's compression accounting, surfaced by the
/// TUI's `/memory` (Step 18.6, #247). Plain data so display code and its
/// tests can build arbitrary states without driving the pipeline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompressCounters {
    /// Compressions recorded this session: the loop's hard-budget passes
    /// plus fired `/compress` runs (both feed [`CompressState`]).
    pub compressions: usize,
    /// Consecutive ineffective (<10% reclaim) recent compressions — 0, 1,
    /// or 2. Two latches `disabled`.
    pub strikes: usize,
    /// Anti-thrash latch: auto-compression is disabled for the session.
    /// `/new` (and `/conversation restore`) re-arm it — F4.
    pub disabled: bool,
    /// Reclaim fraction (0.0–1.0) of the most recent recorded compression;
    /// `None` before any compression recorded.
    pub last_reclaim: Option<f32>,
}

// ---------------------------------------------------------------------------
// Trigger
// ---------------------------------------------------------------------------

/// What [`compression_trigger`] decided for this round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompressTrigger {
    /// Message-space token budget (chars/4 estimate currency).
    pub budget: usize,
    /// Message-count ceiling, set only by the count trigger (structural
    /// pruning alone can never satisfy it — pruning never removes messages).
    pub max_messages: Option<usize>,
    /// True when a token trigger set `budget` — the hard correctness guard
    /// that consults and feeds anti-thrash. False for count-only (VRAM
    /// guard) firings, whose aim-to-halve budget does neither (F2).
    pub hard_budget: bool,
}

/// Decide whether compression fires this round, and with what message-space
/// token budget. One decision serves all three triggers:
///
/// - the mid-loop message-count threshold (the original VRAM guard),
/// - the mid-loop token threshold (issue #223),
/// - the pre-send `send_budget` guard (`max_ok_input` / `safe_context`).
///
/// `current_tokens` is the caller's truthful context figure
/// (prompt-tokens-preferred, Step 18.1) and includes tool-schema tokens; the
/// guard's budget therefore has `tool_tokens` subtracted to land back in
/// message-only space (the same arithmetic the old trim used). The tightest
/// fired budget wins. `message_tokens` is the caller's chars/4 estimate of
/// the message list alone — the currency the pipeline compares its budget
/// against — and prices the count-only trigger's aim-to-halve budget (F1).
pub(crate) fn compression_trigger(
    len: usize,
    current_tokens: usize,
    message_tokens: usize,
    count_threshold: usize,
    token_threshold: Option<usize>,
    send_budget: Option<usize>,
    tool_tokens: usize,
) -> Option<CompressTrigger> {
    // A zero token budget from config means DISABLED, not "compress to zero
    // every round" — the old `trim_to_token_budget` zero-is-noop contract,
    // re-homed here (F3).
    let token_threshold = token_threshold.filter(|&b| b > 0);
    let send_budget = send_budget.filter(|&b| b > 0);

    let count_fired = len > count_threshold;
    let token_fired = token_threshold.is_some_and(|b| current_tokens > b);
    let guard_fired = send_budget.is_some_and(|b| current_tokens > b);
    if !(count_fired || token_fired || guard_fired) {
        return None;
    }
    let mut budget = usize::MAX;
    if token_fired {
        budget = budget.min(token_threshold.unwrap_or(usize::MAX));
    }
    if guard_fired {
        budget = budget.min(
            send_budget
                .unwrap_or(usize::MAX)
                .saturating_sub(tool_tokens),
        );
    }
    let hard_budget = budget != usize::MAX;
    if !hard_budget {
        // Count-only trigger: no token target configured — aim to halve, in
        // MESSAGE-token space. Halving `current_tokens` (which includes
        // tool-schema and, when anchored, chat-template tokens no message
        // compression can ever reclaim) made the target cross-currency
        // unreachable, so the aggressive fit pass fired every round (F1).
        budget = message_tokens / 2;
    }
    Some(CompressTrigger {
        budget,
        max_messages: count_fired.then_some(count_threshold / 2),
        hard_budget,
    })
}

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// One compression request from a loop call site.
pub(crate) struct CompressRequest<'a> {
    pub messages: &'a [Value],
    /// Message-space token budget (chars/4 estimate currency) the result
    /// should fit.
    pub budget: usize,
    /// Message-count ceiling (set by the mid-loop count trigger). When
    /// `Some`, the structural prune alone can never satisfy the request.
    pub max_messages: Option<usize>,
    /// The original task — anchored verbatim into the summary request.
    pub task: &'a str,
    /// True when `budget` came from a token trigger (mid-loop token
    /// threshold, send-budget guard, cw-400 recovery, overflow retry) — the
    /// hard correctness guard that consults and feeds anti-thrash. Count-only
    /// (VRAM guard) requests pass false and do neither (F2): their soft
    /// aim-to-halve budget must never latch the disable switch or convert a
    /// healthy session into a refused send.
    pub hard_budget: bool,
    /// Optional user-supplied focus topic (`/compress <focus>`, Step 18.6):
    /// threaded into the summary request as emphasis guidance. Redacted with
    /// the same [`redact_secrets`] pass as the rendered middle — a user can
    /// type a credential into the focus. The loop's automatic triggers pass
    /// `None`.
    pub focus: Option<&'a str>,
}

impl<'a> CompressRequest<'a> {
    /// A user-initiated request (the TUI's `/compress`, Step 18.6). The user
    /// asked for compression NOW, with or without token pressure, so the
    /// budget is aim-to-halve in message-token space — the count trigger's
    /// exact pricing (F1) — and `hard_budget` is false: like a count-only
    /// firing, a manual run neither consults the anti-thrash latch (an
    /// explicit ask still runs after auto-compression is disabled) nor lets
    /// the pipeline's internal accounting treat it as the correctness guard.
    /// Effectiveness accounting for fired manual runs is the caller's call —
    /// [`compress_user_initiated`] records them.
    pub(crate) fn user_initiated(
        messages: &'a [Value],
        task: &'a str,
        focus: Option<&'a str>,
    ) -> Self {
        Self {
            messages,
            budget: estimate_tokens(messages) / 2,
            max_messages: None,
            task,
            hard_budget: false,
            focus,
        }
    }
}

/// What the pipeline did, in escalation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompressAction {
    /// Already within budget by the pipeline's own estimate — untouched.
    Fit,
    /// Structural pruning alone sufficed (or was all that applied).
    Pruned,
    /// Middle replaced with an LLM summary.
    Summarized,
    /// Middle replaced with the static fallback marker (no summarizer, or
    /// the summarizer failed).
    StaticFallback,
    /// Anti-thrash disabled compression while the list exceeds the budget:
    /// the caller must refuse the send rather than silently truncate.
    Refused,
}

impl CompressAction {
    /// Short human description for the compression notice.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Fit => "no change",
            Self::Pruned => "structural prune",
            Self::Summarized => "prune + summary",
            Self::StaticFallback => "prune + static marker",
            Self::Refused => "refused",
        }
    }
}

/// Result of one [`compress`] run.
pub(crate) struct CompressOutcome {
    pub messages: Vec<Value>,
    pub action: CompressAction,
    /// True when `messages` differs from the input.
    pub fired: bool,
    pub tokens_before: usize,
    pub tokens_after: usize,
    /// One-time anti-thrash notice for the display path, when it just fired.
    pub notice: Option<String>,
}

/// Run the compression pipeline: prune → boundary → redacted summary →
/// marker assembly → repair (+ final structural fit pass). Infallible by
/// design — a summarizer failure degrades to the static marker; only
/// [`CompressAction::Refused`] asks the caller to stop.
pub(crate) async fn compress(
    req: CompressRequest<'_>,
    summarizer: Option<&SummarizeFn>,
    state: &mut CompressState,
) -> CompressOutcome {
    let tokens_before = estimate_tokens(req.messages);
    // Anti-thrash protects the hard token budget (the correctness guard);
    // count-only invocations (the VRAM guard) neither consult nor feed it —
    // `hard_budget` carries the trigger kind so this holds even when the
    // count trigger's soft aim-to-halve budget happens to be exceeded (F2).
    let tokens_over_entry = req.hard_budget && tokens_before > req.budget;
    let over = |tokens: usize, len: usize| {
        tokens > req.budget || req.max_messages.is_some_and(|m| len > m)
    };

    if !over(tokens_before, req.messages.len()) {
        return CompressOutcome {
            messages: req.messages.to_vec(),
            action: CompressAction::Fit,
            fired: false,
            tokens_before,
            tokens_after: tokens_before,
            notice: None,
        };
    }
    if state.disabled && req.hard_budget {
        if tokens_over_entry {
            // The hard guard stays: better to refuse the send than let the
            // backend silently truncate the head (B6's 9/10 failure mode).
            return CompressOutcome {
                messages: req.messages.to_vec(),
                action: CompressAction::Refused,
                fired: false,
                tokens_before,
                tokens_after: tokens_before,
                notice: state.take_notice(),
            };
        }
        // A hard trigger fired but the message estimate fits its budget
        // (e.g. mixed with the count trigger): compression is disabled —
        // pass through unchanged.
        return CompressOutcome {
            messages: req.messages.to_vec(),
            action: CompressAction::Fit,
            fired: false,
            tokens_before,
            tokens_after: tokens_before,
            notice: state.take_notice(),
        };
    }

    // (1) Structural prune — zero LLM cost (Step 18.3's passes).
    let pruned = prune(req.messages, &PruneConfig::default());
    let prune_changed = pruned.chars_reclaimed > 0;
    let pruned = pruned.messages;
    let after_prune = estimate_tokens(&pruned);
    if !over(after_prune, pruned.len()) {
        if tokens_over_entry {
            state.record(tokens_before, after_prune);
        }
        return CompressOutcome {
            messages: pruned,
            action: CompressAction::Pruned,
            fired: prune_changed,
            tokens_before,
            tokens_after: after_prune,
            notice: state.take_notice(),
        };
    }

    // (2) Boundary: head + token-budgeted (and, for the count trigger,
    // count-capped) tail, last-user anchored, tool-pair aligned.
    let boundary = compute_boundary(&pruned, req.budget, req.max_messages);
    let middle = &pruned[boundary.head..boundary.tail_start];

    let (mut assembled, mut action) = if middle.is_empty() {
        // Nothing summarizable between the protected head and tail.
        (pruned.clone(), CompressAction::Pruned)
    } else {
        // (3) LLM summary of the middle, redaction applied to the input.
        let body = match summarizer {
            Some(f) => {
                // Cap the total rendered middle so the summary request itself
                // cannot blow the summarizer's context window — per-message
                // caps alone do not bound the total (F5). The cap is the
                // compression budget in chars (4 chars/token): the budget is
                // what the *conversation* must fit after compression, so a
                // request of the same order fits any window the compressed
                // conversation will. Floored at 8 KiB so tight budgets still
                // give the summarizer enough material to work with.
                let middle_cap = req.budget.saturating_mul(4).max(8_192);
                let request =
                    redact_secrets(&summary_request(req.task, middle, middle_cap, req.focus));
                match f(request).await {
                    Ok(s) if !s.trim().is_empty() => Some(s),
                    Ok(_) => None,
                    Err(e) => {
                        tracing::warn!(error = %e, "compression summarizer failed — static marker fallback");
                        None
                    }
                }
            }
            None => None,
        };
        let action = if body.is_some() {
            CompressAction::Summarized
        } else {
            CompressAction::StaticFallback
        };
        let body = body.unwrap_or_else(|| static_fallback_text(middle.len()));
        // (4) Assembly with the REFERENCE-ONLY prefix + end marker.
        let mut out = Vec::with_capacity(boundary.head + 1 + (pruned.len() - boundary.tail_start));
        out.extend_from_slice(&pruned[..boundary.head]);
        out.push(summary_message(&body));
        out.extend_from_slice(&pruned[boundary.tail_start..]);
        (out, action)
    };

    // Post-hoc safety net: never ship an orphaned tool_call/result half.
    repair_orphaned_tool_calls(&mut assembled);

    // Final structural fit pass: when the protected tail itself blows the
    // budget (B6's shape — one giant tool round), one-line the AGED part
    // rather than letting the backend silently truncate the head (and the
    // task) away. The trailing tool group — results the model has not seen
    // yet — is NEVER pruned here (F1c): the old `keep_last: 0` destroyed
    // every fresh result from the second compression of a session on,
    // leaving the model unable to read anything. An over-budget dispatch is
    // recoverable (cw-400 recovery / overflow retry); a destroyed fresh
    // result is not. The group is derived from the last assistant message
    // carrying `tool_calls` — NOT by counting trailing `role == "tool"`
    // messages, which any interleaved user message (the read-only nudge, a
    // compaction notice) zeroed, flooring `keep_last` at 2 and one-lining
    // older unseen results for a round (#270).
    if estimate_tokens(&assembled) > req.budget {
        let aggressive = prune(
            &assembled,
            &PruneConfig {
                keep_last: trailing_tool_group_len(&assembled).max(2),
                ..PruneConfig::default()
            },
        );
        if aggressive.chars_reclaimed > 0 {
            assembled = aggressive.messages;
            if action == CompressAction::Fit {
                action = CompressAction::Pruned;
            }
        }
        // #285: under a HARD budget (the window-correctness guard), the
        // F1c protection and the window are in direct tension when the
        // trailing group BY ITSELF exceeds what is left of the budget after
        // the (already maximally pruned) head + summary. Reclaim WITHIN the
        // group — newest result kept whole, older members one-lined oldest
        // first — instead of always shipping over-window into a silent
        // backend truncation. Soft (count-only / `/compress`) budgets never
        // reach this: missing an aim-to-halve target is not a correctness
        // problem, so the F1c protection stays absolute there.
        if req.hard_budget
            && estimate_tokens(&assembled) > req.budget
            && reclaim_within_trailing_group(&mut assembled, req.budget)
            && action == CompressAction::Fit
        {
            action = CompressAction::Pruned;
        }
    }

    let tokens_after = estimate_tokens(&assembled);
    let fired =
        prune_changed || assembled.len() != req.messages.len() || tokens_after != tokens_before;
    if tokens_over_entry {
        state.record(tokens_before, tokens_after);
    }
    CompressOutcome {
        messages: assembled,
        action,
        fired,
        tokens_before,
        tokens_after,
        notice: state.take_notice(),
    }
}

// ---------------------------------------------------------------------------
// User-initiated compression (`/compress [focus]`, Step 18.6)
// ---------------------------------------------------------------------------

/// What a user-initiated [`compress_user_initiated`] run did — the public
/// face of [`CompressOutcome`], with the message counts the honesty notice
/// needs ("never claim savings that didn't happen").
#[derive(Debug, Clone)]
pub struct ManualCompressOutcome {
    /// The assembled working set (equals the input when `fired` is false).
    pub messages: Vec<Value>,
    /// True when the pipeline actually changed the working set. False ⇒
    /// "no compression possible" — the caller must not claim savings.
    pub fired: bool,
    pub messages_before: usize,
    pub messages_after: usize,
    /// chars/4 estimates over the message list (the pipeline's currency).
    pub tokens_before: usize,
    pub tokens_after: usize,
    /// What the pipeline did, e.g. `"prune + summary"` (the LLM summarizer
    /// ran) vs `"prune + static marker"` (no summarizer / it failed) vs
    /// `"structural prune"` — [`CompressAction::describe`]'s wording, so the
    /// manual notice and the loop's notice can never drift apart.
    pub how: &'static str,
    /// One-time anti-thrash notice, when this run just latched the disable.
    pub notice: Option<String>,
}

/// Run the shared compression pipeline because the user asked (`/compress
/// [focus]`, Step 18.6, #247) — the SAME prune → boundary → redacted summary
/// → marker assembly the loop's triggers call, via
/// [`CompressRequest::user_initiated`] (aim-to-halve, soft budget). No
/// bespoke compression path.
///
/// Anti-thrash interplay: the soft request never *consults* the latch (an
/// explicit ask still runs after auto-compression is disabled), but a fired
/// run *records* its reclaim into `state` — hermes parity: manual passes
/// feed effectiveness accounting, so `/memory`'s counters stay truthful and
/// a genuinely useless summarizer still latches. A no-op run records
/// nothing: an incompressible-because-tiny session must never strike out
/// auto-compression for later.
///
/// The original-task anchor is derived from the working set itself (first
/// real user message — a leading compaction message is not the task), the
/// same rule the `Summarizing` provider applies.
pub async fn compress_user_initiated(
    messages: &[Value],
    focus: Option<&str>,
    summarizer: Option<&SummarizeFn>,
    state: &mut CompressState,
) -> ManualCompressOutcome {
    let task = messages
        .iter()
        .find(|m| m["role"].as_str() == Some("user") && !is_compaction_message(m))
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string();
    let outcome = compress(
        CompressRequest::user_initiated(messages, &task, focus),
        summarizer,
        state,
    )
    .await;
    if outcome.fired {
        state.record(outcome.tokens_before, outcome.tokens_after);
    }
    let notice = outcome.notice.or_else(|| state.take_notice());
    ManualCompressOutcome {
        messages_before: messages.len(),
        messages_after: outcome.messages.len(),
        fired: outcome.fired,
        tokens_before: outcome.tokens_before,
        tokens_after: outcome.tokens_after,
        how: outcome.action.describe(),
        notice,
        messages: outcome.messages,
    }
}

// ---------------------------------------------------------------------------
// Boundary computation
// ---------------------------------------------------------------------------

struct Boundary {
    /// Protected head: `[0, head)` — leading system message(s) plus the
    /// original task.
    head: usize,
    /// Protected tail: `[tail_start, len)`. The middle `[head, tail_start)`
    /// is what gets summarized.
    tail_start: usize,
}

/// Compute the protected head and the token-budgeted, anchored, pair-aligned
/// protected tail. `max_messages` (the count trigger's ceiling) additionally
/// caps the tail by count so the assembled `head + summary + tail` actually
/// lands at or under the ceiling — a token-budgeted tail alone can swallow
/// an entire small-message conversation and leave nothing to summarize.
fn compute_boundary(messages: &[Value], budget: usize, max_messages: Option<usize>) -> Boundary {
    let head = head_len(messages);
    let max_tail = max_messages.map(|m| m.saturating_sub(head + 1).max(1));

    // Token-budgeted tail: walk backward accumulating estimates until ~25%
    // of the budget is protected, with a hard minimum of TAIL_MIN_MESSAGES.
    let tail_budget = (budget / 4).max(1);
    let mut tail_start = messages.len();
    let mut acc = 0usize;
    let mut kept = 0usize;
    while tail_start > head {
        if max_tail.is_some_and(|m| kept >= m) {
            break;
        }
        let t = estimate_value_tokens(&messages[tail_start - 1]);
        if kept >= TAIL_MIN_MESSAGES && acc + t > tail_budget {
            break;
        }
        acc += t;
        kept += 1;
        tail_start -= 1;
    }

    // Last-user anchor: the most recent REAL user message is never
    // summarized away (hermes #10896 — losing it loses the active request).
    // The pipeline's own compaction messages are user-role but must never
    // anchor: pinning the tail to the previous summary froze the boundary
    // for the rest of the session (F1).
    if let Some(last_user) = messages
        .iter()
        .rposition(|m| m["role"].as_str() == Some("user") && !is_compaction_message(m))
    {
        if last_user >= head {
            tail_start = tail_start.min(last_user);
        }
    }

    // Tool-pair boundary prevention: never start the tail inside a result
    // group — pull the cut back to the assistant carrying the tool_calls so
    // call/result pairs stay together (hermes `_align_boundary_backward`).
    while tail_start > head && messages[tail_start]["role"].as_str() == Some("tool") {
        tail_start -= 1;
    }

    // Count-goal recheck (F1d): the anchor (or pair alignment) may have
    // extended the tail past the count trigger's ceiling, making
    // `max_messages` unreachable — the trigger then re-fires every round
    // and the summarizer runs per round for nothing. Re-apply the cap by
    // advancing the cut; the current request still survives verbatim via
    // the summary's Active-Task rule even when the anchored message lands
    // in the middle. Then re-align so the cut never starts inside a result
    // group (this can give back a few messages of slack — bounded by the
    // group size, not unbounded growth).
    if let Some(max_tail) = max_tail {
        let cap_start = messages.len().saturating_sub(max_tail);
        if tail_start < cap_start {
            tail_start = cap_start;
            while tail_start > head && messages[tail_start]["role"].as_str() == Some("tool") {
                tail_start -= 1;
            }
        }
    }

    Boundary { head, tail_start }
}

/// Length of the protected head: every leading `system` message plus the
/// first `user` message after them (the original task). A compaction
/// message in that slot (a rehydrated history can start with one) is NOT
/// the task and must stay summarizable.
fn head_len(messages: &[Value]) -> usize {
    let mut head = 0;
    while head < messages.len() && messages[head]["role"].as_str() == Some("system") {
        head += 1;
    }
    if head < messages.len()
        && messages[head]["role"].as_str() == Some("user")
        && !is_compaction_message(&messages[head])
    {
        head += 1;
    }
    head
}

// ---------------------------------------------------------------------------
// Trailing-group protection (#270 / #285)
// ---------------------------------------------------------------------------

/// Length of the suffix the aggressive fit pass protects: from the LAST
/// message carrying `tool_calls` (the assistant turn that issued the calls)
/// through the end of the list — that turn, its fresh (unseen) results, and
/// anything interleaved after them. `0` when nothing in the list ever
/// called a tool.
///
/// Deriving the group by counting trailing `role == "tool"` messages was the
/// #270 gap: the read-only-round nudge injects a `user` message immediately
/// before the compression call site, the trailing count read zero,
/// `keep_last` fell to its floor of 2, and every older unseen result in the
/// fresh group was one-lined pre-dispatch for a round. Anchoring on the
/// turn that ISSUED the calls makes the group immune to whatever lands
/// after it (a nudge, a compaction notice). Only `tool_calls` is consulted
/// — the loop appends the backend's `message` object verbatim, and a `role`
/// field is not guaranteed on every wire dialect.
fn trailing_tool_group_len(messages: &[Value]) -> usize {
    messages
        .iter()
        .rposition(|m| m["tool_calls"].as_array().is_some_and(|t| !t.is_empty()))
        .map_or(0, |i| messages.len() - i)
}

/// #285 escape hatch for the F1c trailing-group protection: when the fresh
/// trailing group BY ITSELF exceeds the budget remaining after everything
/// before it (head + summary + already-one-lined aged remnants), no amount
/// of out-of-group reclaim can fit the window — compression honestly reports
/// "still over budget" and the backend then truncates the dispatch silently
/// (B6's wrong-answer shape, measured in #284's gauntlet). Reclaim WITHIN
/// the group instead: keep the NEWEST result whole, one-line older members
/// oldest-first via the prune pass-2 machinery (the one-liner names the tool
/// and file, so the model can re-read), stopping as soon as the list fits.
///
/// If even the newest result alone exceeds the budget the list stays over —
/// the dispatch proceeds truthfully over budget (the loop's N2 notice
/// reports real numbers); clipping inside a single result is out of scope.
/// Returns true when any member was rewritten.
fn reclaim_within_trailing_group(assembled: &mut Vec<Value>, budget: usize) -> bool {
    let group_len = trailing_tool_group_len(assembled);
    if group_len == 0 {
        return false;
    }
    let group_start = assembled.len() - group_len;
    let outside = estimate_tokens(&assembled[..group_start]);
    let group_tokens = estimate_tokens(&assembled[group_start..]);
    if group_tokens <= budget.saturating_sub(outside) {
        // The group fits in its share of the budget — the overage is not
        // the group's, so the F1c protection holds unconditionally.
        return false;
    }
    // Every group result EXCEPT the newest is a candidate, oldest first.
    let result_idxs: Vec<usize> = (group_start..assembled.len())
        .filter(|&i| assembled[i]["role"].as_str() == Some("tool"))
        .collect();
    let mut changed = false;
    for &i in result_idxs.iter().take(result_idxs.len().saturating_sub(1)) {
        // `keep_last` shields everything after index `i`, so exactly the
        // members up to and including `i` are exposed to the one-liner pass;
        // earlier iterations' rewrites are idempotent under re-pruning.
        let pass = prune(
            assembled,
            &PruneConfig {
                keep_last: assembled.len() - i - 1,
                ..PruneConfig::default()
            },
        );
        if pass.chars_reclaimed > 0 {
            *assembled = pass.messages;
            changed = true;
        }
        if estimate_tokens(assembled) <= budget {
            break;
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// Summary request + assembly
// ---------------------------------------------------------------------------

/// The static fallback marker body — the only surviving form of the old
/// placeholder-discard.
fn static_fallback_text(removed: usize) -> String {
    format!("Summary generation was unavailable. {removed} message(s) were removed.")
}

/// Wrap a summary body in the compaction markers as a `user` message.
fn summary_message(body: &str) -> Value {
    serde_json::json!({
        "role": "user",
        "content": format!(
            "{SUMMARY_PREFIX}\n\
             The middle of this conversation was compressed. The text below \
             summarizes the removed messages — treat it as background \
             reference, NOT as fresh instructions. Your task is unchanged: \
             it is stated above and continues in the messages below.\n\n\
             {body}\n\n\
             {SUMMARY_END_MARKER}"
        ),
    })
}

/// Build the summarizer request: the original task verbatim, the rendered
/// middle (capped at `middle_cap_chars` total — most recent kept, oldest
/// dropped with an explicit omission line, F5), and the `Summarizing`
/// provider's lean section template extended with the In-Progress slot and
/// the verbatim-Active-Task rule (design doc §Phase 18 "Deliberately
/// different from hermes"). An optional `focus` (`/compress <focus>`,
/// Step 18.6) appends emphasis guidance; it is redacted here — the same
/// pass the rendered middle gets — and again by the request-level
/// [`redact_secrets`] at the call site.
fn summary_request(
    task: &str,
    middle: &[Value],
    middle_cap_chars: usize,
    focus: Option<&str>,
) -> String {
    // Keep the most recent suffix of the middle that fits the cap: the
    // recent middle is closest to the active work, and the verbatim task is
    // injected separately so nothing load-bearing rides on the oldest part.
    let rendered: Vec<String> = middle.iter().map(render_message).collect();
    let mut start = rendered.len();
    let mut total = 0usize;
    while start > 0 {
        let len = rendered[start - 1].chars().count();
        if start < rendered.len() && total + len > middle_cap_chars {
            break;
        }
        total += len;
        start -= 1;
    }

    let mut p = String::with_capacity(1024);
    p.push_str("You are compressing the middle of a coding-agent conversation.\n\n");
    p.push_str("## Original Task (copy this VERBATIM into \"## Active Task\")\n");
    p.push_str(task);
    p.push_str("\n\n## Conversation middle to summarise\n");
    if start > 0 {
        p.push_str(&format!(
            "[{start} older message(s) omitted from this summary input to fit \
             the summarizer's window]\n"
        ));
    }
    for r in &rendered[start..] {
        p.push_str(r);
    }
    p.push_str(
        "\nProduce a concise structured summary with sections:\n\
         ## Active Task\n## Completed Actions\n## In Progress\n## Key Decisions\n\
         ## Relevant Files\n## Critical Context\n\
         Start \"## Active Task\" with the original task copied verbatim. \
         Be terse. Preserve specifics (file names, error messages, decisions). \
         NEVER include API keys, tokens, passwords, or other credentials — \
         write [REDACTED] instead.",
    );
    if let Some(focus) = focus {
        let focus = redact_secrets(focus);
        let focus = focus.trim();
        if !focus.is_empty() {
            p.push_str(&format!(
                "\nThe user asked for this compression and wants emphasis on \
                 a topic: emphasize anything about {focus} — give it the bulk \
                 of the summary's detail while keeping every section above."
            ));
        }
    }
    p
}

/// Render one wire-shape message as a line of summarizer input.
///
/// Redaction runs BEFORE excerpting (N4): truncating at the excerpt cap can
/// otherwise slice a credential into a fragment too short for any redaction
/// pattern to match — the request-level `redact_secrets` pass would then
/// let it through. (That request-level pass still runs as a second layer.)
fn render_message(m: &Value) -> String {
    let role = m["role"].as_str().unwrap_or("unknown");
    let mut line = format!("[{role}]");
    if let Some(tcs) = m["tool_calls"].as_array() {
        for tc in tcs {
            let name = tc["function"]["name"].as_str().unwrap_or("tool");
            let args = tc["function"]["arguments"].to_string();
            line.push_str(" called ");
            line.push_str(name);
            line.push('(');
            line.push_str(&excerpt(&redact_secrets(&args), 200));
            line.push(')');
        }
    }
    if let Some(content) = m["content"].as_str() {
        if !content.is_empty() {
            line.push(' ');
            line.push_str(&excerpt(&redact_secrets(content), SUMMARY_INPUT_MSG_CAP));
        }
    }
    line.push('\n');
    line
}

/// First `max_chars` chars, newlines preserved, `…`-terminated if cut.
fn excerpt(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

// ---------------------------------------------------------------------------
// Secret redaction
// ---------------------------------------------------------------------------

/// `(pattern, replacement)` table for [`redact_secrets`]. Deliberately small
/// and high-precision: each row matches a *credential value shape*, not
/// prose about credentials — "the api key is in the keychain" must pass.
const REDACTION_TABLE: &[(&str, &str)] = &[
    // Private key blocks (redact even when the END line was truncated away).
    (
        r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
        "[REDACTED]",
    ),
    // OpenAI-style secret keys.
    (r"\bsk-[A-Za-z0-9_-]{20,}", "[REDACTED]"),
    // GitHub tokens (classic + fine-grained).
    (r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}", "[REDACTED]"),
    (r"\bgithub_pat_[A-Za-z0-9_]{20,}", "[REDACTED]"),
    // AWS access key ids.
    (r"\bAKIA[0-9A-Z]{16}\b", "[REDACTED]"),
    // JWTs (`eyJ` = base64 of `{"`).
    (
        r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{6,}\.[A-Za-z0-9_-]{6,}",
        "[REDACTED]",
    ),
    // HTTP bearer credentials. The value class has no spaces, so prose like
    // "bearer of good news" never reaches the 20-char floor.
    (
        r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{20,}",
        "Bearer [REDACTED]",
    ),
    // Generic credential assignment: a secret-ish key, `=`/`:`, and a
    // value of 8+ non-space chars. The key list is closed (no bare
    // "token"/"key") so token-budget talk passes. The optional quote after
    // the key matches the JSON-quoted shape (`"api_key": "…"`) — the native
    // form tool-call args take in this pipeline's summarizer input (F6).
    (
        r#"(?i)\b(api[_-]?key|secret[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret|password|passwd)\b["']?\s*[:=]\s*["']?[^\s"']{8,}["']?"#,
        "${1}=[REDACTED]",
    ),
];

fn redaction_patterns() -> &'static Vec<(regex::Regex, &'static str)> {
    static PATTERNS: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        REDACTION_TABLE
            .iter()
            .map(|(pat, rep)| {
                (
                    regex::Regex::new(pat).expect("redaction pattern must compile"),
                    *rep,
                )
            })
            .collect()
    })
}

/// Replace credential-looking strings with `[REDACTED]`. Applied to ALL
/// summarizer input — the summarizer LLM may ignore prompt instructions and
/// echo secrets back verbatim, and summaries persist for the conversation.
pub(crate) fn redact_secrets(input: &str) -> String {
    let mut out = input.to_string();
    for (re, rep) in redaction_patterns() {
        if re.is_match(&out) {
            out = re.replace_all(&out, *rep).into_owned();
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    // -- builders ------------------------------------------------------------

    fn sys(text: &str) -> Value {
        json!({"role": "system", "content": text})
    }

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": text})
    }

    fn assistant_call(name: &str, args: Value) -> Value {
        json!({"role": "assistant", "content": "",
               "tool_calls": [{"function": {"name": name, "arguments": args}}]})
    }

    fn tool_result(content: &str) -> Value {
        json!({"role": "tool", "content": content})
    }

    /// `[system, task, (assistant_call read_file → big result) × rounds]`.
    fn tool_heavy(task: &str, rounds: usize, result_chars: usize) -> Vec<Value> {
        let mut msgs = vec![sys("you are newt"), user(task)];
        for i in 0..rounds {
            msgs.push(assistant_call(
                "read_file",
                json!({"path": format!("src/file_{i}.rs")}),
            ));
            msgs.push(tool_result(&format!("{i}:{}", "x".repeat(result_chars))));
        }
        msgs
    }

    /// A summarizer that records every prompt it receives and returns a
    /// canned summary.
    fn recording_summarizer(prompts: Arc<Mutex<Vec<String>>>, reply: &'static str) -> Summarizer {
        Box::new(move |prompt: String| {
            let prompts = prompts.clone();
            Box::pin(async move {
                prompts.lock().unwrap().push(prompt);
                Ok(reply.to_string())
            })
        })
    }

    fn failing_summarizer(calls: Arc<AtomicUsize>) -> Summarizer {
        Box::new(move |_prompt: String| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("summarizer endpoint 500")
            })
        })
    }

    /// Hard-budget invocation (token threshold / send-budget semantics).
    async fn run(
        messages: &[Value],
        budget: usize,
        max_messages: Option<usize>,
        summarizer: Option<&SummarizeFn>,
        state: &mut CompressState,
    ) -> CompressOutcome {
        compress(
            CompressRequest {
                messages,
                budget,
                max_messages,
                task: "fix the failing test",
                hard_budget: true,
                focus: None,
            },
            summarizer,
            state,
        )
        .await
    }

    /// Count-only (VRAM guard) invocation: soft aim-to-halve budget that
    /// neither consults nor feeds anti-thrash (F2).
    async fn run_count_only(
        messages: &[Value],
        budget: usize,
        max_messages: Option<usize>,
        summarizer: Option<&SummarizeFn>,
        state: &mut CompressState,
    ) -> CompressOutcome {
        compress(
            CompressRequest {
                messages,
                budget,
                max_messages,
                task: "fix the failing test",
                hard_budget: false,
                focus: None,
            },
            summarizer,
            state,
        )
        .await
    }

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
        let before = estimate_tokens(&msgs);
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
        let before = estimate_tokens(&msgs);
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
        // The summary message carries both markers and the summary body.
        let summary = out.messages[2]["content"].as_str().unwrap();
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

    /// The summary request contains the original task verbatim, the lean
    /// template sections, and the verbatim-Active-Task rule.
    #[tokio::test]
    async fn summary_request_carries_task_verbatim_and_template() {
        let task = "ACTIVE TASK GAUNTLET-7f3d9c: read ten files then report";
        let mut msgs = tool_heavy(task, 6, 4_000);
        msgs[1] = user(task);
        let before = estimate_tokens(&msgs);
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let s = recording_summarizer(prompts.clone(), "SUMMARY");
        let mut state = CompressState::new();
        let out = compress(
            CompressRequest {
                messages: &msgs,
                budget: before / 3,
                max_messages: None,
                task,
                hard_budget: true,
                focus: None,
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

    /// No summarizer → static fallback marker with the exact removed count.
    #[tokio::test]
    async fn no_summarizer_uses_static_fallback_marker() {
        let msgs = tool_heavy("task", 6, 4_000);
        let before = estimate_tokens(&msgs);
        let mut state = CompressState::new();
        let out = run(&msgs, before / 3, None, None, &mut state).await;
        assert_eq!(out.action, CompressAction::StaticFallback);
        let summary = out.messages[2]["content"].as_str().unwrap();
        assert!(summary.starts_with(SUMMARY_PREFIX), "{summary}");
        assert!(summary.contains(SUMMARY_END_MARKER), "{summary}");
        // middle = messages [2, tail_start): compute the expected count from
        // the output shape (head 2 + marker 1 + tail).
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
        let before = estimate_tokens(&msgs);
        let calls = Arc::new(AtomicUsize::new(0));
        let s = failing_summarizer(calls.clone());
        let mut state = CompressState::new();
        let out = run(&msgs, before / 3, None, Some(&*s), &mut state).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "summarizer was attempted");
        assert_eq!(out.action, CompressAction::StaticFallback);
        let summary = out.messages[2]["content"].as_str().unwrap();
        assert!(summary.contains("Summary generation was unavailable."));
    }

    /// An empty/whitespace summary counts as a failure (static marker).
    #[tokio::test]
    async fn empty_summary_falls_back_to_static_marker() {
        let msgs = tool_heavy("task", 6, 4_000);
        let before = estimate_tokens(&msgs);
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let s = recording_summarizer(prompts.clone(), "  \n ");
        let mut state = CompressState::new();
        let out = run(&msgs, before / 3, None, Some(&*s), &mut state).await;
        assert_eq!(out.action, CompressAction::StaticFallback);
    }

    /// The B6 shape with an AGED giant round: one giant tool round that no
    /// boundary can split, followed by a newer small round — the final fit
    /// pass one-lines the giant (aged) results under budget instead of
    /// letting the backend silently truncate the head.
    #[tokio::test]
    async fn giant_aged_round_is_pruned_aggressively_not_shipped_over_budget() {
        let task = "ACTIVE TASK GAUNTLET-7f3d9c: summarize the three files";
        let mut msgs = vec![sys("you are newt"), user(task)];
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
        assert_eq!(out.messages[2]["tool_calls"].as_array().unwrap().len(), 3);
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
        msgs.push(user("[3 consecutive read-only rounds with no file writes.]"));
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
        assert!(out
            .messages
            .iter()
            .any(|m| m["content"]
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
        assert!(!reclaim_within_trailing_group(&mut fits, 10_000));
        assert_eq!(fits, before, "a group within its share is never touched");

        // No group at all: no-op.
        let mut no_group = vec![sys("s"), user(&big)];
        assert!(!reclaim_within_trailing_group(&mut no_group, 100));

        // Single-member group over budget: the newest IS the only member —
        // untouched, truthful over-budget residual (clipping inside one
        // result is out of scope).
        let mut single = group(&[&big]);
        let before = single.clone();
        assert!(!reclaim_within_trailing_group(&mut single, 1_000));
        assert_eq!(single, before);

        // Oversized group, early stop: one-lining the OLDEST member alone
        // fits the budget — the middle and newest members stay whole.
        let mut early = group(&[&big, &small, &small]);
        assert!(reclaim_within_trailing_group(&mut early, 1_500));
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
        assert!(estimate_tokens(&early) <= 1_500, "the list now fits");

        // Newest alone exceeds the budget: all older members one-lined, the
        // newest still whole, the list honestly stays over.
        let mut residual = group(&[&small, &small, &big]);
        assert!(reclaim_within_trailing_group(&mut residual, 1_000));
        let results: Vec<&str> = residual
            .iter()
            .filter(|m| m["role"].as_str() == Some("tool"))
            .map(|m| m["content"].as_str().unwrap())
            .collect();
        assert!(results[0].starts_with("[read_file] read 'f0.txt'"));
        assert!(results[1].starts_with("[read_file] read 'f1.txt'"));
        assert_eq!(results[2], big, "the newest member is never a candidate");
        assert!(
            estimate_tokens(&residual) > 1_000,
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

    /// The count trigger (`max_messages`) forces the summary stage even when
    /// tokens already fit — pruning can never reduce the message count.
    #[tokio::test]
    async fn max_messages_forces_summary_stage() {
        let msgs = tool_heavy("task", 8, 50); // small payloads: tokens fit
        let before = estimate_tokens(&msgs);
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
        let budget = estimate_tokens(&msgs) / 2;
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
        let budget2 = estimate_tokens(&grown) / 2;
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
            let budget = estimate_tokens(&msgs) / 2;
            let out = run_count_only(&msgs, budget, Some(6), None, &mut state).await;
            assert_ne!(out.action, CompressAction::Refused);
        }
        assert!(!state.disabled, "count-only passes must never latch");
        assert_eq!(state.attempts, 0, "count-only passes must never record");

        // A latched state must not block the VRAM guard.
        let mut latched = CompressState::new();
        latched.disabled = true;
        latched.notified = true;
        let budget = estimate_tokens(&msgs) / 2;
        let out = run_count_only(&msgs, budget, Some(6), None, &mut latched).await;
        assert_ne!(out.action, CompressAction::Refused);
        assert!(
            out.messages.len() < msgs.len(),
            "the VRAM guard must stay alive while anti-thrash is latched"
        );
    }

    // -- boundary -------------------------------------------------------------

    #[test]
    fn boundary_head_is_system_plus_original_task() {
        let msgs = tool_heavy("the task", 6, 1_000);
        let b = compute_boundary(&msgs, 1_000, None);
        assert_eq!(b.head, 2, "system + original task");

        // Multiple system messages all land in the head.
        let mut msgs2 = vec![sys("a"), sys("b"), user("task"), user("more")];
        msgs2.extend(tool_heavy("x", 4, 1_000).split_off(2));
        assert_eq!(compute_boundary(&msgs2, 1_000, None).head, 3);
    }

    #[test]
    fn boundary_tail_is_token_budgeted_with_minimum() {
        // 10 rounds of ~250-token results; budget 4_000 → tail budget 1_000.
        let msgs = tool_heavy("task", 10, 1_000);
        let b = compute_boundary(&msgs, 4_000, None);
        let tail_tokens: usize = msgs[b.tail_start..].iter().map(estimate_value_tokens).sum();
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
        let b = compute_boundary(&msgs, 4_000, None);
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
        let b = compute_boundary(&msgs, 2_000, None);
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
        let mut msgs = vec![sys("you are newt"), user("the task")];
        msgs.push(summary_message("## Active Task\nthe task (summarized)"));
        for i in 0..6 {
            msgs.push(assistant_call(
                "read_file",
                json!({"path": format!("f{i}")}),
            ));
            msgs.push(tool_result(&"q".repeat(4_000)));
        }
        let b = compute_boundary(&msgs, 2_000, None);
        assert!(
            b.tail_start > 2,
            "the tail must not pin to the compaction message at index 2 \
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
        let b2 = compute_boundary(&msgs2, 2_000, None);
        assert!(
            b2.tail_start <= follow_up,
            "a real user message still anchors the tail"
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
        let b = compute_boundary(&msgs, 4_000, Some(10));
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
        let b_token = compute_boundary(&msgs, 4_000, None);
        assert!(b_token.tail_start <= task_idx);
    }

    #[test]
    fn boundary_never_splits_a_tool_pair() {
        for budget in [1_000usize, 2_000, 4_000, 8_000, 16_000] {
            let msgs = tool_heavy("task", 8, 2_000);
            let b = compute_boundary(&msgs, budget, None);
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

    // -- anti-thrash ------------------------------------------------------------

    /// Two consecutive <10% reclaims disable compression, the user is
    /// notified exactly once, and further over-budget calls are refused.
    #[tokio::test]
    async fn anti_thrash_disables_notifies_once_then_refuses() {
        // Incompressible over-budget input: user messages only (nothing for
        // prune), head+tail protection covering everything (no middle).
        let mut msgs = vec![sys(&"s".repeat(4_000)), user("task")];
        for i in 0..3 {
            msgs.push(user(&format!("note {i}")));
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

    /// Effective compressions never trip the anti-thrash switch.
    #[tokio::test]
    async fn effective_compressions_do_not_disable() {
        let mut state = CompressState::new();
        for _ in 0..4 {
            let msgs = tool_heavy("task", 6, 4_000);
            let before = estimate_tokens(&msgs);
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
        state.record(1_000, 990); // poor
        state.record(1_000, 400); // good
        state.record(1_000, 990); // poor
        assert!(!state.disabled, "non-consecutive poor passes never disable");
        state.record(1_000, 950); // poor — now two in a row
        assert!(state.disabled);
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
        let out = compress_user_initiated(&msgs, None, Some(&*s), &mut state).await;

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
        // The original-task anchor was derived from the working set itself.
        let p = prompts.lock().unwrap();
        assert!(p[0].contains("ORIGINAL TASK: port the parser"), "{}", p[0]);
        // Fired manual runs feed the effectiveness counters.
        let c = state.counters();
        assert_eq!(c.compressions, 1);
        assert_eq!(c.strikes, 0, "a good reclaim is not a strike");
        assert!(c.last_reclaim.unwrap() > THRASH_MIN_SAVINGS);
        assert!(!c.disabled);
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
        let out = compress_user_initiated(&msgs, Some(&focus), Some(&*s), &mut state).await;
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
        compress_user_initiated(&msgs, None, Some(&*s), &mut state).await;
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
            let out = compress_user_initiated(&msgs, None, None, &mut state).await;
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
        let out = compress_user_initiated(&msgs, None, None, &mut state).await;
        assert!(out.fired, "an explicit ask must bypass the latch");
        assert_eq!(out.how, CompressAction::StaticFallback.describe());
        assert!(state.is_disabled(), "the latch itself stays set");
    }

    /// Counters snapshot: a pure projection of the recorded state.
    #[test]
    fn counters_snapshot_projects_state() {
        let mut state = CompressState::new();
        let c = state.counters();
        assert_eq!((c.compressions, c.strikes, c.disabled), (0, 0, false));
        assert_eq!(c.last_reclaim, None);

        state.record(1_000, 400); // good: 60% reclaim
        let c = state.counters();
        assert_eq!((c.compressions, c.strikes, c.disabled), (1, 0, false));
        assert!((c.last_reclaim.unwrap() - 0.6).abs() < 0.01);

        state.record(1_000, 990); // poor — one strike
        let c = state.counters();
        assert_eq!((c.compressions, c.strikes, c.disabled), (2, 1, false));

        state.record(1_000, 950); // poor — two in a row latches
        let c = state.counters();
        assert_eq!((c.compressions, c.strikes, c.disabled), (3, 2, true));
        assert!(c.last_reclaim.unwrap() < THRASH_MIN_SAVINGS);
    }

    /// A single poor FIRST attempt is one strike, not two: the [1.0, 1.0]
    /// sentinel in the unused slot must never read as a recorded strike.
    #[test]
    fn counters_first_poor_attempt_is_one_strike() {
        let mut state = CompressState::new();
        state.record(1_000, 990);
        assert_eq!(state.counters().strikes, 1);
    }

    // -- trigger ------------------------------------------------------------------

    #[test]
    fn trigger_fires_on_count_token_or_guard() {
        // Nothing fired.
        assert!(compression_trigger(10, 1_000, 900, 40, None, None, 100).is_none());
        // Token threshold (issue #223's crux: count far under threshold).
        assert_eq!(
            compression_trigger(4, 60_000, 59_000, 40, Some(50_000), None, 100),
            Some(CompressTrigger {
                budget: 50_000,
                max_messages: None,
                hard_budget: true,
            })
        );
        // Guard: budget = send_budget − tool schema tokens.
        assert_eq!(
            compression_trigger(4, 9_000, 8_600, 40, None, Some(8_000), 500),
            Some(CompressTrigger {
                budget: 7_500,
                max_messages: None,
                hard_budget: true,
            })
        );
        // Count only: budget halves the MESSAGE-token figure (NOT the
        // schema-inclusive current figure — the F1 cross-currency bug),
        // max_messages set, and the budget is soft (no anti-thrash).
        assert_eq!(
            compression_trigger(41, 1_000, 800, 40, None, None, 100),
            Some(CompressTrigger {
                budget: 400,
                max_messages: Some(20),
                hard_budget: false,
            })
        );
        // All at once: the tightest token budget wins and stays hard.
        assert_eq!(
            compression_trigger(41, 60_000, 59_000, 40, Some(50_000), Some(20_000), 500),
            Some(CompressTrigger {
                budget: 19_500,
                max_messages: Some(20),
                hard_budget: true,
            })
        );
        // Under-threshold figures don't fire their triggers.
        assert!(compression_trigger(4, 7_999, 7_000, 40, Some(50_000), Some(8_000), 0).is_none());
    }

    /// Re-homed `trim_to_token_budget_zero_is_noop` (F3): a configured zero
    /// token budget means DISABLED — `Some(0)` must not fire (the 18.4
    /// regression flipped it to "compress to budget zero every round").
    #[test]
    fn trigger_zero_token_budget_is_disabled() {
        assert!(compression_trigger(4, 100, 90, 40, Some(0), None, 0).is_none());
        assert!(compression_trigger(4, 100, 90, 40, None, Some(0), 10).is_none());
        // Zero token budgets stay disabled while a real count trigger fires.
        assert_eq!(
            compression_trigger(41, 100, 90, 40, Some(0), Some(0), 10),
            Some(CompressTrigger {
                budget: 45,
                max_messages: Some(20),
                hard_budget: false,
            })
        );
    }

    // -- redaction ----------------------------------------------------------------

    #[test]
    fn redaction_catches_true_positives() {
        let cases = [
            (
                "the key is sk-AbCdEf1234567890AbCdEf1234567890",
                "sk-AbCdEf",
            ),
            ("ghp_AbCdEf1234567890AbCdEf1234567890", "ghp_"),
            ("github_pat_11ABCDEFG0123456789_abcdefghij", "github_pat_"),
            ("aws id AKIAIOSFODNN7EXAMPLE", "AKIAIOSFODNN7"),
            (
                "Authorization: Bearer abc.def-ghi_jkl012345678901234567890",
                "abc.def-ghi",
            ),
            ("api_key=9f8e7d6c5b4a32100ffee", "9f8e7d6c"),
            ("password: \"hunter2hunter2\"", "hunter2hunter2"),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abc123def456",
                "eyJhbGci",
            ),
        ];
        for (input, leaked) in cases {
            let out = redact_secrets(input);
            assert!(
                !out.contains(leaked),
                "secret fragment {leaked:?} survived: {out}"
            );
            assert!(out.contains("[REDACTED]"), "no redaction marker: {out}");
        }
        // A private key block, including an unterminated one.
        let key = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow…\n-----END RSA PRIVATE KEY-----";
        assert!(!redact_secrets(key).contains("MIIEow"));
        let cut = "-----BEGIN PRIVATE KEY-----\nMIIEow… (truncated)";
        assert!(!redact_secrets(cut).contains("MIIEow"));
    }

    #[test]
    fn redaction_passes_benign_near_misses() {
        let benign = [
            "the api key is stored in the system keychain",
            "the token budget is 4096 tokens per request",
            "Bearer of good news: the build is green",
            "sk-test was rejected (too short to be a real key)",
            "set password: yes in sshd_config",
            "AKIAFOO is not a full key id",
            "ghp_short",
            "the access_token field is documented in docs/api.md",
            "run `cargo test -p newt-core` and check the password prompt",
        ];
        for input in benign {
            let out = redact_secrets(input);
            assert_eq!(out, input, "benign text must pass unchanged");
        }
    }

    #[test]
    fn redaction_applies_inside_the_summary_request() {
        let middle = vec![tool_result(
            "config: api_key=9f8e7d6c5b4a32100ffee and more text",
        )];
        let request = redact_secrets(&summary_request("the task", &middle, usize::MAX, None));
        assert!(!request.contains("9f8e7d6c5b4a32100ffee"), "{request}");
        assert!(request.contains("api_key=[REDACTED]"), "{request}");
        assert!(request.contains("the task"), "task still present verbatim");
    }

    /// F6: tool-call args reach the summarizer rendered AS JSON — the
    /// quoted-key credential shape must redact.
    #[test]
    fn redaction_catches_json_quoted_credential_keys() {
        let cases = [
            (r#"{"api_key": "9f8e7d6c5b4a32100ffee"}"#, "9f8e7d6c"),
            (r#"{"password": "hunter2hunter2"}"#, "hunter2hunter2"),
            (
                r#"body: "client_secret": "abcd1234efgh5678ijkl""#,
                "abcd1234",
            ),
        ];
        for (input, leaked) in cases {
            let out = redact_secrets(input);
            assert!(
                !out.contains(leaked),
                "secret fragment {leaked:?} survived: {out}"
            );
            assert!(out.contains("[REDACTED]"), "no redaction marker: {out}");
        }
    }

    /// N4: redaction runs BEFORE excerpting — a credential the excerpt cap
    /// would slice mid-value must not leak a fragment too short for any
    /// pattern to match afterward.
    #[test]
    fn redaction_survives_excerpt_truncation() {
        let secret = "sk-AbCdEf1234567890AbCdEf1234567890";
        // The serialized args put the secret astride the 200-char arg cap:
        // unredacted it would be cut to an unmatchable `sk-…` fragment.
        let args = json!({
            "command": format!("{} && export OPENAI_API_KEY={secret}", "x".repeat(140))
        });
        let m = assistant_call("run_command", args);
        let line = render_message(&m);
        assert!(!line.contains("sk-AbC"), "{line}");
        assert!(!line.contains("AbCdEf123"), "no fragment may leak: {line}");
        assert!(line.contains("[REDACTED]"), "{line}");
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
        let capped = summary_request("the task", &middle, 8_192, None);
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
        let uncapped = summary_request("the task", &middle, usize::MAX, None);
        assert!(uncapped.chars().count() > 90_000);
        assert!(!uncapped.contains("older message(s) omitted"));
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
}
