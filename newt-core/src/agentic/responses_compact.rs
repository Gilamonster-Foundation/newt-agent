//! Responses compaction orchestration — the transactional compaction path that
//! coordinates the provenance bridge, the compressor, the spill store, and the
//! anti-thrash state. This is the single entry point for BOTH the proactive
//! pre-dispatch guard and the reactive cw-400 recovery (#1528 B3).

use crate::agentic::content_spill;
use crate::agentic::responses_compaction;
use crate::agentic::smart_harness;
use crate::agentic::tools;
use crate::agentic::CompressState;
use crate::agentic::SummarizeFn;
use crate::agentic::context_recovery;
use crate::agentic::display::print_harness_notice;
use crate::agentic::send_budget::estimate_responses_request_real_tokens;
use crate::agentic::compress::{CompressAction, CompressRequest, RefusalReason};
use crate::tokens::TokenEstimation;
use crate::worktree_adoption::WorktreeSession;
use anyhow::Result;
use serde_json::Value;
use std::sync::Mutex;

/// The outcome of the ONE Responses compaction path (#1528 B2/B3): classify the
/// running `input` into typed provenance, compact via the ONE `compress`, rebuild
/// EXHAUSTIVELY, and validate the fenced request against the budget. Both triggers
/// — the REACTIVE cw-400 recovery and the PROACTIVE pre-dispatch guard — call the
/// same [`compact_responses_input`]; they differ ONLY in how they MAP this outcome
/// (reactive surfaces the provider's original 400; proactive fails closed with a
/// fresh refusal). Making the compaction unrepresentable in two places is the
/// point (reuse discipline): a fix or a guard added here can never be missed at
/// one call site.
#[derive(Debug)]
pub(crate) enum ResponsesCompaction {
    HarnessFailure(String),
    /// Compression fired and the rebuilt (fenced) request fits the budget — the
    /// caller's `input` was rewritten to the compacted form.
    Compacted,
    /// Compression did not fire (nothing left to reclaim) — `input` unchanged.
    NotFired,
    /// The compressor refused: the protected head alone exceeds the budget.
    Refused,
    /// The compressor refused because the selected context manager is
    /// append-only — a different cause with a different remedy, kept distinct so
    /// the rejection diagnostic cannot assert a head size it never measured.
    AppendOnlyRefused,
    /// The rebuilt request still exceeds the budget AFTER fencing — `input` is left
    /// UNCHANGED (BHV-BUDGET-004: never dispatch an oversized request).
    OverBudgetAfterFence(responses_compaction::PostBridgeBudgetExceeded),
    /// `input` could not be classified (a forbidden `system` item) — fail closed.
    BridgeError,
    /// The staged compaction spill batch could not be committed (a poisoned store or a
    /// content-integrity violation) — fail CLOSED, committing NOTHING (BHV-SPILL-007).
    SpillCommitFailed(content_spill::SpillError),
}

/// The bail message for a [`CompressAction::Refused`] outcome.
///
/// The two refusal causes need DIFFERENT remedies — one is a stuck pipeline, the
/// other is the operator's own policy choice — so the message is selected by
/// [`RefusalReason`] rather than asserting the anti-thrash cause unconditionally.
/// Sending an append-only operator to `newt tunings reset` wastes their time and
/// hides the one action that resolves it.
pub(crate) fn refusal_bail_message(reason: Option<RefusalReason>, current: usize, model: &str) -> String {
    match reason {
        Some(RefusalReason::AppendOnly) => format!(
            "context (~{current} tokens) exceeds the model's input budget and the \
             append-only context manager never rewrites recorded turns — select \
             `/context manager standard` to re-enable compaction, or start a new \
             conversation / ask a more focused question"
        ),
        _ => format!(
            "context (~{current} tokens) exceeds the model's input budget and \
             auto-compression is disabled after repeated ineffective passes — \
             start a new conversation or ask a more focused question, or run \
             `newt tunings reset {model}` if this model's learned budget looks wrong"
        ),
    }
}

