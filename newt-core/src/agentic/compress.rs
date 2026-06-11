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

// ---------------------------------------------------------------------------
// Trigger
// ---------------------------------------------------------------------------

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
/// fired budget wins. Returns `(budget, max_messages)` — `max_messages` is
/// set only by the count trigger, because structural pruning alone can never
/// satisfy it (pruning never removes messages).
pub(crate) fn compression_trigger(
    len: usize,
    current_tokens: usize,
    count_threshold: usize,
    token_threshold: Option<usize>,
    send_budget: Option<usize>,
    tool_tokens: usize,
) -> Option<(usize, Option<usize>)> {
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
    if budget == usize::MAX {
        // Count-only trigger: no token target configured — aim to halve.
        budget = current_tokens / 2;
    }
    Some((budget, count_fired.then_some(count_threshold / 2)))
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
    // Anti-thrash protects the token budget (the correctness guard); pure
    // message-count invocations (the VRAM guard) neither consult nor feed it.
    let tokens_over_entry = tokens_before > req.budget;
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
    if state.disabled {
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
        // Only the message-count ceiling is exceeded: an oversized-but-
        // within-budget history is safe to send — pass through unchanged.
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
                let request = redact_secrets(&summary_request(req.task, middle));
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
    // budget (B6's shape — one giant tool round), one-line it rather than
    // letting the backend silently truncate the head (and the task) away.
    if estimate_tokens(&assembled) > req.budget {
        let aggressive = prune(
            &assembled,
            &PruneConfig {
                keep_last: 0,
                ..PruneConfig::default()
            },
        );
        if aggressive.chars_reclaimed > 0 {
            assembled = aggressive.messages;
            if action == CompressAction::Fit {
                action = CompressAction::Pruned;
            }
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

    // Last-user anchor: the most recent user message is never summarized
    // away (hermes #10896 — losing it loses the active request).
    if let Some(last_user) = messages
        .iter()
        .rposition(|m| m["role"].as_str() == Some("user"))
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

    Boundary { head, tail_start }
}

/// Length of the protected head: every leading `system` message plus the
/// first `user` message after them (the original task).
fn head_len(messages: &[Value]) -> usize {
    let mut head = 0;
    while head < messages.len() && messages[head]["role"].as_str() == Some("system") {
        head += 1;
    }
    if head < messages.len() && messages[head]["role"].as_str() == Some("user") {
        head += 1;
    }
    head
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
/// middle, and the `Summarizing` provider's lean section template extended
/// with the In-Progress slot and the verbatim-Active-Task rule (design doc
/// §Phase 18 "Deliberately different from hermes").
fn summary_request(task: &str, middle: &[Value]) -> String {
    let mut p = String::with_capacity(1024);
    p.push_str("You are compressing the middle of a coding-agent conversation.\n\n");
    p.push_str("## Original Task (copy this VERBATIM into \"## Active Task\")\n");
    p.push_str(task);
    p.push_str("\n\n## Conversation middle to summarise\n");
    for m in middle {
        p.push_str(&render_message(m));
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
    p
}

/// Render one wire-shape message as a line of summarizer input.
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
            line.push_str(&excerpt(&args, 200));
            line.push(')');
        }
    }
    if let Some(content) = m["content"].as_str() {
        if !content.is_empty() {
            line.push(' ');
            line.push_str(&excerpt(content, SUMMARY_INPUT_MSG_CAP));
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
    // "token"/"key") so token-budget talk passes.
    (
        r#"(?i)\b(api[_-]?key|secret[_-]?key|access[_-]?token|auth[_-]?token|client[_-]?secret|password|passwd)\b\s*[:=]\s*["']?[^\s"']{8,}["']?"#,
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

    /// The B6 shape: one giant tool round adjacent to the tail that no
    /// boundary can split — the final structural fit pass one-lines the
    /// results instead of letting the backend silently truncate the head.
    #[tokio::test]
    async fn giant_single_round_is_pruned_aggressively_not_shipped_over_budget() {
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
        // Pairing intact: 3 calls, 3 (one-lined) results.
        assert_eq!(out.messages[2]["tool_calls"].as_array().unwrap().len(), 3);
        assert_eq!(
            out.messages
                .iter()
                .filter(|m| m["role"].as_str() == Some("tool"))
                .count(),
            3
        );
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
        let out = run(&msgs, before + 1_000, Some(8), Some(&*s), &mut state).await;
        assert_eq!(out.action, CompressAction::Summarized);
        assert!(out.messages.len() < msgs.len());
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

    // -- trigger ------------------------------------------------------------------

    #[test]
    fn trigger_fires_on_count_token_or_guard() {
        // Nothing fired.
        assert!(compression_trigger(10, 1_000, 40, None, None, 100).is_none());
        // Token threshold (issue #223's crux: count far under threshold).
        assert_eq!(
            compression_trigger(4, 60_000, 40, Some(50_000), None, 100),
            Some((50_000, None))
        );
        // Guard: budget = send_budget − tool schema tokens.
        assert_eq!(
            compression_trigger(4, 9_000, 40, None, Some(8_000), 500),
            Some((7_500, None))
        );
        // Count only: budget halves the current figure, max_messages set.
        assert_eq!(
            compression_trigger(41, 1_000, 40, None, None, 100),
            Some((500, Some(20)))
        );
        // All at once: the tightest token budget wins.
        assert_eq!(
            compression_trigger(41, 60_000, 40, Some(50_000), Some(20_000), 500),
            Some((19_500, Some(20)))
        );
        // Under-threshold figures don't fire their triggers.
        assert!(compression_trigger(4, 7_999, 40, Some(50_000), Some(8_000), 0).is_none());
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
        let request = redact_secrets(&summary_request("the task", &middle));
        assert!(!request.contains("9f8e7d6c5b4a32100ffee"), "{request}");
        assert!(request.contains("api_key=[REDACTED]"), "{request}");
        assert!(request.contains("the task"), "task still present verbatim");
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