/// Why a proactive / reactive compaction attempt did NOT yield a dispatchable
/// request (#1528 B3, item 4). Carried into the fail-closed error CHAIN (as anyhow
/// context on the authoritative preflight, or the provider error for reactive
/// recovery) so headless / structured callers see the reason WITHOUT depending on
/// terminal rendering. No user-facing notice is emitted for a rejected candidate;
/// this reason IS the single diagnostic.
#[derive(Debug, Clone)]
pub(crate) enum CompactionRejection {
    HarnessFailure(String),
    /// `input` could not be classified into typed provenance (a forbidden `system`
    /// item) — BHV-PROVENANCE-002.
    BridgeClassification,
    /// The compressor refused: the protected head alone exceeds the budget.
    CompressorRefused,
    /// The compressor refused because the selected context manager is append-only.
    AppendOnlyRefused,
    /// Compaction could not reduce the request further (nothing left to reclaim).
    NoProgress,
    /// The fenced rebuild still exceeds the budget after framing (BHV-BUDGET-004).
    PostBridgeBudgetExceeded(responses_compaction::PostBridgeBudgetExceeded),
    /// The compacted candidate fit, but its staged spill batch could not be committed
    /// (a poisoned store or a content-integrity violation) — fail closed (BHV-SPILL-007).
    SpillCommit(content_spill::SpillError),
}

impl std::fmt::Display for CompactionRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HarnessFailure(error) => write!(f, "smart harness projection: {error}"),
            Self::BridgeClassification => write!(
                f,
                "proactive compaction could not classify the request (a forbidden system item)"
            ),
            Self::CompressorRefused => write!(
                f,
                "proactive compaction refused: the protected head alone exceeds the input budget"
            ),
            Self::AppendOnlyRefused => write!(
                f,
                "proactive compaction refused: the append-only context manager never rewrites \
                 recorded turns — select `/context manager standard` to re-enable compaction"
            ),
            Self::NoProgress => write!(
                f,
                "proactive compaction made no progress: nothing left to reclaim under the budget"
            ),
            Self::PostBridgeBudgetExceeded(e) => {
                write!(f, "proactive compaction fit the compressor budget but {e}")
            }
            Self::SpillCommit(e) => write!(
                f,
                "proactive compaction could not commit its staged spill batch \
                 ({e:?}) — refusing to install a request that names an uncommitted spill"
            ),
        }
    }
}

impl ResponsesCompaction {
    /// The rejection reason when this outcome is NOT a committed compaction — for
    /// the fail-closed error chain. `Compacted` → `None`; `NotFired` (compaction
    /// made no reduction) → `NoProgress`.
    pub(crate) fn rejection(self) -> Option<CompactionRejection> {
        match self {
            Self::HarnessFailure(error) => Some(CompactionRejection::HarnessFailure(error)),
            Self::Compacted => None,
            Self::NotFired => Some(CompactionRejection::NoProgress),
            Self::Refused => Some(CompactionRejection::CompressorRefused),
            Self::AppendOnlyRefused => Some(CompactionRejection::AppendOnlyRefused),
            Self::BridgeError => Some(CompactionRejection::BridgeClassification),
            Self::OverBudgetAfterFence(e) => Some(CompactionRejection::PostBridgeBudgetExceeded(e)),
            Self::SpillCommitFailed(e) => Some(CompactionRejection::SpillCommit(e)),
        }
    }
}

/// A candidate-local staging buffer for the compaction spill(s) built while
/// compressing a Responses candidate (#1528 B3, §2.6). The compressor STAGES each
/// evicted middle span into this buffer PURELY (the CID is a pure function of the
/// content, so its `compaction:<cid>` handle can ride the summary before any commit);
/// nothing reaches the live store. [`compact_responses_input`] drains it and
/// `commit_batch`es into the live store ONLY on the accept branch — a rejected
/// candidate drops the buffer, leaving the live store untouched (BHV-SPILL-006/007).
pub(crate) type CompactionStageBuffer =
    Mutex<Vec<content_spill::StagedSpill>>;

/// Compact the Responses `input` toward `compaction_budget` via the B2 provenance
/// bridge + the ONE `compress`, then validate the fenced rebuild against
/// `actionable_budget` before committing.
///
/// TRANSACTIONAL (#1528 B3, B3-CG-004 / §2.6): the compressor runs against a CLONE of
/// `compress_state` AND a candidate-local [`CompactionStageBuffer`] — the evicted
/// middle span(s) are STAGED PURELY (no live-store mutation) while the candidate is
/// validated (provenance + post-bridge budget). On success — and only then — the
/// helper commits ALL FOUR effects together: it rewrites `*input`, writes the
/// candidate anti-thrash state back into `*compress_state`, `commit_batch`es the
/// staged spills to the real store, and emits the compaction notice. On ANY
/// reject/refuse/over-budget/bridge-error path, `*input`, `*compress_state`, the spill
/// store, and the notice stream are ALL left UNTOUCHED — the staged buffer is dropped
/// and no `compaction:<cid>` was committed — so no caller can dispatch a
/// half-compacted request or believe an uncommitted compaction occurred. Single-shot
/// (one attempt) — the caller decides whether to gate on an estimate (proactive) or
/// always call (reactive), and how to map a non-`Compacted` outcome (see
/// [`CompactionRejection`]).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn compact_responses_input(
    observed_facts: Option<&crate::agentic::semantic_pins::Projection>,
    worktree_session: Option<&WorktreeSession>,
    input: &mut Vec<Value>,
    instructions: Option<&str>,
    tools: Option<&[Value]>,
    actionable_budget: Option<usize>,
    compaction_budget: usize,
    cal: f32,
    estimation: TokenEstimation,
    task: &str,
    summary_input_cap_floor_chars: usize,
    rewrites_history: bool,
    compaction_store: Option<&dyn content_spill::SpillStore>,
    summarizer: Option<&SummarizeFn>,
    compress_state: &mut CompressState,
    color: bool,
    smart_harness: Option<&smart_harness::SmartHarness>,
    overflow_recovery: bool,
    exposure: &tools::ExposureSettings,
) -> ResponsesCompaction {
    // #2766: the task location outlives lossy history. Reuse the continuation
    // marker so repeated compactions replace the old hint; include its bytes
    // before the existing transactional request-budget checks in either path.
    let with_task = |mut candidate: Vec<Value>| {
        if let Some(hint) = worktree_session.and_then(WorktreeSession::task_hint) {
            candidate.retain(|item| !crate::agentic::compress::is_continuation_message(item));
            candidate.push(serde_json::json!({"role":"user", "content":
                format!("{} {}", crate::agentic::compress::CONTINUATION_PREFIX, workflow_guidance(hint, exposure))}));
        }
        candidate
    };

    if let Some(harness) = smart_harness {
        // Preserve native call IDs and reasoning items through host selection.
        // The legacy summarizer bridge intentionally flattens them to prose.
        let max_bytes =
            match smart_harness::projection_byte_budget(input, compaction_budget, estimation) {
                Ok(max_bytes) => max_bytes,
                Err(error) => return ResponsesCompaction::HarnessFailure(error.to_string()),
            };
        let projected = match harness.project(input, max_bytes).await {
            Ok(messages) => with_task(messages),
            Err(error) => return ResponsesCompaction::HarnessFailure(error.to_string()),
        };
        let required = estimate_responses_request_real_tokens(
            instructions,
            &projected,
            tools,
            estimation,
            cal,
        );
        if let Some(budget) = actionable_budget.filter(|budget| required > *budget) {
            return ResponsesCompaction::OverBudgetAfterFence(
                responses_compaction::PostBridgeBudgetExceeded {
                    actionable_budget: budget,
                    post_bridge_estimate: required,
                    framing_overhead: 0,
                },
            );
        }
        if projected == *input {
            return ResponsesCompaction::NotFired;
        }
        *input = projected;
        return ResponsesCompaction::Compacted;
    }
    // Classify the Responses `input` into TYPED provenance (fail-closed) and render
    // it to chat with `instructions` as a protected system head. A forbidden
    /// `system` item in `input` fails closed here (BHV-PROVENANCE-002).
    // Only the exact note owned by this turn bypasses the untrusted-input
    /// bridge. Unknown system items still fail closed; no marker classification.
    let (host_notes, conversation): (Vec<_>, Vec<_>) = input
        .iter()
        .cloned()
        .partition(|item| observed_facts.is_some_and(|facts| facts.owns_note(item)));
    let messages = match responses_compaction::responses_input_to_compaction(&conversation) {
        Ok(m) => m,
        Err(_) => return ResponsesCompaction::BridgeError,
    };
    let mut chat = host_notes.clone();
    if let Some(ins) = instructions {
        chat.push(serde_json::json!({ "role": "system", "content": ins }));
    }
    chat.extend(responses_compaction::compaction_to_chat(&messages));
    // Run the compressor against a CANDIDATE state AND a candidate-local staging
    // buffer — the live anti-thrash counters / disabled latch and the session
    /// compaction store are mutated only on commit (below). The live `compaction_store`
    /// is passed so the compressor can STAMP its session scope onto each span and render
    /// the pure `compaction:<cid>` handle; the buffer receives the staged span(s)
    /// instead of the live store, so nothing is committed until accept. A `None` store
    /// passes through as `None` (no buffer), so with no real store the compressor
    /// invents no retrieval handle (correction 1).
    let mut candidate_state = compress_state.clone();
    let stage_buffer: CompactionStageBuffer = Mutex::new(Vec::new());
    let request = CompressRequest {
        messages: &chat,
        // Real-token cap minus real-token schema overhead → pipeline chars/4
        // currency (mirrors the Chat path).
        budget: compaction_budget,
        max_messages: None,
        replay_protected_tail_len: context_recovery::protected_tool_tail(&chat),
        task,
        hard_budget: true,
        authoritative: true,
        focus: None,
        est: estimation,
        summary_input_cap_floor_chars,
        rewrites_history,
        compaction_store,
        compaction_stage: compaction_store.map(|_| &stage_buffer),
    };
    let compression = if overflow_recovery {
        context_recovery::compress(request, summarizer, &mut candidate_state, smart_harness).await
    } else {
        smart_harness::compress(request, summarizer, &mut candidate_state, smart_harness).await
    };
    let outcome = match compression {
        Ok(outcome) => outcome,
        Err(error) => return ResponsesCompaction::HarnessFailure(error.to_string()),
    };
    // The notice is NOT emitted yet — only a committed compaction surfaces one.
    if outcome.action == CompressAction::Refused {
        // Carry the CAUSE across, not just the fact: `CompressorRefused` names a
        // protected-head size that the append-only guard returns before ever
        // measuring, and this string is the diagnostic headless callers read.
        return match outcome.refusal {
            Some(RefusalReason::AppendOnly) => ResponsesCompaction::AppendOnlyRefused,
            _ => ResponsesCompaction::Refused,
        };
    }
    if !outcome.fired {
        return ResponsesCompaction::NotFired;
    }
    // Drop the instructions system head we prepended for head protection:
    // Both the base instructions and owned facts stay in the protected head.
    let compacted = &outcome.messages;
    let protected = usize::from(instructions.is_some()) + host_notes.len();
    let rebuilt_chat = &compacted[protected..];
    // Reclassify to typed provenance, then rebuild exhaustively (untrusted-derived
    /// → fenced `user`).
    let rebuilt_msgs = responses_compaction::chat_to_compaction(rebuilt_chat);
    let with_notes = |mut items: Vec<Value>| {
        let mut result = host_notes.clone();
        result.append(&mut items);
        result
    };
    let rebuilt_input = with_task(with_notes(responses_compaction::compaction_to_responses(
        &rebuilt_msgs,
    )));
    // #1528 B2 (BHV-BUDGET-004): the provenance fences are added AFTER the
    // compressor fit its budget, so validate the REBUILT request against the
    // authoritative budget (the SHARED B1 estimator, same real-token currency
    /// dispatch enforces) BEFORE committing. `pre_bridge` estimates the SAME
    /// messages WITHOUT the fences, attributing any overflow to the framing.
    if let Some(budget) = actionable_budget {
        let est_tokens = |items: &[Value]| {
            estimate_responses_request_real_tokens(instructions, items, tools, estimation, cal)
        };
        let post_bridge = est_tokens(&rebuilt_input);
        let pre_bridge = est_tokens(&with_notes(
            responses_compaction::rebuild_unfenced_for_estimate(&rebuilt_msgs),
        ));
        if let Err(exceeded) =
            responses_compaction::check_post_bridge_budget(budget, pre_bridge, post_bridge)
        {
            // REJECT: the staged buffer is dropped WITHOUT commit, so every staged span
            // is retired unused — nothing is fetchable in the live store, its counts are
            // unchanged; `*input`, `*compress_state`, and the notice stream are likewise
            // UNTOUCHED — the candidate is discarded whole.
            return ResponsesCompaction::OverBudgetAfterFence(exceeded);
        }
    }
    // COMMIT — all FOUR effects, now that the candidate is accepted. Ordered so live
    // input never references an uncommitted spill: install the staged spill batch
    // FIRST — and if that batch commit fails (a poisoned buffer or a content-integrity
    // violation), fail CLOSED, leaving `*input`, `*compress_state`, and the notice
    // stream UNTOUCHED (BHV-SPILL-007). Then swap in the input that names those spills,
    // the anti-thrash state, and the notice.
    if let Some(store) = compaction_store {
        let staged = match stage_buffer.into_inner() {
            Ok(s) => s,
            Err(_) => {
                return ResponsesCompaction::SpillCommitFailed(
                    content_spill::SpillError::PoisonedStore,
                )
            }
        };
        if !staged.is_empty() {
            if let Err(e) = store.commit_batch(&staged) {
                return ResponsesCompaction::SpillCommitFailed(e);
            }
        }
    }
    *input = rebuilt_input;
    *compress_state = candidate_state;
    if let Some(notice) = outcome.notice {
        print_harness_notice(&notice, color);
    }
    ResponsesCompaction::Compacted
}

/// Use the workflow_guidance from the parent module for task hint injection.
use super::workflow_guidance;