//! The agentic loop — call model → execute tool calls → feed results back →
//! repeat — extracted verbatim from `newt-tui` in Step 9.7 so the same
//! battle-tested loop can serve both the TUI and the ACP worker (Step 9.8)
//! without being written twice.
//!
//! `ChatCtx` stays a **concrete** type: there is exactly one loop
//! implementation and no `InferenceBackend` trait (YAGNI per the roadmap —
//! revisit only when a second concrete backend exists). The only seam is
//! [`McpTools`], which breaks the `newt-core` ← `newt-mcp-client` dependency
//! cycle; see `mcp.rs`.

// pub(crate) since Step 18.5 (#247): the `Summarizing` memory provider
// delegates to this same pipeline instead of keeping a duplicate one.
pub(crate) mod compress;
mod crew_tool;
mod display;
mod git_tool;
// Issue #308 — the cowork foundation: a non-blocking turn driver around
// `chat_complete` (driver), a renderer-agnostic transcript render (transcript),
// and the redaction-gated ShellObservation seam (observation). All additive;
// they wrap/precede `chat_complete` and never touch its internals.
mod driver;
mod mcp;
mod memory_fetch;
mod note_sink;
mod observation;
mod permissions;
mod recall;
mod tools;
mod transcript;
mod trim;
mod warmup;

pub use compress::{
    compress_user_initiated, CompressCounters, CompressState, ManualCompressOutcome, SummarizeFn,
    SummarizeFuture, Summarizer, SUMMARY_END_MARKER, SUMMARY_PREFIX,
};
pub use crew_tool::{compose_roster_tool_definition, crew_tool_definition, CrewRunner};
pub use display::{print_harness_notice, print_list_item, print_newt, NEWT_ORANGE_CT};
pub use driver::{
    TurnDriver, TurnDriverConfig, TurnDriverError, TurnOutcome, TurnStatus,
    VISIBLE_TRANSCRIPT_ROLES,
};
pub use git_tool::{git_tool_definition, GitTool};
pub use mcp::{McpTools, NoMcp};
pub use memory_fetch::{
    memory_fetch_tool_definition, MemAddr, MemPayload, MemorySource, StoreMemorySource,
};
pub use note_sink::{save_note_tool_definition, NoteNudge, NoteSink};
pub use observation::{ShellObservation, SHELL_OBSERVATION_PREFIX};
pub use permissions::{
    widen_caveats, DenialKind, PermissionDecision, PermissionGate, PermissionRecord,
    PermissionRequest,
};
pub use recall::{recall_tool_definition, RecallSource, StoreRecallSource};
pub use tools::{execute_tool, ocap_disabled, tool_definitions, venv_cmd_prefix};
pub use transcript::{
    transcript_lines, transcript_lines_styled, TranscriptLine, TranscriptRole, TranscriptStyle,
};
pub use trim::trim_for_summary;
pub use warmup::warmup_if_cold;

use crate::retry::{with_backoff_notify, RetryPolicy};
use compress::{compress, compression_trigger, CompressAction, CompressRequest};
use crossterm::{
    execute,
    style::{Color as CtColor, Print, ResetColor, SetForegroundColor},
};
use display::{
    emit_compression_notice, emit_overflow_notice, print_debug, print_retry_indicator, print_trace,
};
use std::io::{self, Write as _};
use tools::{is_hallucination, merged_tool_definitions};
use trim::{
    estimate_request_tokens, estimate_tokens, estimate_value_tokens, merge_round_usage,
    ollama_usage, openai_usage, PromptTracker,
};

/// Retry policy for TUI inference calls: more patient than the hosted-API
/// default because local DGX nodes can drop for 30–60 s under load.
/// Total resilience window: ~90 s (2+4+8+16+30+30 s between attempts).
/// All thresholds are overridable via the standard `NEWT_HTTP_*` env vars.
fn tui_retry_policy() -> RetryPolicy {
    RetryPolicy::for_local_inference()
}

/// Hook recovering a hard context-window 400:
/// `(error, model, today) → new input-token cap`. See [`ChatCtx::recover_cw_400`].
pub type RecoverCw400 = fn(&anyhow::Error, &str, &str) -> Option<u32>;

/// One per-round capability observation, reported through
/// [`ChatCtx::on_round_usage`] at the moment it is observed (Phase 20,
/// `docs/design/model-self-tuning.md` §2.2 — the `recover_cw_400` pattern,
/// generalized to the success direction). Evidence must not wait for a turn
/// epilogue an error can skip: the motivating failure discarded a backend-
/// accepted 8,734-token prompt because the turn later ended in `Err`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundObservation {
    /// Backend evaluated `prompt_tokens` and the round produced a usable
    /// response (tool calls or non-empty content). `estimated_tokens` is the
    /// loop's chars/4 estimate of the same request, for calibration.
    /// Quality-gated AND truncation-gated at the emission sites: never
    /// emitted when the prompt was within 5% of the request's `num_ctx`
    /// (Ollama may have silently dropped the head of the prompt).
    Accepted {
        prompt_tokens: u32,
        estimated_tokens: usize,
    },
    /// Persistent empty responses at `prompt_tokens` after retries (the
    /// 85%-of-safe-context silent-overflow exit).
    SuspectedOverflow { prompt_tokens: u32 },
    /// Response carried only non-content fields (thinking/reasoning) with
    /// empty content.
    ThinkingOnly,
}

/// Input-token ceiling implied by the `num_ctx` THIS request will carry
/// (issue #282, the B6 hole): `num_ctx` caps the backend's whole KV window —
/// input *plus* reply — so the input budget is 80 % of it, the same reply/
/// estimate headroom the probe's budget math already reserves against a hard
/// window (`safe_context` bootstraps at 80 % of the declared window; a cw-400
/// sets `max_ok_input` to 80 % of the parsed limit). `None` (model-default
/// window) or zero contributes nothing — the zero-is-disabled contract (F3),
/// never a compress-to-zero.
fn num_ctx_input_ceiling(num_ctx: Option<u32>) -> Option<usize> {
    num_ctx.map(|c| (c as usize) * 80 / 100).filter(|&c| c > 0)
}

/// Initial pre-send budget for one turn (issue #282; Phase 20 semantics per
/// `docs/design/model-self-tuning.md` §2.1): the empirically-cached figure is
/// `max(max_ok_input, safe_context)` composed, via `min`, with the
/// [`num_ctx_input_ceiling`] of the `num_ctx` the loop is about to send.
///
/// `max_ok_input` is a high-water mark of PROVEN-good input — a floor, not a
/// ceiling. Preferring it over `safe_context` (the pre-Phase-20 contract)
/// turned "largest prompt seen so far" into a cap, which is the motivating
/// failure: a stale 6,068 ratchet refused sends the backend was accepting at
/// 8,734 tokens. `max()` lets whichever of proven-good and believed-safe is
/// larger drive the budget. The cw-400 path already reins `safe_context`
/// down to its authoritative cap, so after a hard 400 `max()` still lands on
/// the authoritative number.
///
/// The `num_ctx` ceiling composition is unchanged: before #282 the budget
/// was the cached numbers alone — unset on a fresh capability cache until
/// the turn ENDS, so the first turn of a session had no effective ceiling
/// and a 41k-token request sailed into a forced 4,096 window with zero
/// compression events (the measured B6 failure: 8/10 silently wrong). The
/// ceiling is a real token budget: when it fires the trigger, `hard_budget`
/// semantics apply (consults + feeds anti-thrash).
fn initial_send_budget(
    max_ok_input: Option<u32>,
    safe_context: Option<u32>,
    num_ctx: Option<u32>,
) -> Option<usize> {
    let cached = match (max_ok_input, safe_context) {
        (Some(m), Some(s)) => Some(m.max(s) as usize),
        (m, s) => m.or(s).map(|c| c as usize),
    };
    match (cached, num_ctx_input_ceiling(num_ctx)) {
        (Some(budget), Some(ceiling)) => Some(budget.min(ceiling)),
        (budget, ceiling) => budget.or(ceiling),
    }
}

/// Convert a chars/4 estimate into real (backend-reported) token space using
/// the learned per-model `estimate_ratio` (Phase 20,
/// `docs/design/model-self-tuning.md` §2.3). Ceiling: estimates must err on
/// the side of counting, never undercounting — the 18.1 rule.
fn calibrate_up(est: usize, ratio: f32) -> usize {
    (est as f32 * ratio).ceil() as usize
}

/// Convert a real-token budget into estimate (chars/4) space — the currency
/// the compression pipeline measures and reclaims in (Phase 20 §2.3).
/// Floor: a tighter target is safer than a looser one.
fn calibrate_down(real: usize, ratio: f32) -> usize {
    (real as f32 / ratio).floor() as usize
}

/// Sanitize a per-model `estimate_ratio` for one turn (Phase 20 §2.3): only
/// finite values inside the learning clamp [0.5, 3.0] are trusted; anything
/// else (absent, NaN, a corrupted cache entry) degrades to 1.0 — the
/// identity, i.e. exactly the pre-calibration behavior.
fn sanitize_estimate_ratio(estimate_ratio: Option<f32>) -> f32 {
    estimate_ratio
        .filter(|r| r.is_finite() && (0.5..=3.0).contains(r))
        .unwrap_or(1.0)
}

/// Report one quality-gated [`RoundObservation::Accepted`] (Phase 20 §2.2).
/// Called only from usable-output control paths (tool calls or non-empty
/// content — the quality gate); skips when the prompt was truncation-suspect
/// (≥95% of the request's `num_ctx`, where Ollama may have silently dropped
/// the head) or when the backend reported no usage for the round.
fn emit_accepted(
    hook: &mut Option<&mut dyn FnMut(RoundObservation)>,
    round_usage: Option<crate::TokenUsage>,
    truncation_suspect: bool,
    estimated_tokens: usize,
) {
    if truncation_suspect {
        return;
    }
    if let (Some(hook), Some(u)) = (hook.as_deref_mut(), round_usage) {
        hook(RoundObservation::Accepted {
            prompt_tokens: u.input_tokens,
            estimated_tokens,
        });
    }
}

// Unit tests for the #282 budget wiring: the `num_ctx` a request will carry
// must participate in the pre-send budget — composing with the cached
// capability numbers via `min`, vanishing when absent, and never turning a
// zero/absent window into a compress-to-zero (F3).
#[cfg(test)]
mod send_budget_tests {
    use super::compress::compression_trigger;
    use super::{initial_send_budget, num_ctx_input_ceiling};

    /// THE B6 first-turn hole: a fresh capability cache (no `max_ok_input`,
    /// no `safe_context`) used to mean NO budget at all even though the
    /// request itself carried `options.num_ctx = 4096`. The ceiling must now
    /// arm the trigger on turn 1 — as a HARD budget (anti-thrash semantics).
    #[test]
    fn first_turn_fresh_cache_trigger_sees_the_num_ctx_ceiling() {
        let budget = initial_send_budget(None, None, Some(4096));
        assert_eq!(budget, Some(3276), "80% of 4096 — reply headroom reserved");
        // The measured B6 shape: ~41k estimated tokens, 3 messages, no
        // count/token thresholds in reach — pre-fix this returned None and
        // the request sailed into the 4k window with zero events.
        let trigger = compression_trigger(3, 41_355, 39_900, 40, None, budget, 1_432)
            .expect("the ceiling must fire the trigger on the first turn");
        assert!(trigger.hard_budget, "a real token budget, not a soft halve");
        assert_eq!(
            trigger.budget,
            3_276 - 1_432,
            "budget lands in message space: ceiling minus tool-schema tokens"
        );
        assert_eq!(trigger.max_messages, None, "no count firing here");
    }

    /// Absent `num_ctx` → exactly the cached-numbers budget (no ceiling).
    /// CONTRACT CHANGED in Phase 20 (docs/design/model-self-tuning.md §2.1):
    /// the cached figure is now `max(max_ok_input, safe_context)` — the
    /// high-water mark is a floor of proven-good, not a ceiling, so it must
    /// never pull the budget BELOW the believed-safe window.
    #[test]
    fn absent_num_ctx_leaves_the_budget_unchanged() {
        assert_eq!(initial_send_budget(None, None, None), None);
        assert_eq!(initial_send_budget(Some(2_000), None, None), Some(2_000));
        assert_eq!(initial_send_budget(None, Some(5_000), None), Some(5_000));
        assert_eq!(
            initial_send_budget(Some(2_000), Some(5_000), None),
            Some(5_000),
            "an HWM below safe_context is a floor, not a cap — safe_context wins"
        );
        // And with no budget at all, the trigger stays silent regardless of size.
        assert_eq!(
            compression_trigger(3, 41_355, 39_900, 40, None, None, 1_432),
            None
        );
    }

    /// Phase 20 §2.1 — the max(proven, believed) contract, all three shapes:
    /// HWM below the claim, HWM above the claim (proven beyond it), and the
    /// post-cw-400 shape where `safe_context` was reined to the authoritative
    /// cap so `max()` still lands on the authoritative number.
    #[test]
    fn cached_budget_is_max_of_proven_and_believed() {
        // The motivating failure: max_ok_input ratcheted to 6,068 (largest
        // prompt SEEN) while safe_context believed 80% of a 32k window safe.
        // Pre-fix the 6,068 won and refused sends the backend accepted.
        assert_eq!(
            initial_send_budget(Some(6_068), Some(26_214), None),
            Some(26_214),
            "HWM below safe_context → safe_context"
        );
        // Proven beyond the claim: an accepted 8,734-token prompt outranks a
        // conservative claim-derived window.
        assert_eq!(
            initial_send_budget(Some(8_734), Some(6_553), None),
            Some(8_734),
            "HWM above safe_context (proven beyond the claim) → HWM"
        );
        // cw-400-reined shape (#223): the 400 set max_ok_input to 80% of the
        // endpoint's reported hard limit (authoritative, may be HIGH) and
        // reined safe_context down to equal-or-lower — max() must land on
        // the authoritative cap, not regress to the VRAM-capped figure.
        assert_eq!(
            initial_send_budget(Some(800_000), Some(64_000), None),
            Some(800_000),
            "post-cw-400: max_ok_input is the authoritative cap"
        );
        assert_eq!(
            initial_send_budget(Some(800_000), Some(800_000), None),
            Some(800_000)
        );
    }

    /// Phase 20 §2.3 — the calibration converters: ratio 1.0 is the identity,
    /// estimate→real rounds UP (must-err-on-counting, the 18.1 rule),
    /// real→estimate rounds DOWN (a tighter compression target is safer).
    #[test]
    fn calibration_helpers_round_in_the_safe_direction() {
        use super::{calibrate_down, calibrate_up, sanitize_estimate_ratio};
        // Identity at 1.0 — the no-calibration baseline is exact.
        assert_eq!(calibrate_up(6_068, 1.0), 6_068);
        assert_eq!(calibrate_down(6_068, 1.0), 6_068);
        // The measured nemotron3 shape: chars/4 undercounts ~30% (×1.3).
        assert_eq!(calibrate_up(1_000, 1.3), 1_300);
        assert_eq!(calibrate_down(1_000, 1.3), 769, "floor, never round up");
        // Fractional results: up ceils, down floors.
        assert_eq!(calibrate_up(3, 1.5), 5, "4.5 ceils to 5");
        assert_eq!(calibrate_down(3, 2.0), 1, "1.5 floors to 1");
        // Sanitizer: absent / NaN / out-of-clamp all degrade to identity.
        assert_eq!(sanitize_estimate_ratio(None), 1.0);
        assert_eq!(sanitize_estimate_ratio(Some(f32::NAN)), 1.0);
        assert_eq!(sanitize_estimate_ratio(Some(0.1)), 1.0);
        assert_eq!(sanitize_estimate_ratio(Some(5.0)), 1.0);
        assert_eq!(sanitize_estimate_ratio(Some(1.29)), 1.29);
        assert_eq!(sanitize_estimate_ratio(Some(0.5)), 0.5, "clamp inclusive");
        assert_eq!(sanitize_estimate_ratio(Some(3.0)), 3.0, "clamp inclusive");
    }

    /// Phase 20 §2.3 — currency composition at the trigger boundary: a
    /// real-token send budget minus calibrated-up tool tokens, fired through
    /// the trigger, then calibrated DOWN into the pipeline's chars/4 space,
    /// must equal converting each leg separately (the e2e wiring in both
    /// loops relies on this composition).
    #[test]
    fn calibration_composes_across_the_trigger_boundary() {
        use super::{calibrate_down, calibrate_up};
        let cal = 1.3_f32;
        let send_budget = 8_734_usize; // real tokens
        let tool_tokens_est = 1_000_usize; // chars/4 estimate
        let tool_tokens_real = calibrate_up(tool_tokens_est, cal); // 1,300
        let current_real = calibrate_up(9_000, cal); // estimate → real
        let trigger = compression_trigger(
            3,
            current_real,
            9_000,
            40,
            None,
            Some(send_budget),
            tool_tokens_real,
        )
        .expect("over-budget context fires the guard");
        assert!(trigger.hard_budget);
        // trigger.budget is real space (send budget minus real tool tokens);
        // the pipeline target converts it back to estimate space.
        assert_eq!(trigger.budget, send_budget - tool_tokens_real);
        let pipeline_budget = calibrate_down(trigger.budget, cal);
        assert_eq!(pipeline_budget, calibrate_down(8_734 - 1_300, cal));
        assert!(
            pipeline_budget < trigger.budget,
            "ratio > 1: the estimate-space target is tighter than the real one"
        );
    }

    /// The ceiling composes with existing budgets via `min` — whichever is
    /// tighter wins, in both directions.
    #[test]
    fn ceiling_composes_with_cached_budgets_via_min() {
        // Cached cap tighter than the ceiling: cached wins (mid-loop B5
        // behavior is untouched by #282).
        assert_eq!(
            initial_send_budget(Some(2_135), None, Some(4_096)),
            Some(2_135)
        );
        // Ceiling tighter than the cached cap: the B6 shape — bootstrap
        // safe_context 104,857 vs forced num_ctx 4,096.
        assert_eq!(
            initial_send_budget(None, Some(104_857), Some(4_096)),
            Some(3_276)
        );
        assert_eq!(
            initial_send_budget(Some(104_857), Some(104_857), Some(4_096)),
            Some(3_276)
        );
    }

    /// Zero/tiny `num_ctx` must never become a compress-to-zero budget — the
    /// zero-is-disabled contract (F3) holds at the source.
    #[test]
    fn zero_or_tiny_num_ctx_is_no_budget_at_all() {
        assert_eq!(num_ctx_input_ceiling(None), None);
        assert_eq!(num_ctx_input_ceiling(Some(0)), None);
        assert_eq!(num_ctx_input_ceiling(Some(1)), None, "80% rounds to zero");
        assert_eq!(initial_send_budget(None, None, Some(0)), None);
        // A zero ceiling must not shadow a real cached budget either.
        assert_eq!(initial_send_budget(Some(2_000), None, Some(0)), Some(2_000));
    }
}

/// Everything one agentic turn needs, resolved once by the caller (the TUI
/// resolves config + capability cache + caveats per turn and threads them in
/// here, so the loop itself never re-reads config from disk).
pub struct ChatCtx<'a> {
    pub url: &'a str,
    pub model: &'a str,
    /// Wire protocol of the active backend (Ollama vs OpenAI-compatible).
    pub kind: crate::BackendKind,
    /// Bearer token for authenticated OpenAI-compatible endpoints.
    pub api_key: Option<&'a str>,
    /// Full message list already assembled by `MemoryManager::build_messages`.
    pub messages: &'a [crate::MemMessage],
    pub task: &'a str,
    pub workspace: &'a str,
    pub color: bool,
    pub caveats: &'a crate::caveats::Caveats,
    /// Maximum tool-call rounds before forcing a final tools-disabled
    /// completion (from `[tui].max_tool_rounds`, default 25).
    pub max_tool_rounds: usize,
    /// Max lines of tool output shown inline (from `[tui].tool_output_lines`,
    /// default 20). Resolved once per turn and threaded to `execute_tool` so
    /// the tool loop never re-reads config from disk.
    pub tool_output_lines: usize,
    /// Enable per-round diagnostic output. Set via `NEWT_DEBUG=1` or the
    /// `[tui] debug = true` config key.
    pub debug: bool,
    /// Enable deeper backend compatibility traces. Set via `NEWT_TRACE=1` or
    /// the `[tui] trace = true` config key.
    pub trace: bool,
    /// Ollama `options.num_ctx` — caps KV-cache allocation to prevent VRAM
    /// exhaustion on large models. `None` → model default (often 131k).
    /// Also feeds the pre-send budget as a hard input ceiling for this turn's
    /// requests (issue #282): Ollama silently evaluates only the window's
    /// tail, so anything newt sends must already fit inside the `num_ctx` it
    /// sends it with. Ignored on the OpenAI path (no such request field).
    pub num_ctx: Option<u32>,
    /// TCP connect timeout. Short (5 s default) so a down endpoint fails fast
    /// rather than blocking the full `inference_timeout_secs`.
    pub connect_timeout_secs: u64,
    /// Total inference timeout. Must be long enough for the model to generate
    /// a complete response (120 s default).
    pub inference_timeout_secs: u64,
    /// Message list size at which the agent trims the middle of the in-flight
    /// conversation to prevent context overflow mid-turn.
    pub mid_loop_trim_threshold: usize,
    /// Estimated-token threshold that also triggers a mid-loop trim, regardless
    /// of message count. Guards against a single huge tool result blowing past
    /// the context window in one round (from `[tui].mid_loop_trim_tokens`).
    /// `None` disables token-based trimming. See issue #223.
    pub mid_loop_trim_tokens: Option<usize>,
    /// Highest input-token count this model has accepted without a 400, from
    /// `model-capabilities.json`. Used as the pre-send budget gate: requests
    /// estimated to exceed it are trimmed *before* dispatch. `None` falls back
    /// to `safe_context`. See issue #223.
    pub max_ok_input: Option<u32>,
    /// Shell command run after every successful file write to give the model
    /// immediate ground-truth feedback (e.g. "cargo check -q --workspace").
    /// `None` disables auto-checking. Set per-workspace in `.newt/config.toml`.
    pub build_check_cmd: Option<String>,
    /// Empirically derived safe context size for this model (input tokens).
    /// Used to detect likely overflow when the model returns an empty response.
    /// Sourced from `model-capabilities.json` via `ensure_context_window`.
    /// `None` disables overflow detection.
    pub safe_context: Option<u32>,
    /// Hook invoked when a dispatch fails, to recover a hard context-window
    /// 400: `(error, model, today) → new input-token cap`. The TUI wires its
    /// `recover_context_window_400` (which parses the endpoint's real limit
    /// and persists it to `model-capabilities.json` — that cache stays
    /// TUI-side with the probe module). `None` disables recovery: the error
    /// propagates exactly as it did when no limit could be parsed. See #223.
    pub recover_cw_400: Option<RecoverCw400>,
    /// Model-writable note store behind the `save_note` tool (Step 19.3,
    /// #248). `None` ⇒ the tool is not advertised and the loop never writes
    /// memory (eval / headless callers unaffected). The TUI passes a sink
    /// over its session `MemoryManager`, so `save_note` and `/remember`
    /// share one store, one security scan, one char budget.
    pub note_sink: Option<&'a mut dyn NoteSink>,
    /// Turn-counted memory-nudge state ([`NoteNudge`]), owned by the caller
    /// across user turns and lent to the loop per call. Consulted only when
    /// `note_sink` is present; `None` disables the nudge.
    pub note_nudge: Option<&'a mut NoteNudge>,
    /// Read-only search over PAST conversations behind the `recall` tool
    /// (Step 17.5, #246). `None` ⇒ the tool is not advertised and the loop
    /// never searches (eval / headless callers unaffected). The TUI passes
    /// a [`StoreRecallSource`] over its session `ConversationStore` —
    /// workspace-fenced, current conversation excluded.
    pub recall_source: Option<&'a dyn RecallSource>,
    /// Read-only pull of an ADDRESSED memory item behind the `memory_fetch`
    /// tool (progressive-disclosure memory, Workstream A MVP, #319). `None` ⇒
    /// the tool is not advertised and the loop never fetches — eval / headless
    /// / ACP callers (which pass `memory_source: None`) are unaffected,
    /// bit-for-bit. The TUI passes a [`StoreMemorySource`] over its session
    /// `NoteStore` + `ConversationStore` (workspace-fenced), so `note:` and
    /// `turn:` addresses resolve against the same surfaces `/remember` and
    /// `recall` use. Gated exactly like `recall_source`.
    pub memory_source: Option<&'a dyn MemorySource>,
    /// Compression summarizer (Step 18.4, #247): given the redacted summary
    /// request, returns the summary text — typically one tools-disabled
    /// completion against the same backend (the TUI wires this, mirroring
    /// the `Summarizing` provider's `with_summarizer` injection). `None`
    /// (eval / headless) ⇒ the compression pipeline degrades to the static
    /// "Summary generation was unavailable" marker instead of an LLM summary.
    pub summarizer: Option<&'a SummarizeFn>,
    /// Session-scoped compression anti-thrash state ([`CompressState`]),
    /// owned by the caller across user turns and lent per call (the
    /// `note_nudge` pattern). `None` ⇒ a fresh per-turn state.
    pub compress_state: Option<&'a mut CompressState>,
    /// Per-turn tool-event recorder (Step 17.6, #246): when present, the
    /// loop pushes one [`crate::ToolEvent`] per executed tool call — name,
    /// privacy-preserving args digest (never raw args), outcome, duration
    /// claim — at the same site that renders tool activity live. The TUI
    /// lends a fresh `Vec` per turn (the `note_nudge` pattern) and persists
    /// it into the turn's `events` column. `None` (eval / headless) ⇒
    /// nothing is recorded.
    pub tool_events: Option<&'a mut Vec<crate::ToolEvent>>,
    /// Prompted ocap grants (issue #263): when present, a capability denial
    /// inside `execute_tool` consults the human — allow once / session allow
    /// / deny — instead of failing outright; the loop blocks like a long
    /// tool call while the prompt is pending. `None` (the default — every
    /// headless caller: ACP worker, `newt-eval`) keeps each denial exactly
    /// as before, so nothing non-interactive can ever hang on a prompt.
    pub permission_gate: Option<&'a mut dyn PermissionGate>,
    /// Per-round capability observation hook (Phase 20,
    /// `docs/design/model-self-tuning.md` §2.2): the loop reports each
    /// round's evidence — accepted prompt sizes (with the matching chars/4
    /// estimate for calibration), persistent-empty suspected overflows, the
    /// thinking-only response quirk — at the moment of observation, so the
    /// caller can persist it even when the turn later bails or errors (the
    /// motivating failure discarded an accepted 8,734-token prompt because
    /// the only write-back lived in the TUI's `Ok`-arm epilogue). `None`
    /// (ACP worker, eval, cowork driver) preserves today's behavior exactly
    /// (spec §5).
    pub on_round_usage: Option<&'a mut dyn FnMut(RoundObservation)>,
    /// Learned observed/estimated prompt-token ratio for this model
    /// (Phase 20 §2.3), applied wherever chars/4 estimates meet
    /// backend-reported token budgets — compression triggers, targets, and
    /// the tool-schema overhead. `None` or out-of-clamp values degrade to
    /// 1.0 (no calibration; pre-Phase-20 behavior).
    pub estimate_ratio: Option<f32>,
    /// #307 named-permission-preset exec FLOOR. When a `/mode` preset is active
    /// its exec clamp is threaded here so the `--disable-ocap` / `--yolo`
    /// bypass in `execute_tool` cannot raise exec authority above the preset:
    /// an out-of-floor command falls through to the confined shell and is
    /// denied. `None` (no active preset, and every headless caller) leaves the
    /// bypass bit-for-bit. The floor is also already `meet`-ed into `caveats`,
    /// so the confined-shell and gate paths enforce it too; this field is the
    /// one extra place the otherwise caveats-blind bypass must consult.
    pub exec_floor: Option<&'a crate::caveats::Scope<String>>,
    /// `retry` technique (R2 action arm): a turn-scoped copy-on-first-write ledger
    /// ([`crate::verify_gate::WriteLedger`]) the file-write tools record into before
    /// each `write_file` / `edit_file`, so the caller can revert exactly the files
    /// newt wrote this turn (and only those) after the gate runs. Shared via
    /// [`RefCell`](std::cell::RefCell) so the loop records while the caller reads.
    /// `None` (every headless caller, and any profile without `retry`) ⇒ nothing is
    /// recorded and no file is ever reverted — bit-for-bit today's behavior.
    pub write_ledger: Option<&'a std::cell::RefCell<crate::verify_gate::WriteLedger>>,
    /// User-interrupt flag (Esc / Ctrl-C during a turn). When set mid-turn the
    /// loop abandons at its next checkpoint — the round-loop top, and the two
    /// model awaits (the non-streaming probe and the token stream) — and
    /// returns early. `None` (every headless / eval caller) ⇒ no interrupt
    /// path, bit-for-bit today's behavior. The caller owns the `AtomicBool`,
    /// trips it from a keyboard watcher, and inspects it after the call to tell
    /// an interrupted turn from a genuinely empty reply.
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
    /// The injected embedded-git capability (PR4, #461). `Some` ⇒ the `git`
    /// tool is advertised and dispatches through it (`LocalGitTool` in
    /// `newt-git`, injected by the binary). `None` (every headless / eval
    /// caller, and any session not in a git repo) ⇒ the tool is never
    /// advertised — bit-for-bit today's behavior. The trait-injection seam, not
    /// a direct dep, because `newt-git` depends on `newt-core` (circular).
    pub git_tool: Option<&'a dyn GitTool>,
    /// The injected crew/team orchestration capability (#479). `Some` ⇒ the
    /// `compose_roster` + `crew` tools are advertised and dispatch through it
    /// (`LocalCrewRunner` in `newt-cli`, injected by the `/team` toggle). `None`
    /// ⇒ never advertised. Trait-injection seam like `git_tool` (newt-scheduler
    /// depends on newt-core, so the dep can't be direct).
    pub crew_runner: Option<&'a dyn CrewRunner>,
}

/// retry technique (R2 action arm): before a `write_file`/`edit_file` is dispatched,
/// capture the target's pre-write bytes into the turn ledger. Recorded at the loop's
/// dispatch site (not inside `execute_tool`) so the seam stays narrow and only the
/// two file-writing tools are ever tracked. [`WriteLedger::note_before_write`] is
/// idempotent on the turn's first write of a path, so the *pre-turn* state is what
/// is preserved. A no-op when no ledger is lent — every headless caller and any
/// profile without `retry` — so behavior is bit-for-bit unchanged there.
fn ledger_note_write(
    write_ledger: Option<&std::cell::RefCell<crate::verify_gate::WriteLedger>>,
    name: &str,
    args: &serde_json::Value,
    workspace: &str,
) {
    let Some(led) = write_ledger else {
        return;
    };
    if name != "write_file" && name != "edit_file" {
        return;
    }
    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
        // Key on the lexically-normalized path so a raw model path like
        // `examples/../foo.py` matches the gate's filesystem-normalized `foo.py`
        // and the revert lookup actually hits — otherwise a non-normalized path
        // would silently evade revert (the fabrication persists, the gate is gamed).
        let abs = lexical_normalize(&std::path::Path::new(workspace).join(p));
        led.borrow_mut().note_before_write(abs);
    }
}

/// Lexically normalize a path — collapse `.`, resolve `..`, drop empty components —
/// **without** touching the filesystem, so a ledger key built from a raw
/// model-supplied path matches the gate's `read_dir`-normalized path. Purely
/// lexical: it deliberately does not resolve symlinks (the gate never follows them
/// and revert is workspace-boundary-guarded), so it cannot itself escape the tree.
fn lexical_normalize(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                // Only pop a real path segment; never climb above a root/prefix.
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod retry_ledger_tests {
    use super::*;

    #[test]
    fn lexical_normalize_collapses_dot_and_parent() {
        assert_eq!(
            lexical_normalize(std::path::Path::new("/ws/examples/../foo.py")),
            std::path::PathBuf::from("/ws/foo.py")
        );
        assert_eq!(
            lexical_normalize(std::path::Path::new("/ws/./a//b/foo.py")),
            std::path::PathBuf::from("/ws/a/b/foo.py")
        );
        // A leading `..` with no segment to pop is preserved, not climbed past root.
        assert_eq!(
            lexical_normalize(std::path::Path::new("../x.py")),
            std::path::PathBuf::from("../x.py")
        );
    }

    #[test]
    fn ledger_note_write_keys_on_the_normalized_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("foo.py"), "real\n").unwrap();
        let led = std::cell::RefCell::new(crate::verify_gate::WriteLedger::new());
        // a raw, non-normalized model path
        let args = serde_json::json!({ "path": "examples/../foo.py" });
        ledger_note_write(
            Some(&led),
            "write_file",
            &args,
            tmp.path().to_str().unwrap(),
        );
        // the key normalized to <ws>/foo.py — the same path the gate would produce —
        // so revert finds and restores it (returns true).
        assert!(
            led.borrow().revert(&tmp.path().join("foo.py")).unwrap(),
            "the normalized key matches the gate's path"
        );
        // a read-only tool is never recorded
        ledger_note_write(
            Some(&led),
            "read_file",
            &serde_json::json!({ "path": "foo.py" }),
            tmp.path().to_str().unwrap(),
        );
        assert_eq!(led.borrow().len(), 1, "only write tools are tracked");
    }
}

/// True when a backend 400 says the model can't accept a `tools` field.
/// Ollama phrases it `"<model> does not support tools"`; OpenAI-compatible
/// servers vary, so we also accept the looser `"not support tools"`. Used to
/// drop tools and retry once, then keep them off for the session (deepseek-r1).
fn is_tools_unsupported_error(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("does not support tools") || s.contains("not support tools")
}

/// Main agentic loop: call model → execute tool calls → feed results back → repeat.
/// Returns `(reply_text, was_streamed, token_usage, hallucination_count)`.
/// When `was_streamed` is true the text was already printed token-by-token.
///
/// Token-usage semantics (Step 18.1): `input_tokens` is the **largest single
/// prompt** the backend evaluated across the turn's rounds — the truthful
/// "how full did the context get" figure that feeds the capability ratchet —
/// NOT the per-round sum, which double-counts history (every round's prompt
/// re-includes all prior rounds; the B3 baseline measured 5.4× inflation).
/// `output_tokens` is the sum across rounds (each completion is new).
/// `true` once the caller's interrupt flag is set (Esc / Ctrl-C). Cheap relaxed
/// load — the flag is a one-way latch, so no ordering guarantees are needed.
fn is_cancelled(cancel: Option<&std::sync::atomic::AtomicBool>) -> bool {
    cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
}

/// Resolve as soon as the interrupt flag is set, polling at ~50 ms — fine for a
/// human keypress and cheap enough to lose the race to any real I/O future.
/// Never resolves when `cancel` is `None`, so a `select!` against it collapses
/// to just the other arm (no behavior change for headless callers).
async fn cancelled(cancel: Option<&std::sync::atomic::AtomicBool>) {
    match cancel {
        None => std::future::pending().await,
        Some(flag) => {
            // Poll briskly so an interrupt is felt promptly (well under the
            // ~100 ms human threshold) — the cost is negligible next to a model
            // round-trip.
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
        }
    }
}

/// Race a model future against the interrupt flag: `Some(v)` if it finished,
/// `None` if the user interrupted first (the future is dropped, cancelling any
/// in-flight request).
async fn cancellable<F: std::future::Future>(
    cancel: Option<&std::sync::atomic::AtomicBool>,
    fut: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        _ = cancelled(cancel) => None,
        v = fut => Some(v),
    }
}

/// Drive `fut` while animating the thinking line in place (~8 fps), so a slow
/// non-streaming probe still feels alive — the braille/hourglass spinner
/// otherwise only advances when reasoning tokens arrive. Clears the line when
/// `fut` resolves so the following output starts clean. A no-op when `animate`
/// is false (piped / `thinking = "off"` / no TTY) — bit-for-bit today's path.
async fn with_thinking_spinner<F: std::future::Future>(
    animate: bool,
    label: &str,
    fut: F,
) -> F::Output {
    if !animate {
        return fut.await;
    }
    tokio::pin!(fut);
    let start = std::time::Instant::now();
    let mut frame = 0usize;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(120));
    loop {
        tokio::select! {
            biased;
            out = &mut fut => {
                let _ = write!(io::stdout(), "\r\x1b[K");
                let _ = io::stdout().flush();
                return out;
            }
            _ = ticker.tick() => {
                let line = format_spinner(frame, start.elapsed().as_secs_f32(), label, 0);
                let mut out = io::stdout();
                let _ = execute!(
                    out,
                    Print("\r\x1b[K"),
                    SetForegroundColor(CtColor::DarkGrey),
                    Print(&line),
                    ResetColor,
                );
                let _ = out.flush();
                frame = frame.wrapping_add(1);
            }
        }
    }
}

pub async fn chat_complete(
    ctx: ChatCtx<'_>,
    mcp: &mut dyn McpTools,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)> {
    // OpenAI-compatible endpoints speak a different wire format (request,
    // tool_calls, and usage shapes all differ), so they get their own loop.
    if ctx.kind == crate::BackendKind::Openai {
        return openai_chat_complete(ctx, mcp).await;
    }
    let ChatCtx {
        url,
        model,
        kind: _,
        api_key: _,
        messages: mem_messages,
        task,
        workspace,
        color,
        caveats,
        max_tool_rounds,
        tool_output_lines,
        debug,
        trace,
        num_ctx,
        connect_timeout_secs,
        inference_timeout_secs,
        mid_loop_trim_threshold,
        mid_loop_trim_tokens,
        max_ok_input,
        build_check_cmd,
        safe_context,
        recover_cw_400,
        mut note_sink,
        mut note_nudge,
        recall_source,
        memory_source,
        summarizer,
        compress_state,
        mut tool_events,
        mut permission_gate,
        mut on_round_usage,
        estimate_ratio,
        exec_floor,
        write_ledger,
        cancel,
        git_tool,
        crew_runner,
    } = ctx;
    // Headless callers may pass no session state — compression still works,
    // with per-turn anti-thrash accounting.
    let mut local_compress_state = CompressState::new();
    let compress_state = match compress_state {
        Some(s) => s,
        None => &mut local_compress_state,
    };
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .timeout(std::time::Duration::from_secs(inference_timeout_secs))
        .build()?;
    let chat_url = format!("{}/api/chat", url.trim_end_matches('/'));
    let retry = tui_retry_policy();
    // The save_note tool is advertised only when a sink exists (Step 19.3);
    // recall only when a source exists (Step 17.5); memory_fetch only when a
    // memory source exists (#319) — same presence gating.
    let advertise_save_note = note_sink.is_some();
    let advertise_recall = recall_source.is_some();
    let advertise_memory_fetch = memory_source.is_some();
    let advertise_git = git_tool.is_some();
    let advertise_team = crew_runner.is_some();

    // Convert MemMessage list to Ollama JSON format.
    // The memory manager already included the current task as the last user message.
    let mut messages: Vec<serde_json::Value> = mem_messages
        .iter()
        .map(|m| serde_json::json!({"role": m.role.as_str(), "content": m.content}))
        .collect();

    // In-band memory nudge (Step 19.3): after `[memory] note_nudge_interval`
    // user turns with zero organic save_note use, append a one-line reminder
    // to this turn's user message. Only when a sink exists — without one the
    // save_note tool isn't even advertised.
    if note_sink.is_some() {
        if let Some(line) = note_nudge.as_deref_mut().and_then(NoteNudge::begin_turn) {
            append_nudge_line(&mut messages, &line);
        }
    }

    let mut accumulated_usage: Option<crate::TokenUsage> = None;
    let mut hallucination_count: u32 = 0;
    let mut overflow_retries: u32 = 0;
    let mut suspicious_empty_retries: u32 = 0;
    // Hard context-window 400s recovered (parse limit → trim → retry). See #223.
    let mut cw_retries: u32 = 0;
    // Some models reject ANY request carrying a `tools` field (e.g.
    // deepseek-r1). Once one 400s with "does not support tools", drop tools for
    // the rest of the session so even a bare "hello" works; notice it once.
    let mut tools_supported = true;
    let mut tools_unsupported_notified = false;
    // Pre-send token budget gate: trim before dispatch when the current context
    // size exceeds the model's empirically-confirmed max input (or the safe
    // context) — capped by the `num_ctx` every request this turn will carry
    // (#282: on a fresh capability cache the cached numbers are unset or huge,
    // so without the ceiling the first turn dispatched 10× over the real
    // window with zero events — B6). Mutable because a recovered 400 tightens
    // it mid-turn. See #223.
    let num_ctx_ceiling = num_ctx_input_ceiling(num_ctx);
    let mut send_budget: Option<usize> = initial_send_budget(max_ok_input, safe_context, num_ctx);
    // Step 20.3: is the send budget backed by an authoritative ceiling, or
    // does it rest on the proven-good high-water mark (`max_ok_input`) alone?
    // `safe_context` (a believed/declared window) and the per-request
    // `num_ctx` ceiling are authoritative; a cw-400 recovery flips this true
    // mid-turn. Cloud endpoints with no `/api/show` seed neither, so their
    // guard is non-authoritative and fails open instead of refusing.
    let mut send_budget_authoritative = safe_context.is_some() || num_ctx_ceiling.is_some();
    // Tool schemas ride along in every request body; count them once (18.1).
    // Stable for the whole turn: the builtin + MCP tool set doesn't change
    // mid-turn, so hoisting out of the round loop is safe.
    let tools = merged_tool_definitions(
        mcp,
        advertise_save_note,
        advertise_recall,
        advertise_memory_fetch,
        advertise_git,
        advertise_team,
    );
    let tool_tokens = estimate_value_tokens(&tools);
    // Phase 20 §2.3: one sanitized calibration ratio per turn. The
    // tool-schema overhead converts to real-token space once — the schema
    // set is stable for the whole turn, and the send budget it is subtracted
    // from is real-token currency.
    let cal = sanitize_estimate_ratio(estimate_ratio);
    let tool_tokens_real = calibrate_up(tool_tokens, cal);
    // Animate the in-place "thinking…/compressing…" status line only on a TTY
    // with streaming enabled (never in a pipe / `newt worker`). Constant for the
    // turn — both the compression and probe waits reuse it.
    let animate = color && thinking_stream_enabled();
    // Truthful context-size tracker: anchors on the backend's last-reported
    // prompt token count, chars/4 + schema estimate as fallback (Step 18.1).
    let mut prompt_tracker = PromptTracker::new();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    // Consecutive rounds where the model only called read-only tools (no writes).
    // When this hits READ_ONLY_NUDGE_AFTER, a brief injected message tells the
    // model to stop exploring and start writing.
    let mut read_only_rounds: usize = 0;
    // Phase 20 §2.2: the thinking-only quirk is reported at most once per
    // turn — re-detection adds no information and would thrash the cache.
    let mut thinking_only_reported = false;

    // Agentic loop — up to `max_tool_rounds` tool-call rounds.
    'round_loop: for round in 0..max_tool_rounds {
        // Interrupt checkpoint (Esc / Ctrl-C): bail before spending another
        // round on the model or a tool. The reply is empty — the caller sees
        // `cancel` set and treats the turn as abandoned regardless.
        if is_cancelled(cancel) {
            return Ok((String::new(), false, accumulated_usage, hallucination_count));
        }
        if round > 0 {
            // Brief separator between rounds so user can follow the flow.
            if color {
                execute!(
                    io::stdout(),
                    SetForegroundColor(CtColor::DarkGrey),
                    Print("…\n"),
                    ResetColor
                )
                .ok();
            }
        }

        // Read-only round nudge: if the model has spent several consecutive
        // rounds only reading (list_dir / read_file / web_fetch / search /
        // use_skill) without writing anything, inject a brief reminder to
        // stop exploring and call edit_file or write_file.  This breaks the
        // "endless exploration → empty response" failure mode seen with some
        // local models (e.g. nemotron3:33b).
        const READ_ONLY_NUDGE_AFTER: usize = 3;
        if read_only_rounds >= READ_ONLY_NUDGE_AFTER {
            let remaining = max_tool_rounds.saturating_sub(round + 1);
            messages.push(serde_json::json!({
                "role": "user",
                "content": format!(
                    "[{read_only_rounds} read-only rounds so far. Stop AIMLESS exploring and \
                     start making the change. This is a nudge, not a limit — you may still \
                     read. In particular, before an edit_file, read the ONE file you are about \
                     to change so your old_string matches its exact text; never guess old_string \
                     or repeat a failed edit. ~{remaining} round(s) left.]"
                )
            }));
            read_only_rounds = 0;
        }

        // Context compression (Step 18.4, #247): one shared pipeline —
        // structural prune → boundary → redacted LLM summary → marker
        // assembly — serves both the mid-loop trigger (message count OR
        // current tokens: the VRAM guard and issue #223's token guard) and
        // the pre-send budget guard (`max_ok_input`/`safe_context`). The
        // current-token figure is prompt-tokens-preferred (Step 18.1). The
        // old amputation trim survives only as the pipeline's no-summarizer
        // static-marker path.
        {
            // Phase 20 §2.3: `current` is calibrated into real-token space —
            // the same currency as the (backend-derived) send budget and the
            // configured token threshold it is compared against.
            let current = prompt_tracker.current(&messages, Some(&tools), cal);
            // The count-only budget is priced in message-token space — the
            // same chars/4 currency the pipeline compares its budget against
            // (F1); `current` (schema/template-inclusive) still drives the
            // token triggers.
            let message_tokens = estimate_tokens(&messages);
            if let Some(trigger) = compression_trigger(
                messages.len(),
                current,
                message_tokens,
                mid_loop_trim_threshold,
                mid_loop_trim_tokens,
                send_budget,
                tool_tokens_real,
            ) {
                // A hard trigger's budget is real-token currency; the
                // pipeline measures and reclaims in chars/4 — convert once
                // (Phase 20 §2.3). Count-only budgets are already priced in
                // message-token space (F1) and pass through unconverted.
                let pipeline_budget = if trigger.hard_budget {
                    calibrate_down(trigger.budget, cal)
                } else {
                    trigger.budget
                };
                // Step 20.3: does this budget rest on an authoritative ceiling
                // or the lone proven-good HWM? A fired token threshold is
                // user-authoritative; otherwise the guard fired, authoritative
                // only when the send budget is backed by a believed window.
                let token_fired = mid_loop_trim_tokens.is_some_and(|t| t > 0 && current > t);
                // Compression makes its own summarizer model call — animate the
                // line with a "compressing context…" stage so it doesn't sit
                // frozen, and race it against the interrupt flag so Esc bails
                // out of a slow summarize instead of waiting for it to finish.
                let outcome = match with_thinking_spinner(
                    animate,
                    "compressing context…",
                    cancellable(
                        cancel,
                        compress(
                            CompressRequest {
                                messages: &messages,
                                budget: pipeline_budget,
                                max_messages: trigger.max_messages,
                                task,
                                hard_budget: trigger.hard_budget,
                                authoritative: token_fired || send_budget_authoritative,
                                focus: None,
                            },
                            summarizer,
                            compress_state,
                        ),
                    ),
                )
                .await
                {
                    Some(o) => o,
                    None => {
                        return Ok((String::new(), false, accumulated_usage, hallucination_count))
                    }
                };
                if let Some(notice) = outcome.notice {
                    print_harness_notice(&notice, color);
                }
                if outcome.action == CompressAction::Refused {
                    // Anti-thrash disabled compression and the context still
                    // exceeds the budget: refuse the send rather than let the
                    // backend silently truncate the task away (baseline B6).
                    // Phase 20: name the model and the reset escape hatch —
                    // a poisoned learned budget is a known cause of this bail.
                    anyhow::bail!(
                        "context (~{current} tokens) exceeds the model's input budget and \
                         auto-compression is disabled after repeated ineffective passes — \
                         start a new conversation or ask a more focused question, or run \
                         `newt tunings reset {model}` if this model's learned budget looks wrong"
                    );
                }
                if outcome.fired {
                    // N2: a hard-budget compression whose assembled result is
                    // still over budget says so — the dispatch proceeds (the
                    // cw-400/overflow recovery bounds it), but visibly. The
                    // comparison is in the SAME (chars/4) currency the
                    // pipeline measured `tokens_after` in (Phase 20 §2.3).
                    let mut how = outcome.action.describe().to_string();
                    if trigger.hard_budget && outcome.tokens_after > pipeline_budget {
                        how.push_str(", still over budget");
                    }
                    emit_compression_notice(
                        color,
                        outcome.tokens_before,
                        outcome.tokens_after,
                        &how,
                    );
                    if debug {
                        print_debug(
                            &format!(
                                "compression: {} → {} messages (budget ~{} tokens, \
                                 +~{tool_tokens} tool-schema tokens ride along)",
                                messages.len(),
                                outcome.messages.len(),
                                pipeline_budget,
                            ),
                            color,
                        );
                    }
                    messages = outcome.messages;
                    prompt_tracker.invalidate();
                }
            }
        }

        // Phase 20 §2.2: chars/4 estimate of EXACTLY the request about to be
        // dispatched (the message list as sent, plus tool schemas) — paired
        // with the backend's reported prompt size in the `Accepted`
        // observation so the caller can learn the calibration ratio.
        let round_est_raw = estimate_request_tokens(&messages, Some(&tools));

        // Tool-call rounds: stream:false (fast, just JSON).
        // Final text round: stream:true so the user sees tokens arrive.
        // We don't know which round is last, so we probe with stream:false first
        // and switch to streaming only when the model returns no tool calls.
        let mut body_no_stream = if let Some(ctx_size) = num_ctx {
            serde_json::json!({
                "model": model,
                "messages": messages,
                "stream": false,
                "tools": tools.clone(),
                "options": { "num_ctx": ctx_size },
            })
        } else {
            serde_json::json!({
                "model": model,
                "messages": messages,
                "stream": false,
                "tools": tools.clone(),
            })
        };
        // Drop tools entirely for a model that rejects them (set below on a
        // "does not support tools" 400) — an empty array still trips strict
        // models, so remove the key.
        if !tools_supported {
            if let Some(o) = body_no_stream.as_object_mut() {
                o.remove("tools");
            }
        }

        // Retry the send+status+parse as one unit — a connection drop at any
        // of these steps is transient and worth retrying with backoff. Raced
        // against the interrupt flag so Esc bails out of a slow / stuck probe
        // (the common "the model isn't answering" case) without waiting for it.
        // Wrapped in the thinking animation so this otherwise-silent wait shows
        // a live hourglass + clock instead of a frozen line.
        let dispatch = match with_thinking_spinner(
            animate,
            "thinking…",
            cancellable(
                cancel,
                with_backoff_notify(
                    &retry,
                    || async {
                        let resp = client
                            .post(&chat_url)
                            .json(&body_no_stream)
                            .send()
                            .await
                            .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
                        if !resp.status().is_success() {
                            let status = resp.status();
                            let text = resp.text().await.unwrap_or_default();
                            anyhow::bail!("Ollama {status}: {text}");
                        }
                        resp.json::<serde_json::Value>()
                            .await
                            .map_err(anyhow::Error::from)
                    },
                    |attempt, delay| print_retry_indicator(attempt, delay, color),
                ),
            ),
        )
        .await
        {
            Some(d) => d,
            // Interrupted mid-probe: abandon the turn.
            None => return Ok((String::new(), false, accumulated_usage, hallucination_count)),
        };
        let json: serde_json::Value = match dispatch {
            Ok(j) => j,
            Err(e) => {
                // No-tools recovery: a model that rejects the `tools` field
                // (deepseek-r1) 400s even on "hello". Drop tools, notice once,
                // and re-dispatch the same round — self-limiting (the rebuilt
                // body omits tools) and session-persistent.
                if tools_supported && is_tools_unsupported_error(&e) {
                    tools_supported = false;
                    if !tools_unsupported_notified {
                        tools_unsupported_notified = true;
                        print_newt(
                            &format!(
                                "{model} does not support tools — tools disabled for this session"
                            ),
                            color,
                            false,
                        );
                    }
                    continue 'round_loop;
                }
                // Graceful context-window 400 recovery: parse the model's real
                // limit, tighten the budget, compress, and retry once (#223;
                // compress-not-trim since Step 18.4).
                if cw_retries < 2 {
                    if let Some(new_cap) = recover_cw_400.and_then(|f| f(&e, model, &today)) {
                        emit_overflow_notice(
                            color,
                            accumulated_usage.as_ref(),
                            Some(new_cap),
                            model,
                            cw_retries + 1,
                        );
                        // A recovered cap can only tighten — the request still
                        // carries the same `num_ctx`, so its ceiling holds (#282).
                        let new_budget =
                            num_ctx_ceiling.map_or(new_cap as usize, |c| (new_cap as usize).min(c));
                        send_budget = Some(new_budget);
                        // The endpoint's parsed hard limit is authoritative —
                        // a refuse on it is correct from here on (Step 20.3).
                        send_budget_authoritative = true;
                        let outcome = compress(
                            CompressRequest {
                                // Real-token budget minus real-token schema
                                // overhead, converted into the pipeline's
                                // chars/4 currency (Phase 20 §2.3).
                                messages: &messages,
                                budget: calibrate_down(
                                    new_budget.saturating_sub(tool_tokens_real),
                                    cal,
                                ),
                                max_messages: None,
                                task,
                                hard_budget: true,
                                authoritative: true,
                                focus: None,
                            },
                            summarizer,
                            compress_state,
                        )
                        .await;
                        if let Some(notice) = outcome.notice {
                            print_harness_notice(&notice, color);
                        }
                        if outcome.action == CompressAction::Refused {
                            // Refuse the resend; surface the endpoint's 400.
                            return Err(e);
                        }
                        if outcome.fired {
                            messages = outcome.messages;
                            prompt_tracker.invalidate();
                        }
                        cw_retries += 1;
                        continue 'round_loop;
                    }
                }
                return Err(e);
            }
        };

        // Merge token usage from this non-streaming probe round (input = max
        // single prompt, output = sum — Step 18.1) and anchor the context-size
        // tracker on the backend-reported prompt size of this dispatch.
        let round_usage = ollama_usage(&json);
        if let Some(u) = round_usage {
            prompt_tracker.record(u.input_tokens, messages.len());
        }
        accumulated_usage = merge_round_usage(accumulated_usage, round_usage);

        // Phase 20 §2.2: a prompt within 5% of the request's `num_ctx` may
        // have been silently head-truncated by Ollama — such a round is
        // window evidence of NOTHING and must neither raise the budget nor
        // emit an `Accepted` observation.
        let truncation_suspect = round_usage
            .is_some_and(|u| num_ctx.is_some_and(|c| u.input_tokens >= c.saturating_mul(95) / 100));
        // Mid-turn budget raise on window evidence alone: the backend just
        // evaluated this many prompt tokens inside the `num_ctx` it was sent,
        // so one over-budget acceptance stops the compress-every-round thrash
        // within the same turn (Phase 20 §2.2). Never lowers; stays under the
        // per-request input ceiling.
        if let (Some(u), Some(budget), false) = (round_usage, send_budget, truncation_suspect) {
            let raised = (u.input_tokens as usize).min(num_ctx_ceiling.unwrap_or(usize::MAX));
            if raised > budget {
                send_budget = Some(raised);
                if debug {
                    print_debug(
                        &format!(
                            "send budget raised to ~{raised} tokens (backend accepted \
                             {}-token prompt)",
                            u.input_tokens
                        ),
                        color,
                    );
                }
            }
        }

        let message = &json["message"];
        // Capture the probe content now — it may be our only copy of the
        // model's reply if the subsequent streaming re-issue returns empty.
        let probe_content = message["content"].as_str().unwrap_or("").to_string();

        let tool_calls = message["tool_calls"].as_array();
        let has_tools = tool_calls.map(|tc| !tc.is_empty()).unwrap_or(false);

        if debug {
            let content_excerpt = if probe_content.is_empty() {
                "(empty)".to_string()
            } else {
                let chars: String = probe_content.chars().take(80).collect();
                if probe_content.len() > 80 {
                    format!("{chars}…")
                } else {
                    chars
                }
            };
            let tc_count = tool_calls.map(|tc| tc.len()).unwrap_or(0);
            let usage_str = match round_usage {
                Some(u) => format!("{} in / {} out", u.input_tokens, u.output_tokens),
                None => "no usage".into(),
            };
            print_debug(
                &format!(
                    "round {round} probe: tool_calls={tc_count} usage=[{usage_str}] content={content_excerpt:?}"
                ),
                color,
            );
        }

        if !has_tools {
            // No tool calls — re-issue with stream:true so the user sees tokens.
            // `messages` already contains the task; just replay with streaming.
            //
            // IMPORTANT: the probe round already generated the model's answer in
            // `probe_content`. The streaming re-issue is a *second* inference call
            // from the same history; if it returns empty (non-determinism, context
            // pressure, or model quirk) we fall back to the probe content so the
            // user never sees a silent blank response.
            let mut body_stream = if let Some(ctx_size) = num_ctx {
                serde_json::json!({
                    "model": model,
                    "messages": &messages,
                    "stream": true,
                    "tools": tools.clone(),
                    "options": { "num_ctx": ctx_size },
                })
            } else {
                serde_json::json!({
                    "model": model,
                    "messages": &messages,
                    "stream": true,
                    "tools": tools.clone(),
                })
            };
            // A no-tools model (set on a prior "does not support tools" 400)
            // must not see the key on the streaming round either.
            if !tools_supported {
                if let Some(o) = body_stream.as_object_mut() {
                    o.remove("tools");
                }
            }
            // Retry the connection; if we connect successfully but the stream
            // drops mid-token, that's a separate (harder) failure mode. Raced
            // against the interrupt flag like the probe above.
            let sresp = match cancellable(
                cancel,
                with_backoff_notify(
                    &retry,
                    || async {
                        client
                            .post(&chat_url)
                            .json(&body_stream)
                            .send()
                            .await
                            .map_err(|e| anyhow::anyhow!("stream request failed: {e}"))
                    },
                    |attempt, delay| print_retry_indicator(attempt, delay, color),
                ),
            )
            .await
            {
                Some(r) => r?,
                None => return Ok((String::new(), false, accumulated_usage, hallucination_count)),
            };

            if !sresp.status().is_success() {
                if debug {
                    print_debug("stream request non-2xx — using probe content", color);
                }
                // Phase 20 §2.2: the probe round produced usable content —
                // quality gate met, report it before returning.
                if !probe_content.is_empty() {
                    emit_accepted(
                        &mut on_round_usage,
                        round_usage,
                        truncation_suspect,
                        round_est_raw,
                    );
                }
                return Ok((probe_content, false, accumulated_usage, hallucination_count));
            }
            // Cargo-style reasoning spinner: TTY-gated (`color`) and opt-out via
            // `[tui] thinking = "off"`. Never in a pipe / `newt worker`.
            let show_thinking = color && thinking_stream_enabled();
            let (streamed, stream_usage) =
                stream_response(sresp, color, show_thinking, cancel).await?;

            if streamed.is_empty() {
                // The streaming re-issue produced no tokens. Fall back to the
                // probe content rather than returning silence.
                if debug {
                    print_debug(
                        &format!(
                            "stream returned empty — falling back to probe content ({} chars)",
                            probe_content.len()
                        ),
                        color,
                    );
                }
                if probe_content.is_empty() {
                    let merged = merge_round_usage(accumulated_usage, stream_usage);
                    let empty_round_usage = merge_round_usage(round_usage, stream_usage);
                    let generated_unusable_output = empty_round_usage
                        .as_ref()
                        .map(|u| u.output_tokens > 0)
                        .unwrap_or(false);

                    if generated_unusable_output && suspicious_empty_retries < 1 {
                        if trace {
                            print_trace(&ollama_response_shape(&json), color);
                        }
                        if debug {
                            let fields = ollama_non_content_fields(&json);
                            let field_note = if fields.is_empty() {
                                "no known non-content fields".to_string()
                            } else {
                                format!("non-content fields: {}", fields.join(", "))
                            };
                            print_debug(
                                &format!(
                                    "empty assistant content with generated tokens — retrying once ({field_note})"
                                ),
                                color,
                            );
                        }
                        // Phase 20 §2.2: empty content carrying non-content
                        // fields is the thinking-only quirk — report it at
                        // detection (at most once per turn) so the prompt-
                        // inflating corrective retry isn't re-learned from
                        // scratch every session.
                        if !thinking_only_reported && !ollama_non_content_fields(&json).is_empty() {
                            thinking_only_reported = true;
                            if let Some(hook) = on_round_usage.as_deref_mut() {
                                hook(RoundObservation::ThinkingOnly);
                            }
                        }
                        messages.push(serde_json::json!({
                            "role": "user",
                            "content": "Your previous response produced generated tokens but no assistant-visible content and no tool call. Reply with either a tool call or final assistant content."
                        }));
                        accumulated_usage = merged;
                        suspicious_empty_retries += 1;
                        continue 'round_loop;
                    }

                    // Both probe and stream are empty — likely context overflow.
                    // `input_tokens` is the largest single prompt evaluated this
                    // turn (Step 18.1), so the 85%-of-safe-context check now
                    // compares one real prompt against the window instead of a
                    // multi-round sum that inflated past it after ~2 rounds.
                    let overflow_likely = merged
                        .as_ref()
                        .zip(safe_context)
                        .map(|(u, safe)| u.input_tokens >= safe * 85 / 100)
                        .unwrap_or(false);
                    if overflow_likely && overflow_retries < 2 {
                        emit_overflow_notice(
                            color,
                            merged.as_ref(),
                            safe_context,
                            model,
                            overflow_retries + 1,
                        );
                        // Compress toward 3/4 of the safe window — comfortably
                        // under the 85% trigger (was a blunt count trim before
                        // Step 18.4). The retry happens regardless: it is
                        // already bounded by `overflow_retries`. The target
                        // arithmetic stays in real-token space (`safe_context`
                        // and the schema overhead are real-token figures),
                        // then converts once into the pipeline's chars/4
                        // currency (Phase 20 §2.3).
                        let target = calibrate_down(
                            safe_context
                                .map(|s| (s as usize).saturating_mul(3) / 4)
                                .unwrap_or(0)
                                .saturating_sub(tool_tokens_real),
                            cal,
                        );
                        let outcome = compress(
                            CompressRequest {
                                messages: &messages,
                                budget: target,
                                max_messages: None,
                                task,
                                hard_budget: true,
                                // A suspected silent overflow is a real failure
                                // signal — refuse semantics apply (Step 20.3).
                                authoritative: true,
                                focus: None,
                            },
                            summarizer,
                            compress_state,
                        )
                        .await;
                        if let Some(notice) = outcome.notice {
                            print_harness_notice(&notice, color);
                        }
                        if outcome.fired {
                            messages = outcome.messages;
                            prompt_tracker.invalidate();
                        } else {
                            // N1: the retry must differ from the request that
                            // just returned empty — when compress was a no-op
                            // (Fit / nothing reclaimable), fall back to one
                            // structural prune with a tight protected tail.
                            let fallback = crate::prune::prune(
                                &messages,
                                &crate::prune::PruneConfig {
                                    keep_last: 2,
                                    ..Default::default()
                                },
                            );
                            if fallback.chars_reclaimed > 0 {
                                messages = fallback.messages;
                                prompt_tracker.invalidate();
                            }
                        }
                        accumulated_usage = merged;
                        overflow_retries += 1;
                        continue 'round_loop;
                    }
                    // Phase 20 §2.2: persistent empties past the retry budget
                    // at ≥85% of the safe window are silent-overflow evidence
                    // — reported at the exit, with the merged prompt figure,
                    // before either return below.
                    if overflow_likely {
                        if let (Some(hook), Some(u)) =
                            (on_round_usage.as_deref_mut(), merged.as_ref())
                        {
                            hook(RoundObservation::SuspectedOverflow {
                                prompt_tokens: u.input_tokens,
                            });
                        }
                    }
                    if generated_unusable_output {
                        if trace {
                            print_trace(&ollama_response_shape(&json), color);
                        }
                        // Phase 20 §2.2: the diagnostic exit is also a
                        // thinking-only detection site (at most once per
                        // turn; the function returns right after, so the
                        // turn-local flag needs no update here).
                        if !thinking_only_reported && !ollama_non_content_fields(&json).is_empty() {
                            if let Some(hook) = on_round_usage.as_deref_mut() {
                                hook(RoundObservation::ThinkingOnly);
                            }
                        }
                        return Ok((
                            suspicious_empty_ollama_diagnostic(&json),
                            false,
                            merged,
                            hallucination_count,
                        ));
                    }
                    let msg = "(model returned an empty response — try rephrasing, or check the model with `newt doctor`)";
                    return Ok((msg.to_string(), false, merged, hallucination_count));
                }
                // Use probe content; print it since it was never streamed.
                // Phase 20 §2.2: non-empty probe content is usable output.
                emit_accepted(
                    &mut on_round_usage,
                    round_usage,
                    truncation_suspect,
                    round_est_raw,
                );
                return Ok((
                    probe_content,
                    false,
                    merge_round_usage(accumulated_usage, stream_usage),
                    hallucination_count,
                ));
            }

            // Phase 20 §2.2: a non-empty streamed answer is usable output.
            emit_accepted(
                &mut on_round_usage,
                round_usage,
                truncation_suspect,
                round_est_raw,
            );
            return Ok((
                streamed,
                true,
                merge_round_usage(accumulated_usage, stream_usage),
                hallucination_count,
            ));
        }

        // Has tool calls — add assistant turn and execute them.
        // Phase 20 §2.2: tool calls are usable output — the dispatched prompt
        // is proven accepted regardless of how the turn later ends.
        emit_accepted(
            &mut on_round_usage,
            round_usage,
            truncation_suspect,
            round_est_raw,
        );
        messages.push(message.clone());
        let mut round_wrote = false;
        for tc in tool_calls.unwrap() {
            let anthropic_native = tc["function"].is_null();
            let name = if anthropic_native {
                tc["name"].as_str().unwrap_or("unknown")
            } else {
                tc["function"]["name"].as_str().unwrap_or("unknown")
            };
            let args = if anthropic_native {
                tc["input"].clone()
            } else {
                match &tc["function"]["arguments"] {
                    serde_json::Value::String(s) => {
                        serde_json::from_str(s).unwrap_or(serde_json::Value::Null)
                    }
                    v => v.clone(),
                }
            };
            if is_hallucination(name, &args) {
                hallucination_count += 1;
            }
            if !is_read_only_tool(name) {
                round_wrote = true;
            }
            // Organic save_note use resets the memory-nudge counter (the
            // read-only-rounds reset pattern) — active curators never see it.
            if name == "save_note" && note_sink.is_some() {
                if let Some(n) = note_nudge.as_deref_mut() {
                    n.note_saved();
                }
            }
            // retry technique: snapshot the file's pre-write bytes before the
            // write tool runs, so the post-turn gate can revert exactly newt's writes.
            ledger_note_write(write_ledger, name, &args, workspace);
            let tool_t0 = std::time::Instant::now();
            let result = execute_tool(
                name,
                &args,
                workspace,
                color,
                tool_output_lines,
                caveats,
                mcp,
                build_check_cmd.as_deref(),
                // Reborrow + re-coerce: shortens the trait-object lifetime to
                // this call (Option<&mut dyn _> is invariant, so the longer
                // ChatCtx lifetime can't unify directly).
                note_sink
                    .as_deref_mut()
                    .map(|s| &mut *s as &mut dyn NoteSink),
                recall_source,
                memory_source,
                // #263 prompted grants — same reborrow pattern as note_sink.
                permission_gate
                    .as_deref_mut()
                    .map(|g| &mut *g as &mut dyn PermissionGate),
                // #307: the active preset's exec floor (the bypass ceiling).
                exec_floor,
                // PR4: the injected embedded-git capability (None for headless).
                git_tool,
                // #479: the injected crew/team orchestration (None for headless).
                crew_runner,
            )
            .await;
            // 17.6: record the call for the turn's events column — args are
            // digested (never stored raw), duration is a display claim.
            if let Some(rec) = tool_events.as_deref_mut() {
                rec.push(crate::ToolEvent::from_call(
                    name,
                    &args,
                    tools::tool_result_ok(&result),
                    u64::try_from(tool_t0.elapsed().as_millis()).ok(),
                ));
            }
            messages.push(serde_json::json!({
                "role": "tool",
                "content": result
            }));
        }
        if round_wrote {
            read_only_rounds = 0;
        } else {
            read_only_rounds = read_only_rounds.saturating_add(1);
        }
    }

    // Reached the round cap. Trim the bloated message list so the final
    // summary request doesn't overflow the model's context window, then
    // make ONE tools-disabled completion so the user gets a real partial answer.
    let trimmed = trim_for_summary(&messages, 2, 6);
    let (text, streamed, usage) = final_summary_ollama(
        &client,
        &chat_url,
        model,
        trimmed,
        max_tool_rounds,
        accumulated_usage,
    )
    .await?;
    Ok((text, streamed, usage, hallucination_count))
}

/// Returns `true` when `name` is a tool that doesn't modify the workspace.
/// Used to count consecutive read-only rounds and inject a write-nudge.
/// `save_note` writes *memory*, not the workspace — a round that only saved
/// a note must not suppress the stop-exploring-start-writing nudge; `recall`
/// (17.5) reads past conversations and is likewise pure exploration.
fn is_read_only_tool(name: &str) -> bool {
    matches!(
        name,
        "list_dir" | "read_file" | "search" | "web_fetch" | "use_skill" | "save_note" | "recall"
    )
}

/// Append the memory-nudge line to the current user message — the last
/// message in the list per the memory-manager contract. Defensive fallback:
/// if the last message somehow isn't a user turn, push a standalone user
/// message instead (mirrors the read-only-rounds nudge injection).
fn append_nudge_line(messages: &mut Vec<serde_json::Value>, line: &str) {
    match messages.last_mut() {
        Some(last) if last["role"] == "user" => {
            let cur = last["content"].as_str().unwrap_or_default();
            last["content"] = serde_json::Value::String(format!("{cur}\n\n{line}"));
        }
        _ => messages.push(serde_json::json!({"role": "user", "content": line})),
    }
}

fn ollama_non_content_fields(json: &serde_json::Value) -> Vec<&'static str> {
    let message = &json["message"];
    ["reasoning", "reasoning_content", "thinking"]
        .into_iter()
        .filter(|field| {
            message[*field]
                .as_str()
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
        })
        .collect()
}

fn ollama_response_shape(json: &serde_json::Value) -> String {
    let message = &json["message"];
    let message_keys = message
        .as_object()
        .map(|obj| obj.keys().cloned().collect::<Vec<_>>().join(","))
        .unwrap_or_else(|| "<missing>".to_string());
    let content_chars = message["content"]
        .as_str()
        .map(|content| content.chars().count())
        .unwrap_or(0);
    let tool_calls = message["tool_calls"]
        .as_array()
        .map(|calls| calls.len())
        .unwrap_or(0);
    let non_content = ollama_non_content_fields(json);
    let non_content = if non_content.is_empty() {
        "none".to_string()
    } else {
        non_content.join(",")
    };
    format!(
        "ollama response shape: message_keys=[{message_keys}] content_chars={content_chars} tool_calls={tool_calls} non_content_fields=[{non_content}] prompt_eval_count={} eval_count={}",
        json["prompt_eval_count"]
            .as_u64()
            .map_or("missing".to_string(), |n| n.to_string()),
        json["eval_count"]
            .as_u64()
            .map_or("missing".to_string(), |n| n.to_string())
    )
}

fn suspicious_empty_ollama_diagnostic(json: &serde_json::Value) -> String {
    let fields = ollama_non_content_fields(json);
    let field_note = if fields.is_empty() {
        "no known non-content fields were present".to_string()
    } else {
        format!("non-content field(s) present: {}", fields.join(", "))
    };
    format!(
        "(model generated output tokens but returned no assistant-visible content or tool calls; {field_note}; rerun with `newt --trace` to capture the response shape)"
    )
}

/// Build the nudge appended to the message list when the tool-round cap is hit.
fn cap_exit_nudge(max_tool_rounds: usize) -> String {
    format!(
        "You have reached the tool-call limit ({max_tool_rounds} rounds). \
         Do NOT call any more tools. Summarize what you found across the tool \
         calls above and give your best final answer now."
    )
}

/// Fallback message returned when even the final tools-disabled completion
/// fails. Includes accumulated token counts so the user knows what was consumed,
/// and gives actionable advice rather than just naming the limit.
fn cap_exit_fallback(max_tool_rounds: usize, accumulated: Option<crate::TokenUsage>) -> String {
    let tokens_hint = match accumulated {
        Some(u) => format!(
            " ({} in / {} out tokens consumed across {max_tool_rounds} rounds)",
            u.input_tokens, u.output_tokens,
        ),
        None => String::new(),
    };
    format!(
        "(reached the tool-call limit of {max_tool_rounds} rounds{tokens_hint}, \
         and the final summarization request also failed — \
         raise [tui].max_tool_rounds in your config, or ask a more focused question)"
    )
}

/// Final tools-disabled completion for the Ollama (`/api/chat`) path.
///
/// `messages` is the already-trimmed list (caller uses `trim_for_summary`).
/// `accumulated` carries usage from the preceding tool-call rounds so it
/// survives even when this summary request fails.
async fn final_summary_ollama(
    client: &reqwest::Client,
    chat_url: &str,
    model: &str,
    mut messages: Vec<serde_json::Value>,
    max_tool_rounds: usize,
    accumulated: Option<crate::TokenUsage>,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>)> {
    messages.push(serde_json::json!({
        "role": "user",
        "content": cap_exit_nudge(max_tool_rounds),
    }));
    // No `tools` key => the model cannot emit tool calls.
    let body = serde_json::json!({
        "model": model,
        "messages": &messages,
        "stream": false,
    });
    let retry = tui_retry_policy();
    let result = with_backoff_notify(
        &retry,
        || async {
            let resp = client
                .post(chat_url)
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("Ollama {status}: {text}");
            }
            resp.json::<serde_json::Value>()
                .await
                .map_err(anyhow::Error::from)
        },
        |_, _| {}, // no color context here; tracing::warn covers it
    )
    .await;
    match result {
        Ok(json) => {
            // #385: strip inline <think>…</think> reasoning Nemotron-style models emit
            // in the content stream (the separate `thinking` field is handled elsewhere).
            // All-reasoning content collapses to empty → the thinking-only recovery below.
            let (content, _reasoning) = crate::reasoning::split_reasoning(
                json["message"]["content"].as_str().unwrap_or(""),
            );
            let total = merge_round_usage(accumulated, ollama_usage(&json));
            if content.is_empty() {
                Ok((
                    cap_exit_fallback(max_tool_rounds, accumulated),
                    false,
                    accumulated,
                ))
            } else {
                Ok((content, false, total))
            }
        }
        // On any failure (including exhausted retries), still return the
        // accumulated usage so the caller can log the tokens consumed.
        Err(_) => Ok((
            cap_exit_fallback(max_tool_rounds, accumulated),
            false,
            accumulated,
        )),
    }
}

/// Final tools-disabled completion for the OpenAI (`/v1/chat/completions`) path.
///
/// `messages` is the already-trimmed list (caller uses `trim_for_summary`).
/// `accumulated` carries usage from the preceding tool-call rounds.
async fn final_summary_openai(
    client: &reqwest::Client,
    chat_url: &str,
    model: &str,
    api_key: Option<&str>,
    mut messages: Vec<serde_json::Value>,
    max_tool_rounds: usize,
    accumulated: Option<crate::TokenUsage>,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>)> {
    messages.push(serde_json::json!({
        "role": "user",
        "content": cap_exit_nudge(max_tool_rounds),
    }));
    // Omit `tools` / `tool_choice` => the model cannot emit tool calls.
    let body = serde_json::json!({
        "model": model,
        "messages": &messages,
        "stream": false,
    });
    let retry = tui_retry_policy();
    let result = with_backoff_notify(
        &retry,
        || async {
            let mut req = client.post(chat_url).json(&body);
            if let Some(key) = api_key {
                req = req.bearer_auth(key);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("inference endpoint {status}: {text}");
            }
            resp.json::<serde_json::Value>()
                .await
                .map_err(anyhow::Error::from)
        },
        |_, _| {},
    )
    .await;
    match result {
        Ok(json) => {
            // #385: strip inline <think>…</think> reasoning from the content.
            let (content, _reasoning) = crate::reasoning::split_reasoning(
                json["choices"][0]["message"]["content"]
                    .as_str()
                    .unwrap_or(""),
            );
            let total = merge_round_usage(accumulated, openai_usage(&json["usage"]));
            if content.is_empty() {
                Ok((
                    cap_exit_fallback(max_tool_rounds, accumulated),
                    false,
                    accumulated,
                ))
            } else {
                Ok((content, false, total))
            }
        }
        Err(_) => Ok((
            cap_exit_fallback(max_tool_rounds, accumulated),
            false,
            accumulated,
        )),
    }
}

/// OpenAI-compatible variant of [`chat_complete`]: the same agentic tool-call
/// loop, but over `POST {endpoint}/v1/chat/completions` with bearer auth and
/// the OpenAI `tool_calls` / `tool_call_id` / `usage` shapes.
///
/// Non-streaming for now — the final answer is returned (and printed by the
/// caller) rather than streamed token-by-token. Token-by-token SSE streaming
/// is a follow-up; functionally the loop is complete, including tools.
pub async fn openai_chat_complete(
    ctx: ChatCtx<'_>,
    mcp: &mut dyn McpTools,
) -> anyhow::Result<(String, bool, Option<crate::TokenUsage>, u32)> {
    let ChatCtx {
        url,
        model,
        kind: _,
        api_key,
        messages: mem_messages,
        task,
        workspace,
        color,
        caveats,
        max_tool_rounds,
        tool_output_lines,
        debug,
        trace,
        num_ctx,
        connect_timeout_secs,
        inference_timeout_secs,
        mid_loop_trim_threshold,
        mid_loop_trim_tokens,
        max_ok_input,
        build_check_cmd,
        safe_context,
        recover_cw_400,
        mut note_sink,
        mut note_nudge,
        recall_source,
        memory_source,
        summarizer,
        compress_state,
        mut tool_events,
        mut permission_gate,
        mut on_round_usage,
        estimate_ratio,
        exec_floor,
        write_ledger,
        cancel,
        git_tool,
        crew_runner,
    } = ctx;
    // Headless callers may pass no session state (mirrors the Ollama path).
    let mut local_compress_state = CompressState::new();
    let compress_state = match compress_state {
        Some(s) => s,
        None => &mut local_compress_state,
    };
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs))
        .timeout(std::time::Duration::from_secs(inference_timeout_secs))
        .build()?;
    let chat_url = format!("{}/v1/chat/completions", url.trim_end_matches('/'));
    let retry = tui_retry_policy();
    // The save_note tool is advertised only when a sink exists (Step 19.3);
    // recall only when a source exists (Step 17.5); memory_fetch only when a
    // memory source exists (#319) — mirrors the Ollama path.
    let advertise_save_note = note_sink.is_some();
    let advertise_recall = recall_source.is_some();
    let advertise_memory_fetch = memory_source.is_some();
    let advertise_git = git_tool.is_some();
    let advertise_team = crew_runner.is_some();

    let mut messages: Vec<serde_json::Value> = mem_messages
        .iter()
        .map(|m| serde_json::json!({"role": m.role.as_str(), "content": m.content}))
        .collect();

    // In-band memory nudge (Step 19.3) — mirrors the Ollama path.
    if note_sink.is_some() {
        if let Some(line) = note_nudge.as_deref_mut().and_then(NoteNudge::begin_turn) {
            append_nudge_line(&mut messages, &line);
        }
    }

    let mut accumulated_usage: Option<crate::TokenUsage> = None;
    let mut hallucination_count: u32 = 0;
    // Hard context-window 400s recovered (parse limit → trim → retry). See #223.
    let mut cw_retries: u32 = 0;
    // No-tools recovery (mirrors the Ollama path): a model that rejects the
    // `tools` field 400s even on "hello"; drop tools and retry, notice once.
    let mut tools_supported = true;
    let mut tools_unsupported_notified = false;
    // Pre-send token budget gate; tightened mid-turn by a recovered 400
    // (#223). Phase 20 §2.1 max(proven, believed) semantics — no `num_ctx`
    // ceiling on this wire (limits are server-side, e.g. vLLM
    // --max-model-len), so the ceiling leg is `None`.
    let mut send_budget: Option<usize> = initial_send_budget(max_ok_input, safe_context, None);
    // Step 20.3: on this wire there is no `num_ctx` ceiling, so the send
    // budget is authoritative only when a believed window (`safe_context`)
    // seeds it. Cloud OpenAI-compatible models have no `/api/show` to seed
    // one, so their budget rests on the proven-good HWM alone — the guard
    // fails open rather than refusing. A cw-400 flips this true mid-turn.
    let mut send_budget_authoritative = safe_context.is_some();
    // Tool schemas ride along in every request body; count them once (18.1).
    let tools = merged_tool_definitions(
        mcp,
        advertise_save_note,
        advertise_recall,
        advertise_memory_fetch,
        advertise_git,
        advertise_team,
    );
    let tool_tokens = estimate_value_tokens(&tools);
    // Phase 20 §2.3: per-turn calibration ratio + real-token schema overhead
    // (mirrors the Ollama path).
    let cal = sanitize_estimate_ratio(estimate_ratio);
    let tool_tokens_real = calibrate_up(tool_tokens, cal);
    // Truthful context-size tracker (prompt-tokens-preferred, Step 18.1).
    let mut prompt_tracker = PromptTracker::new();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Agentic loop — up to `max_tool_rounds` tool-call rounds (matches the Ollama path).
    'round_loop: for round in 0..max_tool_rounds {
        // Interrupt checkpoint (Esc / Ctrl-C), same contract as the Ollama path:
        // bail at the round boundary with an empty reply; the caller treats the
        // turn as abandoned. (The OpenAI path's per-request awaits are not yet
        // individually raced — round granularity is enough for the opt-in
        // provider plugin; the Ollama first-class path cancels mid-await.)
        if is_cancelled(cancel) {
            return Ok((String::new(), false, accumulated_usage, hallucination_count));
        }
        if round > 0 && color {
            execute!(
                io::stdout(),
                SetForegroundColor(CtColor::DarkGrey),
                Print("…\n"),
                ResetColor
            )
            .ok();
        }

        // Context compression (Step 18.4, #247 — mirrors the Ollama path):
        // the shared prune → boundary → redacted summary → marker pipeline
        // serves the mid-loop trigger and the pre-send budget guard.
        {
            // Phase 20 §2.3: calibrated `current` (real-token space) —
            // mirrors the Ollama path.
            let current = prompt_tracker.current(&messages, Some(&tools), cal);
            // Count-only budget priced in message-token space (F1) — mirrors
            // the Ollama path.
            let message_tokens = estimate_tokens(&messages);
            if let Some(trigger) = compression_trigger(
                messages.len(),
                current,
                message_tokens,
                mid_loop_trim_threshold,
                mid_loop_trim_tokens,
                send_budget,
                tool_tokens_real,
            ) {
                // Hard budgets are real-token currency → pipeline chars/4
                // (Phase 20 §2.3); count-only budgets pass through (F1).
                let pipeline_budget = if trigger.hard_budget {
                    calibrate_down(trigger.budget, cal)
                } else {
                    trigger.budget
                };
                // Step 20.3: authoritative iff a token threshold fired or the
                // send budget rests on a believed ceiling (mirrors the Ollama
                // loop). A lone-HWM guard is non-authoritative → fails open.
                let token_fired = mid_loop_trim_tokens.is_some_and(|t| t > 0 && current > t);
                let outcome = compress(
                    CompressRequest {
                        messages: &messages,
                        budget: pipeline_budget,
                        max_messages: trigger.max_messages,
                        task,
                        hard_budget: trigger.hard_budget,
                        authoritative: token_fired || send_budget_authoritative,
                        focus: None,
                    },
                    summarizer,
                    compress_state,
                )
                .await;
                if let Some(notice) = outcome.notice {
                    print_harness_notice(&notice, color);
                }
                if outcome.action == CompressAction::Refused {
                    anyhow::bail!(
                        "context (~{current} tokens) exceeds the model's input budget and \
                         auto-compression is disabled after repeated ineffective passes — \
                         start a new conversation or ask a more focused question, or run \
                         `newt tunings reset {model}` if this model's learned budget looks wrong"
                    );
                }
                if outcome.fired {
                    // N2 (mirrors the Ollama path): flag a still-over-budget
                    // assembly in the notice — compared in the pipeline's
                    // own chars/4 currency (Phase 20 §2.3).
                    let mut how = outcome.action.describe().to_string();
                    if trigger.hard_budget && outcome.tokens_after > pipeline_budget {
                        how.push_str(", still over budget");
                    }
                    emit_compression_notice(
                        color,
                        outcome.tokens_before,
                        outcome.tokens_after,
                        &how,
                    );
                    if debug {
                        print_debug(
                            &format!(
                                "compression: {} → {} messages (budget ~{} tokens, \
                                 +~{tool_tokens} tool-schema tokens ride along)",
                                messages.len(),
                                outcome.messages.len(),
                                pipeline_budget,
                            ),
                            color,
                        );
                    }
                    messages = outcome.messages;
                    prompt_tracker.invalidate();
                }
            }
        }

        // Phase 20 §2.2: chars/4 estimate of exactly the request about to be
        // dispatched — mirrors the Ollama path.
        let round_est_raw = estimate_request_tokens(&messages, Some(&tools));

        // OpenAI-compatible endpoints don't use Ollama's `options.num_ctx` —
        // context limits are configured server-side (vLLM --max-model-len).
        let mut body = serde_json::json!({
            "model": model,
            "messages": messages,
            "tools": tools.clone(),
            "tool_choice": "auto",
            "stream": false,
        });
        // Drop tools (and the now-meaningless tool_choice) for a model that
        // rejected them on a prior "does not support tools" 400.
        if !tools_supported {
            if let Some(o) = body.as_object_mut() {
                o.remove("tools");
                o.remove("tool_choice");
            }
        }
        // No `num_ctx` is sent here, so #282's per-request input ceiling has
        // no value to key on either — the pre-send guard stays on the cached
        // `max_ok_input` ∥ `safe_context` numbers and the cw-400 recovery
        // (these endpoints DO reject oversize requests with a parseable 400,
        // unlike Ollama's silent truncation).
        let _ = num_ctx; // not applicable for OpenAI-compatible endpoints
        let dispatch = with_backoff_notify(
            &retry,
            || async {
                let mut req = client.post(&chat_url).json(&body);
                if let Some(key) = api_key {
                    req = req.bearer_auth(key);
                }
                let resp = req
                    .send()
                    .await
                    .map_err(|e| anyhow::anyhow!("request failed: {e}"))?;
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("inference endpoint {status}: {text}");
                }
                resp.json::<serde_json::Value>()
                    .await
                    .map_err(anyhow::Error::from)
            },
            |attempt, delay| print_retry_indicator(attempt, delay, color),
        )
        .await;
        let json: serde_json::Value = match dispatch {
            Ok(j) => j,
            Err(e) => {
                // No-tools recovery: a model that rejects the `tools` field
                // (deepseek-r1) 400s even on "hello". Drop tools, notice once,
                // and re-dispatch the same round — self-limiting (the rebuilt
                // body omits tools) and session-persistent.
                if tools_supported && is_tools_unsupported_error(&e) {
                    tools_supported = false;
                    if !tools_unsupported_notified {
                        tools_unsupported_notified = true;
                        print_newt(
                            &format!(
                                "{model} does not support tools — tools disabled for this session"
                            ),
                            color,
                            false,
                        );
                    }
                    continue 'round_loop;
                }
                // Graceful context-window 400 recovery: parse the model's real
                // limit, tighten the budget, compress, and retry once (#223;
                // compress-not-trim since Step 18.4).
                if cw_retries < 2 {
                    if let Some(new_cap) = recover_cw_400.and_then(|f| f(&e, model, &today)) {
                        emit_overflow_notice(
                            color,
                            accumulated_usage.as_ref(),
                            Some(new_cap),
                            model,
                            cw_retries + 1,
                        );
                        send_budget = Some(new_cap as usize);
                        // The endpoint's parsed hard limit is authoritative
                        // from here on (Step 20.3; mirrors the Ollama path).
                        send_budget_authoritative = true;
                        let outcome = compress(
                            CompressRequest {
                                // Real-token cap minus real-token schema
                                // overhead → pipeline chars/4 currency
                                // (Phase 20 §2.3; mirrors the Ollama path).
                                messages: &messages,
                                budget: calibrate_down(
                                    (new_cap as usize).saturating_sub(tool_tokens_real),
                                    cal,
                                ),
                                max_messages: None,
                                task,
                                hard_budget: true,
                                authoritative: true,
                                focus: None,
                            },
                            summarizer,
                            compress_state,
                        )
                        .await;
                        if let Some(notice) = outcome.notice {
                            print_harness_notice(&notice, color);
                        }
                        if outcome.action == CompressAction::Refused {
                            // Refuse the resend; surface the endpoint's 400.
                            return Err(e);
                        }
                        if outcome.fired {
                            messages = outcome.messages;
                            prompt_tracker.invalidate();
                        }
                        cw_retries += 1;
                        continue 'round_loop;
                    }
                }
                return Err(e);
            }
        };
        // Merge per-round token usage (input = max single prompt, output =
        // sum — Step 18.1) and anchor the context-size tracker.
        let round_usage = openai_usage(&json["usage"]);
        if let Some(u) = round_usage {
            prompt_tracker.record(u.input_tokens, messages.len());
        }
        accumulated_usage = merge_round_usage(accumulated_usage, round_usage);

        // Phase 20 §2.2: no `num_ctx` on this wire, so there is no silent
        // head-truncation mode to suspect (oversize requests get a parseable
        // 400 instead) and no per-request ceiling on the mid-turn raise.
        let truncation_suspect = false;
        if let (Some(u), Some(budget), false) = (round_usage, send_budget, truncation_suspect) {
            let raised = u.input_tokens as usize;
            if raised > budget {
                send_budget = Some(raised);
                if debug {
                    print_debug(
                        &format!(
                            "send budget raised to ~{raised} tokens (backend accepted \
                             {}-token prompt)",
                            u.input_tokens
                        ),
                        color,
                    );
                }
            }
        }

        let message = &json["choices"][0]["message"];

        let tool_calls = message["tool_calls"].as_array();
        let has_tools = tool_calls.map(|tc| !tc.is_empty()).unwrap_or(false);

        if debug {
            let content = message["content"].as_str().unwrap_or("");
            let excerpt: String = content.chars().take(80).collect();
            let tc_count = tool_calls.map(|tc| tc.len()).unwrap_or(0);
            let usage_str = match round_usage {
                Some(u) => format!("{} in / {} out", u.input_tokens, u.output_tokens),
                None => "no usage".into(),
            };
            print_debug(
                &format!(
                    "round {round}: tool_calls={tc_count} usage=[{usage_str}] content={excerpt:?}"
                ),
                color,
            );
        }

        if !has_tools {
            let content = message["content"].as_str().unwrap_or("").to_string();
            if content.is_empty() && debug {
                print_debug(
                    "empty content with no tool calls — model produced nothing",
                    color,
                );
            }
            // Phase 20 §2.2: non-empty final content is usable output —
            // report the accepted prompt before returning.
            if !content.is_empty() {
                emit_accepted(
                    &mut on_round_usage,
                    round_usage,
                    truncation_suspect,
                    round_est_raw,
                );
            }
            let out = if content.is_empty() {
                "(model returned an empty response — try rephrasing, or check the model with `newt doctor`)".to_string()
            } else {
                content
            };
            return Ok((out, false, accumulated_usage, hallucination_count));
        }

        // Record the assistant turn verbatim (it carries the tool_calls), then
        // run each call and feed the result back keyed by its tool_call_id.
        // Phase 20 §2.2: tool calls are usable output (mirrors the Ollama path).
        emit_accepted(
            &mut on_round_usage,
            round_usage,
            truncation_suspect,
            round_est_raw,
        );
        messages.push(message.clone());
        for tc in tool_calls.unwrap() {
            let id = tc["id"].as_str().unwrap_or("");
            // Some API proxies (e.g. NVIDIA inference → Anthropic backend) wrap
            // Anthropic-native tool-use blocks inside the OpenAI `tool_calls`
            // array without converting the inner schema.  Fall back from the
            // OpenAI path (`function.name` / `function.arguments`) to the
            // Anthropic-native path (`name` / `input`) when the `function` key
            // is absent, so both wire formats route correctly.
            let anthropic_native = tc["function"].is_null();
            if anthropic_native && debug {
                let raw_name = tc["name"].as_str().unwrap_or("<missing>");
                print_debug(
                    &format!(
                        "tool call in Anthropic-native format inside tool_calls array \
                         (no `function` key) — name={raw_name:?}"
                    ),
                    color,
                );
            }
            let name = if anthropic_native {
                tc["name"].as_str().unwrap_or("unknown")
            } else {
                tc["function"]["name"].as_str().unwrap_or("unknown")
            };
            let args = if anthropic_native {
                tc["input"].clone()
            } else {
                match &tc["function"]["arguments"] {
                    serde_json::Value::String(s) => {
                        serde_json::from_str(s).unwrap_or(serde_json::Value::Null)
                    }
                    v => v.clone(),
                }
            };
            if trace {
                print_trace(
                    &format!(
                        "raw tool_call element: {}",
                        serde_json::to_string(tc).unwrap_or_else(|_| "?".into())
                    ),
                    color,
                );
            }
            let mcp_handles = mcp.handles(name);
            if debug {
                print_debug(
                    &format!("dispatching tool name={name:?} mcp_handles={mcp_handles}"),
                    color,
                );
            }
            if is_hallucination(name, &args) {
                hallucination_count += 1;
            }
            // Organic save_note use resets the memory-nudge counter (mirrors
            // the Ollama path).
            if name == "save_note" && note_sink.is_some() {
                if let Some(n) = note_nudge.as_deref_mut() {
                    n.note_saved();
                }
            }
            // retry technique: snapshot the file's pre-write bytes before the
            // write tool runs, so the post-turn gate can revert exactly newt's writes.
            ledger_note_write(write_ledger, name, &args, workspace);
            let tool_t0 = std::time::Instant::now();
            let result = execute_tool(
                name,
                &args,
                workspace,
                color,
                tool_output_lines,
                caveats,
                mcp,
                build_check_cmd.as_deref(),
                // Reborrow + re-coerce: shortens the trait-object lifetime to
                // this call (Option<&mut dyn _> is invariant, so the longer
                // ChatCtx lifetime can't unify directly).
                note_sink
                    .as_deref_mut()
                    .map(|s| &mut *s as &mut dyn NoteSink),
                recall_source,
                memory_source,
                // #263 prompted grants — same reborrow pattern as note_sink.
                permission_gate
                    .as_deref_mut()
                    .map(|g| &mut *g as &mut dyn PermissionGate),
                // #307: the active preset's exec floor (the bypass ceiling).
                exec_floor,
                // PR4: the injected embedded-git capability (None for headless).
                git_tool,
                // #479: the injected crew/team orchestration (None for headless).
                crew_runner,
            )
            .await;
            if debug {
                let excerpt: String = result.chars().take(120).collect();
                print_debug(&format!("tool result: {excerpt:?}"), color);
            }
            // 17.6: record the call for the turn's events column (mirrors
            // the Ollama path) — digested args, duration as a display claim.
            if let Some(rec) = tool_events.as_deref_mut() {
                rec.push(crate::ToolEvent::from_call(
                    name,
                    &args,
                    tools::tool_result_ok(&result),
                    u64::try_from(tool_t0.elapsed().as_millis()).ok(),
                ));
            }
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result,
            }));
        }
    }

    // Reached the round cap. Trim the message list and make ONE final
    // tools-disabled completion (matches the Ollama path).
    let trimmed = trim_for_summary(&messages, 2, 6);
    let (text, streamed, usage) = final_summary_openai(
        &client,
        &chat_url,
        model,
        api_key,
        trimmed,
        max_tool_rounds,
        accumulated_usage,
    )
    .await?;
    Ok((text, streamed, usage, hallucination_count))
}

/// Whether the reasoning spinner is enabled: `NEWT_THINKING` (set by
/// `/thinking`) overrides `[tui] thinking`; default on.
fn thinking_stream_enabled() -> bool {
    match std::env::var("NEWT_THINKING").ok().as_deref() {
        Some("off") => return false,
        Some("on" | "stream") => return true,
        _ => {}
    }
    crate::Config::resolve()
        .ok()
        .and_then(|c| c.tui)
        .map(|t| t.thinking == crate::ThinkingMode::Stream)
        .unwrap_or(true)
}

/// Spinner glyph frames (braille) for the thinking renderer.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The ephemeral spinner line text. Pure for testing. A braille spinner that
/// advances every frame (so the line is visibly alive even while only the clock
/// moves), a stage `label` telling the user what's happening right now
/// (`thinking…`, `compressing context…`, …), and the elapsed seconds. The
/// `· N chars` tail is shown only once generation has produced output
/// (`chars > 0`).
fn format_spinner(frame: usize, secs: f32, label: &str, chars: usize) -> String {
    let braille = SPINNER_FRAMES[frame % SPINNER_FRAMES.len()];
    if chars == 0 {
        format!("{braille} {label} {secs:.1}s")
    } else {
        format!("{braille} {label} {secs:.1}s · {chars} chars")
    }
}

/// Cargo-style thinking renderer: a model's reasoning (#385 — normally
/// suppressed) streams as DIM scrolled lines that stay in scrollback, with one
/// **ephemeral** spinner line pinned at the bottom (`\r`-redrawn, cleared with
/// `\x1b[K` — one line, never a region; the plain-scroller carve-out), showing
/// elapsed + size. Erased the instant the answer begins. Constructed only when
/// the caller opts in (TTY); headless never builds it.
struct ThinkingSpinner {
    color: bool,
    frame: usize,
    start: std::time::Instant,
    chars: usize,
    line_buf: String,
    spinner_drawn: bool,
}

impl ThinkingSpinner {
    fn new(color: bool) -> Self {
        Self {
            color,
            frame: 0,
            start: std::time::Instant::now(),
            chars: 0,
            line_buf: String::new(),
            spinner_drawn: false,
        }
    }

    /// Feed a reasoning chunk: flush completed lines as dim scrollback, then
    /// redraw the bottom spinner.
    fn reasoning(&mut self, chunk: &str) {
        self.chars += chunk.chars().count();
        self.line_buf.push_str(chunk);
        while let Some(nl) = self.line_buf.find('\n') {
            let line: String = self.line_buf.drain(..=nl).collect();
            self.print_dim_line(line.trim_end_matches(['\n', '\r']));
        }
        self.redraw_spinner();
    }

    fn print_dim_line(&mut self, line: &str) {
        self.erase_spinner();
        if line.trim().is_empty() {
            return;
        }
        let mut out = io::stdout();
        if self.color {
            let _ = execute!(
                out,
                SetForegroundColor(CtColor::DarkGrey),
                Print("  "),
                Print(line),
                ResetColor,
                Print("\n"),
            );
        } else {
            let _ = writeln!(out, "  {line}");
        }
    }

    fn redraw_spinner(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        let line = format_spinner(
            self.frame,
            self.start.elapsed().as_secs_f32(),
            "thinking…",
            self.chars,
        );
        let mut out = io::stdout();
        if self.color {
            let _ = execute!(
                out,
                Print("\r\x1b[K"),
                SetForegroundColor(CtColor::DarkGrey),
                Print(&line),
                ResetColor,
            );
        } else {
            let _ = write!(out, "\r{line}");
        }
        let _ = out.flush();
        self.spinner_drawn = true;
    }

    fn erase_spinner(&mut self) {
        if self.spinner_drawn {
            let _ = write!(io::stdout(), "\r\x1b[K");
            let _ = io::stdout().flush();
            self.spinner_drawn = false;
        }
    }

    /// The answer is starting (or the stream ended): flush trailing reasoning
    /// and erase the spinner so output flows on cleanly. Idempotent.
    fn finish(&mut self) {
        if !self.line_buf.is_empty() {
            let tail = std::mem::take(&mut self.line_buf);
            self.print_dim_line(tail.trim_end_matches(['\n', '\r']));
        }
        self.erase_spinner();
    }
}

/// Stream an Ollama NDJSON response, printing tokens as they arrive.
/// Returns `(accumulated_text, token_usage)`.
/// Token usage is extracted from the final chunk (`done: true`).
/// `show_thinking` opts into the cargo-style reasoning spinner (TTY only).
async fn stream_response(
    resp: reqwest::Response,
    color: bool,
    show_thinking: bool,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> anyhow::Result<(String, Option<crate::TokenUsage>)> {
    let mut spinner = show_thinking.then(|| ThinkingSpinner::new(color));
    let mut full = String::new();
    let mut started = false;
    let mut usage: Option<crate::TokenUsage> = None;
    // #385: suppress inline <think>…</think> reasoning from the live stream + the
    // accumulated reply, even when a tag is split across token boundaries.
    let mut think = crate::reasoning::ThinkFilter::new();

    let mut resp = resp;
    // Race each chunk read against the interrupt flag so Esc stops the token
    // stream promptly; on interrupt, stop reading and return what we have.
    while let Some(chunk) = match cancellable(cancel, resp.chunk()).await {
        Some(c) => c?,
        None => None,
    } {
        let text = String::from_utf8_lossy(&chunk);
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let Ok(json) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let raw = json["message"]["content"].as_str().unwrap_or("");
            let (token, reasoning) = think.feed_split(raw);
            // Surface reasoning live (cargo-style) — both the inline `<think>`
            // span the filter just split out AND any separate `thinking` field.
            if let Some(sp) = spinner.as_mut() {
                if !reasoning.is_empty() {
                    sp.reasoning(&reasoning);
                }
                if let Some(t) = json["message"]["thinking"].as_str() {
                    if !t.is_empty() {
                        sp.reasoning(t);
                    }
                }
            }
            let token = token.as_str();
            if !token.is_empty() {
                if !started {
                    // The answer is starting — tear the spinner down first.
                    if let Some(sp) = spinner.as_mut() {
                        sp.finish();
                    }
                    if color {
                        execute!(
                            io::stdout(),
                            SetForegroundColor(NEWT_ORANGE_CT),
                            Print("▸  "),
                            ResetColor,
                        )
                        .ok();
                    } else {
                        print!("▸  ");
                    }
                    started = true;
                }
                print!("{token}");
                io::stdout().flush().ok();
                full.push_str(token);
            }
            if json["done"].as_bool().unwrap_or(false) {
                // Extract token counts from the final Ollama chunk.
                let input = json["prompt_eval_count"].as_u64().map(|n| n as u32);
                let output = json["eval_count"].as_u64().map(|n| n as u32);
                usage = input.zip(output).map(|(i, o)| crate::TokenUsage {
                    input_tokens: i,
                    output_tokens: o,
                });
                break;
            }
        }
    }
    // #385: flush any clean tail the filter held back (a trailing run that turned out
    // not to be the start of a `<think>` tag).
    let tail = think.finish();
    if !tail.is_empty() {
        if !started {
            if let Some(sp) = spinner.as_mut() {
                sp.finish();
            }
            print!("▸  ");
            started = true;
        }
        print!("{tail}");
        io::stdout().flush().ok();
        full.push_str(&tail);
    }
    // All-reasoning response (no clean content): tear the spinner down anyway so
    // the terminal isn't left mid-spinner.
    if let Some(sp) = spinner.as_mut() {
        sp.finish();
    }
    if started {
        println!();
    }
    Ok((full, usage))
}

#[cfg(test)]
mod cap_exit_unit_tests {
    use super::*;

    #[test]
    fn cap_exit_nudge_names_the_limit() {
        let nudge = cap_exit_nudge(5);
        assert!(nudge.contains("5 rounds"), "got: {nudge}");
        assert!(nudge.contains("Do NOT call any more tools"));
    }

    #[test]
    fn spinner_line_formats_and_frame_wraps() {
        // Braille spinner + stage label + clock; the chars tail shows once
        // generation has produced output.
        assert_eq!(
            format_spinner(0, 1.23, "thinking…", 340),
            "⠋ thinking… 1.2s · 340 chars"
        );
        // A different stage label, and chars == 0 drops the `· N chars` tail.
        assert_eq!(
            format_spinner(1, 0.5, "compressing context…", 0),
            "⠙ compressing context… 0.5s"
        );
        // Frame index wraps over the braille glyph set.
        assert!(
            format_spinner(SPINNER_FRAMES.len(), 0.0, "thinking…", 0).contains(SPINNER_FRAMES[0])
        );
    }

    #[test]
    fn cap_exit_fallback_includes_usage_when_present() {
        let with = cap_exit_fallback(
            4,
            Some(crate::TokenUsage {
                input_tokens: 12,
                output_tokens: 34,
            }),
        );
        assert!(with.contains("12 in / 34 out tokens"), "got: {with}");
        assert!(with.contains("max_tool_rounds"));

        let without = cap_exit_fallback(4, None);
        assert!(!without.contains("tokens consumed"), "got: {without}");
        assert!(without.contains("tool-call limit of 4"));
    }

    #[test]
    fn read_only_tools_classified_correctly() {
        // save_note writes memory, not the workspace: a round that only
        // saved a note must still count toward the read-only write-nudge.
        for name in &[
            "list_dir",
            "read_file",
            "search",
            "web_fetch",
            "use_skill",
            "save_note",
        ] {
            assert!(is_read_only_tool(name), "{name} should be read-only");
        }
    }

    #[test]
    fn write_tools_not_read_only() {
        for name in &["edit_file", "write_file", "run_command"] {
            assert!(!is_read_only_tool(name), "{name} should NOT be read-only");
        }
    }
}

// ---------------------------------------------------------------------------
// Tool-call round cap + graceful cap-exit (issue: configurable max_tool_rounds)
// ---------------------------------------------------------------------------
//
// These tests exercise both agentic loops (`chat_complete` -> Ollama path and
// `openai_chat_complete`) against a wiremock backend. The mock returns tool
// calls while `tools` are present in the request and a real text answer once
// they are absent — letting us assert that:
//   (1) the loop honours the configured `max_tool_rounds` cap, and
//   (2) on hitting the cap newt issues ONE final tools-disabled completion and
//       returns its text (NOT the `(reached tool-call limit)` placeholder).
//
// (The companion test that recovers a hard context-window 400 via the
// `recover_cw_400` hook lives in newt-tui — it exercises the TUI-side probe
// cache persistence under a HOME env guard.)
#[cfg(test)]
mod tool_round_cap_tests {
    use super::*;
    use crate::caveats::Caveats;
    use crate::{BackendKind, MemMessage};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    /// Was the `"tools"` key present on this request body?
    fn request_has_tools(req: &Request) -> bool {
        serde_json::from_slice::<serde_json::Value>(&req.body)
            .ok()
            .map(|v| v.get("tools").is_some())
            .unwrap_or(false)
    }

    /// Ollama-shaped responder: returns a tool call whenever `tools` are
    /// offered, and a plain text answer once they are withheld. Counts the
    /// number of tool-offering requests it served.
    struct OllamaResponder {
        tool_rounds_served: Arc<AtomicUsize>,
        final_answer: String,
    }

    impl Respond for OllamaResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                self.tool_rounds_served.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{
                            "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                        }]
                    }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": self.final_answer }
                }))
            }
        }
    }

    /// OpenAI-shaped responder: same logic, OpenAI `choices[0].message` shape.
    struct OpenAiResponder {
        tool_rounds_served: Arc<AtomicUsize>,
        final_answer: String,
    }

    impl Respond for OpenAiResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if request_has_tools(req) {
                self.tool_rounds_served.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "definitely_not_a_real_tool", "arguments": "{}" }
                        }]
                    }}]
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": self.final_answer } }]
                }))
            }
        }
    }

    fn msgs() -> Vec<MemMessage> {
        vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ]
    }

    #[tokio::test]
    async fn ollama_loop_honors_configured_cap_and_returns_real_final_answer() {
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(OllamaResponder {
                tool_rounds_served: served.clone(),
                final_answer: "here is my partial summary".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let cap = 3;
        let (reply, streamed, _usage, _hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: cap,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                git_tool: None,
                crew_runner: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete should succeed");

        // The cap was honoured: exactly `cap` tool-offering rounds were served.
        assert_eq!(served.load(Ordering::SeqCst), cap);
        // The cap-exit issued a final tools-disabled completion and returned
        // its text — NOT the dead placeholder.
        assert_eq!(reply, "here is my partial summary");
        assert_ne!(reply, "(reached tool-call limit)");
        assert!(!streamed);
    }

    #[tokio::test]
    async fn a_set_cancel_flag_abandons_the_turn_before_any_network_call() {
        // The interrupt checkpoint at the round-loop top runs before the first
        // request, so a pre-tripped flag returns instantly — the bogus URL
        // (a closed port) is never contacted. If the checkpoint regressed,
        // the dispatch would try to connect and this would not return empty.
        let messages = msgs();
        let caveats = Caveats::top();
        let flag = std::sync::atomic::AtomicBool::new(true);
        let (reply, streamed, usage, hallu) = chat_complete(
            ChatCtx {
                url: "http://127.0.0.1:1",
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: 5,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                exec_floor: None,
                write_ledger: None,
                cancel: Some(&flag),
                git_tool: None,
                crew_runner: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("an interrupted turn still returns Ok, just empty");
        assert!(reply.is_empty(), "interrupted before any model output");
        assert!(!streamed);
        assert!(usage.is_none());
        assert_eq!(hallu, 0);
    }

    #[tokio::test]
    async fn openai_loop_honors_configured_cap_and_returns_real_final_answer() {
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiResponder {
                tool_rounds_served: served.clone(),
                final_answer: "openai partial answer".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let cap = 2;
        let (reply, streamed, _usage, _hallu) = openai_chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Openai,
                api_key: Some("sk-test"),
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: cap,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                git_tool: None,
                crew_runner: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("openai_chat_complete should succeed");

        assert_eq!(served.load(Ordering::SeqCst), cap);
        assert_eq!(reply, "openai partial answer");
        assert_ne!(reply, "(reached tool-call limit)");
        assert!(!streamed);
    }

    /// 17.6: with a recorder lent in `ChatCtx.tool_events`, the Ollama loop
    /// records one event per executed tool call — name as invoked, digested
    /// args (keys + hash, never raw values), best-effort outcome, duration
    /// claim. Without a recorder (every other test here) nothing changes.
    #[tokio::test]
    async fn ollama_loop_records_tool_events_with_digested_args() {
        let server = MockServer::start().await;
        struct TwoToolResponder;
        impl Respond for TwoToolResponder {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                if request_has_tools(req) {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": { "content": "", "tool_calls": [
                            { "function": { "name": "list_dir",
                                            "arguments": {"path": "."} } },
                            { "function": { "name": "definitely_not_a_real_tool",
                                            "arguments": {"token": "tippy-top-secret"} } }
                        ]}
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": { "content": "done" }
                    }))
                }
            }
        }
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(TwoToolResponder)
            .mount(&server)
            .await;

        let ws = tempfile::TempDir::new().unwrap();
        let workspace = ws.path().to_string_lossy().into_owned();
        let messages = msgs();
        let caveats = Caveats::top();
        let mut events: Vec<crate::ToolEvent> = Vec::new();
        chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: &workspace,
                color: false,
                caveats: &caveats,
                max_tool_rounds: 1,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: Some(&mut events),
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                git_tool: None,
                crew_runner: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete should succeed");

        assert_eq!(events.len(), 2, "one event per tool call: {events:?}");
        assert_eq!(events[0].tool, "list_dir");
        assert!(events[0].ok, "a real listing reads as success");
        assert!(events[0].args_digest.contains("path"));
        assert!(events[0].duration_ms.is_some());
        assert_eq!(events[1].tool, "definitely_not_a_real_tool");
        assert!(!events[1].ok, "an unknown tool reads as failure");
        // Args are digested, never recorded raw.
        assert!(events[1].args_digest.contains("token"));
        assert!(
            !events[1].args_digest.contains("tippy-top-secret"),
            "raw arg value leaked: {}",
            events[1].args_digest
        );
    }

    /// 17.6: the OpenAI loop records the same per-call events (its tool
    /// arguments arrive as a JSON *string* — the digest must match the
    /// parsed-args digest the Ollama path produces for identical args).
    #[tokio::test]
    async fn openai_loop_records_tool_events_with_digested_args() {
        let server = MockServer::start().await;
        struct OneToolResponder;
        impl Respond for OneToolResponder {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                if request_has_tools(req) {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "choices": [{ "message": {
                            "content": null,
                            "tool_calls": [{
                                "id": "call_1",
                                "type": "function",
                                "function": { "name": "list_dir",
                                              "arguments": "{\"path\": \".\"}" }
                            }]
                        }}]
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "choices": [{ "message": { "content": "done" } }]
                    }))
                }
            }
        }
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OneToolResponder)
            .mount(&server)
            .await;

        let ws = tempfile::TempDir::new().unwrap();
        let workspace = ws.path().to_string_lossy().into_owned();
        let messages = msgs();
        let caveats = Caveats::top();
        let mut events: Vec<crate::ToolEvent> = Vec::new();
        openai_chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Openai,
                api_key: Some("sk-test"),
                messages: &messages,
                task: "do the thing",
                workspace: &workspace,
                color: false,
                caveats: &caveats,
                max_tool_rounds: 1,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: Some(&mut events),
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                git_tool: None,
                crew_runner: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("openai_chat_complete should succeed");

        assert_eq!(events.len(), 1, "one event per tool call: {events:?}");
        assert_eq!(events[0].tool, "list_dir");
        assert!(events[0].ok);
        assert_eq!(
            events[0].args_digest,
            crate::ToolEvent::from_call("x", &serde_json::json!({"path": "."}), true, None)
                .args_digest,
            "string-encoded args must digest like parsed args"
        );
    }

    #[tokio::test]
    async fn cap_exit_fallback_when_final_summary_errors() {
        // No mock for the tools-disabled request would still 404 via the
        // tool-offering mock only matching when... actually both match the same
        // path, so instead we mount a server that always 500s the *second*
        // shape. Simpler: a server that returns tool calls for tools-present
        // and a 500 for tools-absent, forcing the fallback branch.
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));
        struct ErrOnFinal {
            served: Arc<AtomicUsize>,
        }
        impl Respond for ErrOnFinal {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                if request_has_tools(req) {
                    self.served.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": { "content": "", "tool_calls": [{
                            "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                        }]}
                    }))
                } else {
                    ResponseTemplate::new(500).set_body_string("boom")
                }
            }
        }
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ErrOnFinal {
                served: served.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, _streamed, _usage, _hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: 2,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                git_tool: None,
                crew_runner: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete should succeed even when final summary errors");

        // Fallback names the limit + the knob — strictly better than the bare
        // placeholder.
        assert!(reply.contains("tool-call limit"));
        assert!(reply.contains("max_tool_rounds"));
    }

    /// `run_command` called with a tool name as the first word must return a
    /// corrective error message, not shell it through agent-bridle.
    #[tokio::test]
    async fn run_command_refuses_tool_name_as_shell_command() {
        let ws = tempfile::TempDir::new().unwrap();
        let caveats = Caveats::top();
        for tool in [
            "list_dir",
            "read_file",
            "write_file",
            "use_skill",
            "web_fetch",
        ] {
            let args = serde_json::json!({ "command": format!("{tool} some/path") });
            let out = execute_tool(
                "run_command",
                &args,
                &ws.path().to_string_lossy(),
                false,
                20,
                &caveats,
                &mut NoMcp,
                None,
                None,
                None,
                None, // memory_source
                None,
                None,
                None, // git_tool
                None, // crew_runner
            )
            .await;
            assert!(
                out.contains("is a tool, not a shell command"),
                "expected corrective message for '{tool}', got: {out}"
            );
        }
    }

    /// When the final summary 500s, the accumulated usage from the tool rounds
    /// must still be returned (not None), so usage.jsonl is not blank.
    #[tokio::test]
    async fn accumulated_usage_survives_summary_failure() {
        let server = MockServer::start().await;
        let served = Arc::new(AtomicUsize::new(0));

        struct UsageRoundsErrFinal {
            served: Arc<AtomicUsize>,
        }
        impl Respond for UsageRoundsErrFinal {
            fn respond(&self, req: &Request) -> ResponseTemplate {
                if request_has_tools(req) {
                    self.served.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": { "content": "", "tool_calls": [{
                            "function": { "name": "definitely_not_a_real_tool", "arguments": {} }
                        }]},
                        // Ollama reports per-round usage even in non-streaming mode.
                        "prompt_eval_count": 100,
                        "eval_count": 20,
                    }))
                } else {
                    ResponseTemplate::new(500).set_body_string("boom")
                }
            }
        }

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(UsageRoundsErrFinal {
                served: served.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let cap = 2;
        let (reply, _streamed, usage, hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "do the thing",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: cap,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 120,
                mid_loop_trim_threshold: 40,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                git_tool: None,
                crew_runner: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete must succeed even when final summary errors");

        // The fallback reply must contain accumulated token counts.
        assert!(reply.contains("tool-call limit"), "got: {reply}");
        assert!(
            reply.contains("in / ") && reply.contains("out tokens"),
            "fallback must include accumulated token counts, got: {reply}"
        );

        // The usage returned must be non-None and reflect the rounds.
        let u = usage.expect("usage must be Some even when final summary fails");
        // SEMANTICS CHANGED in Step 18.1: each round's 100-token prompt
        // contained the same history, so the turn input is the largest single
        // prompt (100), not the 200 sum that double-counted it.
        assert_eq!(
            u.input_tokens, 100,
            "largest single prompt across 2 rounds, not the sum"
        );
        assert_eq!(
            u.output_tokens, 40,
            "2 rounds × 20 output tokens each = 40 total"
        );

        // Unknown tool calls during cap rounds counted as hallucinations.
        assert_eq!(
            hallu, cap as u32,
            "each round had one hallucinated tool call"
        );
    }

    // -----------------------------------------------------------------------
    // Read-only nudge injection test
    //
    // Scenario: model keeps calling list_dir (read-only) for 3 rounds.
    // On round 4 the harness injects the nudge.  The responder detects the
    // nudge text in the message list and returns a final text answer instead
    // of another tool call, proving the nudge reached the model.
    // -----------------------------------------------------------------------

    struct ReadOnlyNudgeResponder {
        /// Flipped to true the first time the responder sees the nudge text.
        nudge_seen: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Respond for ReadOnlyNudgeResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = serde_json::from_slice::<serde_json::Value>(&req.body).unwrap_or_default();
            let has_nudge = body["messages"]
                .as_array()
                .map(|msgs| {
                    msgs.iter().any(|m| {
                        m["content"]
                            .as_str()
                            .map(|c| c.contains("read-only rounds so far"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);

            if has_nudge {
                self.nudge_seen
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                // Return a plain text answer — no more tool calls.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "nudge received, writing file now" }
                }))
            } else if request_has_tools(req) {
                // Keep returning list_dir calls until the nudge arrives.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{ "function": {
                            "name": "list_dir",
                            "arguments": { "path": "." }
                        }}]
                    }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "final summary" }
                }))
            }
        }
    }

    #[tokio::test]
    async fn read_only_nudge_injected_after_three_rounds() {
        let server = MockServer::start().await;
        let nudge_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ReadOnlyNudgeResponder {
                nudge_seen: nudge_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, _streamed, _usage, _hallu) = chat_complete(
            ChatCtx {
                url: &server.uri(),
                model: "test-model",
                kind: BackendKind::Ollama,
                api_key: None,
                messages: &messages,
                task: "list all files",
                workspace: ".",
                color: false,
                caveats: &caveats,
                max_tool_rounds: 10,
                tool_output_lines: 5,
                debug: false,
                trace: false,
                num_ctx: None,
                connect_timeout_secs: 5,
                inference_timeout_secs: 30,
                mid_loop_trim_threshold: 40,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                exec_floor: None,
                write_ledger: None,
                cancel: None,
                git_tool: None,
                crew_runner: None,
            },
            &mut NoMcp,
        )
        .await
        .expect("chat_complete should succeed");

        assert!(
            nudge_seen.load(std::sync::atomic::Ordering::SeqCst),
            "nudge was never injected after 3 consecutive read-only rounds"
        );
        assert_eq!(
            reply, "nudge received, writing file now",
            "model should have responded to the nudge with a final answer"
        );
    }
}

// ---------------------------------------------------------------------------
// HTTP-loop tests — streaming, overflow retry, mid-loop trim, and final
// summary, all against wiremock backends.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod http_loop_tests {
    use super::*;
    use crate::caveats::Caveats;
    use crate::{BackendKind, MemMessage};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn msgs() -> Vec<MemMessage> {
        vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ]
    }

    fn ctx<'a>(
        server_uri: &'a str,
        messages: &'a [MemMessage],
        caveats: &'a Caveats,
    ) -> ChatCtx<'a> {
        ChatCtx {
            url: server_uri,
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages,
            task: "do the thing",
            workspace: ".",
            color: false,
            caveats,
            max_tool_rounds: 8,
            tool_output_lines: 20,
            debug: false,
            trace: false,
            num_ctx: None,
            connect_timeout_secs: 5,
            inference_timeout_secs: 30,
            mid_loop_trim_threshold: 40,
            mid_loop_trim_tokens: None,
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: None,
            recover_cw_400: None,
            note_sink: None,
            note_nudge: None,
            recall_source: None,
            memory_source: None,
            summarizer: None,
            compress_state: None,
            tool_events: None,
            permission_gate: None,
            on_round_usage: None,
            estimate_ratio: None,
            exec_floor: None,
            write_ledger: None,
            cancel: None,
            git_tool: None,
            crew_runner: None,
        }
    }

    fn body_json(req: &Request) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap_or_default()
    }

    fn is_stream(req: &Request) -> bool {
        body_json(req)["stream"].as_bool().unwrap_or(false)
    }

    fn ndjson(lines: &[serde_json::Value]) -> ResponseTemplate {
        let body: String = lines
            .iter()
            .map(|l| format!("{l}\n"))
            .collect::<Vec<_>>()
            .join("");
        ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/x-ndjson")
    }

    /// Probe (stream:false) answers with plain content; the streaming re-issue
    /// (stream:true) returns NDJSON tokens with usage on the `done` chunk.
    struct StreamHappyResponder;
    impl Respond for StreamHappyResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                ndjson(&[
                    serde_json::json!({"message": {"content": "Hello "}, "done": false}),
                    serde_json::json!({
                        "message": {"content": "world"}, "done": true,
                        "prompt_eval_count": 7, "eval_count": 3
                    }),
                ])
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "probe answer"},
                    "prompt_eval_count": 5, "eval_count": 2,
                }))
            }
        }
    }

    #[tokio::test]
    async fn ollama_streams_final_answer_and_merges_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(StreamHappyResponder)
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, streamed, usage, hallu) =
            chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
                .await
                .expect("chat_complete should succeed");

        assert_eq!(reply, "Hello world", "tokens accumulated across chunks");
        assert!(streamed, "the streaming path printed the tokens");
        let u = usage.expect("probe + stream usage merged");
        // SEMANTICS CHANGED in Step 18.1: both requests carried the same
        // conversation, so input is max(5, 7) = 7 — the old sum (12) counted
        // the shared history twice. Output is still 2 + 3 (new generation).
        assert_eq!(u.input_tokens, 7, "max(5 probe, 7 stream), not the sum");
        assert_eq!(u.output_tokens, 5, "2 (probe) + 3 (stream)");
        assert_eq!(hallu, 0);
    }

    #[test]
    fn detects_tools_unsupported_400_phrasings() {
        assert!(is_tools_unsupported_error(&anyhow::anyhow!(
            "Ollama 400 Bad Request: registry.ollama.ai/library/deepseek-r1:70b does not support tools"
        )));
        // Looser OpenAI-compatible phrasing.
        assert!(is_tools_unsupported_error(&anyhow::anyhow!(
            "this model does not support tools at this time"
        )));
        // Unrelated 400s must NOT trip the no-tools path.
        assert!(!is_tools_unsupported_error(&anyhow::anyhow!(
            "Ollama 400 Bad Request: context window exceeded"
        )));
    }

    /// A model that rejects the `tools` field (deepseek-r1) 400s on the first
    /// dispatch; newt must drop tools and re-dispatch, answering normally. The
    /// tools-absent retry is the one that succeeds — no tools-400 loop.
    struct NoToolsResponder {
        rejections: Arc<AtomicUsize>,
        served_without_tools: Arc<AtomicBool>,
    }
    impl Respond for NoToolsResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let has_tools = body_json(req).get("tools").is_some();
            if has_tools {
                self.rejections.fetch_add(1, Ordering::SeqCst);
                return ResponseTemplate::new(400).set_body_string(
                    "registry.ollama.ai/library/deepseek-r1:70b does not support tools",
                );
            }
            self.served_without_tools.store(true, Ordering::SeqCst);
            if is_stream(req) {
                ndjson(&[serde_json::json!({
                    "message": {"content": "hello there"}, "done": true,
                    "prompt_eval_count": 4, "eval_count": 2
                })])
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "probe answer"},
                    "prompt_eval_count": 4, "eval_count": 2,
                }))
            }
        }
    }

    #[tokio::test]
    async fn no_tools_model_recovers_by_dropping_tools() {
        let server = MockServer::start().await;
        let rejections = Arc::new(AtomicUsize::new(0));
        let served_without_tools = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(NoToolsResponder {
                rejections: rejections.clone(),
                served_without_tools: served_without_tools.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, streamed, _usage, _) =
            chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
                .await
                .expect("a no-tools model still answers a bare prompt");

        assert_eq!(reply, "hello there", "the tools-absent retry answered");
        assert!(streamed);
        assert!(
            served_without_tools.load(Ordering::SeqCst),
            "a request without the tools field was eventually served"
        );
        assert_eq!(
            rejections.load(Ordering::SeqCst),
            1,
            "exactly one tools-bearing request 400s — the drop is self-limiting"
        );
    }

    /// The streaming re-issue produces no tokens — the loop must fall back to
    /// the probe round's content rather than returning silence.
    struct EmptyStreamResponder;
    impl Respond for EmptyStreamResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                ndjson(&[serde_json::json!({"message": {"content": ""}, "done": true})])
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "probe says hi"},
                    "prompt_eval_count": 5, "eval_count": 2,
                }))
            }
        }
    }

    #[tokio::test]
    async fn empty_stream_falls_back_to_probe_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(EmptyStreamResponder)
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, streamed, usage, _) =
            chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
                .await
                .expect("chat_complete should succeed");

        assert_eq!(reply, "probe says hi");
        assert!(!streamed, "fallback content was never streamed");
        assert_eq!(usage.unwrap().input_tokens, 5);
    }

    /// Probe AND stream both empty, with no safe-context hint → the loop gives
    /// the explicit empty-response diagnostic instead of silence.
    struct AllEmptyResponder;
    impl Respond for AllEmptyResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                ndjson(&[serde_json::json!({"message": {"content": ""}, "done": true})])
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": ""}}))
            }
        }
    }

    #[tokio::test]
    async fn fully_empty_response_yields_diagnostic_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(AllEmptyResponder)
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, streamed, _, _) =
            chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
                .await
                .expect("chat_complete should succeed");

        assert!(
            reply.contains("model returned an empty response"),
            "got: {reply}"
        );
        assert!(reply.contains("newt doctor"), "points at diagnostics");
        assert!(!streamed);
    }

    struct SuspiciousEmptyThenRecover {
        probes: Arc<AtomicUsize>,
        saw_nudge: Arc<AtomicBool>,
    }
    impl Respond for SuspiciousEmptyThenRecover {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                if self.probes.load(Ordering::SeqCst) <= 1 {
                    ndjson(&[serde_json::json!({
                        "message": {"content": ""},
                        "done": true,
                        "prompt_eval_count": 9,
                        "eval_count": 4
                    })])
                } else {
                    ndjson(&[
                        serde_json::json!({"message": {"content": "recovered "}, "done": false}),
                        serde_json::json!({
                            "message": {"content": "after empty retry"},
                            "done": true,
                            "prompt_eval_count": 5,
                            "eval_count": 3
                        }),
                    ])
                }
            } else {
                let body = body_json(req);
                if body["messages"].as_array().into_iter().flatten().any(|m| {
                    m["content"]
                        .as_str()
                        .unwrap_or("")
                        .contains("no assistant-visible content")
                }) {
                    self.saw_nudge.store(true, Ordering::SeqCst);
                }
                let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": {
                            "content": "",
                            "thinking": "I know what to do but did not emit final text."
                        },
                        "prompt_eval_count": 10,
                        "eval_count": 2559,
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": {"content": "recovered after empty retry"},
                        "prompt_eval_count": 5,
                        "eval_count": 3,
                    }))
                }
            }
        }
    }

    #[tokio::test]
    async fn suspicious_empty_generated_output_retries_with_nudge() {
        let server = MockServer::start().await;
        let probes = Arc::new(AtomicUsize::new(0));
        let saw_nudge = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(SuspiciousEmptyThenRecover {
                probes: probes.clone(),
                saw_nudge: saw_nudge.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, streamed, usage, _) =
            chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
                .await
                .expect("chat_complete should succeed");

        assert_eq!(reply, "recovered after empty retry");
        assert!(streamed);
        assert_eq!(probes.load(Ordering::SeqCst), 2);
        assert!(saw_nudge.load(Ordering::SeqCst));
        assert!(
            usage
                .expect("usage survives suspicious retry")
                .output_tokens
                >= 2566,
            "usage from the suspicious empty round must be preserved"
        );
    }

    struct SuspiciousEmptyStaysEmpty;
    impl Respond for SuspiciousEmptyStaysEmpty {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                ndjson(&[serde_json::json!({
                    "message": {"content": ""},
                    "done": true,
                    "prompt_eval_count": 9,
                    "eval_count": 4
                })])
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "reasoning_content": "internal-only response"
                    },
                    "prompt_eval_count": 10,
                    "eval_count": 12,
                }))
            }
        }
    }

    #[tokio::test]
    async fn suspicious_empty_generated_output_reports_targeted_diagnostic() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(SuspiciousEmptyStaysEmpty)
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.trace = true;
        let (reply, streamed, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert!(reply.contains("generated output tokens"), "got: {reply}");
        assert!(
            reply.contains("reasoning_content"),
            "diagnostic should name the non-content field: {reply}"
        );
        assert!(reply.contains("--trace"), "points at trace diagnostics");
        assert!(!streamed);
    }

    /// First round: empty content with token usage near the safe-context
    /// ceiling → the loop must emit the overflow notice, trim, and retry.
    /// Second round: a real answer.
    struct OverflowThenRecover {
        probes: Arc<AtomicUsize>,
    }
    impl Respond for OverflowThenRecover {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                // Streams mirror the probe sequence: empty first, content after.
                if self.probes.load(Ordering::SeqCst) <= 1 {
                    ndjson(&[serde_json::json!({
                        "message": {"content": ""}, "done": true,
                        "prompt_eval_count": 90, "eval_count": 1
                    })])
                } else {
                    ndjson(&[
                        serde_json::json!({"message": {"content": "recovered "}, "done": false}),
                        serde_json::json!({
                            "message": {"content": "after trim"}, "done": true,
                            "prompt_eval_count": 12, "eval_count": 4
                        }),
                    ])
                }
            } else {
                let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": {"content": ""},
                        "prompt_eval_count": 90, "eval_count": 1,
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": {"content": "recovered after trim"},
                        "prompt_eval_count": 12, "eval_count": 4,
                    }))
                }
            }
        }
    }

    #[tokio::test]
    async fn context_overflow_trims_and_retries_then_recovers() {
        let server = MockServer::start().await;
        let probes = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(OverflowThenRecover {
                probes: probes.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        // Safe window of 100 input tokens: the empty round reported a 90-token
        // prompt, and 90 ≥ 85% of 100, so it is classified as likely overflow.
        // (Step 18.1: the check compares the largest single prompt against the
        // window — the old multi-round sum, 180 here, inflated past 85% after
        // two rounds on EVERY long turn, firing spurious overflow retries.)
        c.safe_context = Some(100);
        let (reply, streamed, usage, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert_eq!(
            probes.load(Ordering::SeqCst),
            2,
            "overflow must trigger exactly one trim-and-retry probe"
        );
        assert_eq!(reply, "recovered after trim");
        assert!(streamed);
        assert_eq!(
            usage
                .expect("accumulated usage survives the retry")
                .input_tokens,
            90,
            "largest single prompt across the overflowed + recovered rounds"
        );
    }

    /// Tool calls every round with a tiny trim threshold: the mid-loop
    /// compression must fire — observable as the compaction marker (NOT the
    /// old amputation placeholder) reaching the model. With no summarizer
    /// injected, this is the static-fallback path (Step 18.4).
    struct TrimObservingResponder {
        marker_seen: Arc<AtomicBool>,
        old_placeholder_seen: Arc<AtomicBool>,
    }
    impl Respond for TrimObservingResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            let contains = |needle: &str| {
                body["messages"]
                    .as_array()
                    .map(|m| {
                        m.iter().any(|msg| {
                            msg["content"]
                                .as_str()
                                .map(|c| c.contains(needle))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            };
            if contains(SUMMARY_PREFIX) && contains("Summary generation was unavailable.") {
                self.marker_seen.store(true, Ordering::SeqCst);
            }
            if contains("earlier tool-call messages omitted") {
                self.old_placeholder_seen.store(true, Ordering::SeqCst);
            }
            if body.get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "", "tool_calls": [{
                        "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                    }]}
                }))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": "final after trim"}}))
            }
        }
    }

    #[tokio::test]
    async fn mid_loop_compression_fires_when_message_list_grows() {
        let server = MockServer::start().await;
        let marker_seen = Arc::new(AtomicBool::new(false));
        let old_placeholder_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(TrimObservingResponder {
                marker_seen: marker_seen.clone(),
                old_placeholder_seen: old_placeholder_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.max_tool_rounds = 3;
        c.mid_loop_trim_threshold = 4;
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert!(
            marker_seen.load(Ordering::SeqCst),
            "the static compaction marker must have reached the model mid-loop"
        );
        assert!(
            !old_placeholder_seen.load(Ordering::SeqCst),
            "the pre-18.4 amputation placeholder must never be emitted"
        );
        assert_eq!(reply, "final after trim");
    }

    /// The cap-exit summary round returns 200 with EMPTY content: the loop
    /// must surface the named fallback, not the empty string.
    struct EmptyFinalSummary;
    impl Respond for EmptyFinalSummary {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if body_json(req).get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "", "tool_calls": [{
                        "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                    }]}
                }))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": ""}}))
            }
        }
    }

    #[tokio::test]
    async fn empty_final_summary_yields_cap_fallback() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(EmptyFinalSummary)
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.max_tool_rounds = 2;
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert!(reply.contains("tool-call limit of 2"), "got: {reply}");
        assert!(reply.contains("max_tool_rounds"), "names the knob");
    }

    // -----------------------------------------------------------------------
    // OpenAI-path coverage
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn chat_complete_dispatches_openai_kind_and_returns_first_round_answer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "openai says hi"}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 4},
            })))
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.api_key = Some("sk-test");
        // Calling chat_complete (not openai_chat_complete) pins the dispatch.
        let (reply, streamed, usage, hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("openai dispatch should succeed");

        assert_eq!(reply, "openai says hi");
        assert!(!streamed, "openai path is non-streaming");
        let u = usage.unwrap();
        assert_eq!((u.input_tokens, u.output_tokens), (10, 4));
        assert_eq!(hallu, 0);
    }

    #[tokio::test]
    async fn openai_empty_content_yields_diagnostic_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": ""}}]
            })))
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.expect("should succeed");
        assert!(
            reply.contains("model returned an empty response"),
            "got: {reply}"
        );
    }

    /// Mock MCP that handles exactly one namespaced tool for routing tests.
    struct OneToolMcp {
        name: &'static str,
        result: &'static str,
    }
    #[async_trait::async_trait]
    impl McpTools for OneToolMcp {
        fn handles(&self, name: &str) -> bool {
            name == self.name
        }
        fn tool_defs(&self) -> Vec<serde_json::Value> {
            Vec::new()
        }
        async fn call(&mut self, _name: &str, _args: &serde_json::Value) -> String {
            self.result.to_string()
        }
    }

    /// An API proxy may put Anthropic-native tool-use blocks
    /// (`{"name":"…","input":{}}`) inside the OpenAI `tool_calls` array
    /// instead of converting them to `{"function":{"name":"…","arguments":"…"}}`.
    /// The loop must detect the missing `function` key, fall back to the
    /// Anthropic-native fields, and route the call correctly.
    #[tokio::test]
    async fn openai_anthropic_native_tool_calls_route_correctly() {
        let server = MockServer::start().await;
        // Round 1: Anthropic-native tool-use block in the tool_calls array.
        // Round 2: plain text final answer after receiving the tool result.
        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cc = call_count.clone();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |_req: &Request| {
                let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "choices": [{"message": {
                            "content": null,
                            "tool_calls": [{
                                "type": "tool_use",
                                "id": "toolu_01ABC",
                                "name": "my_server__my_tool",
                                "input": {"key": "value"}
                            }]
                        }}],
                        "usage": {"prompt_tokens": 50, "completion_tokens": 10}
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "choices": [{"message": {"content": "done after anthropic-native tool"}}],
                        "usage": {"prompt_tokens": 60, "completion_tokens": 8}
                    }))
                }
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        let mut mcp = OneToolMcp {
            name: "my_server__my_tool",
            result: "tool-result-text",
        };
        let (reply, _, _, hallu) = chat_complete(c, &mut mcp)
            .await
            .expect("should succeed with anthropic-native tool format");
        assert_eq!(reply, "done after anthropic-native tool");
        assert_eq!(hallu, 0, "should not be counted as hallucination");
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "must have done both rounds (tool call + final answer)"
        );
    }

    /// Regression: some OpenAI-compatible API proxies normalise hyphens to
    /// underscores in tool names (`acme-server` → `acme_server`).  Verify that
    /// the underscore form routes through MCP rather than falling to "unknown tool".
    #[tokio::test]
    async fn openai_hyphenated_server_name_routes_through_mcp() {
        let server = MockServer::start().await;
        let call_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cc = call_count.clone();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |_req: &Request| {
                let n = cc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    // Proxy returns the underscore-normalised form of the
                    // server prefix even though we advertised hyphens.
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "choices": [{"message": {
                            "content": "",
                            "tool_calls": [{
                                "index": 0,
                                "function": {
                                    "arguments": "{}",
                                    "name": "acme_server__probe_tool"
                                },
                                "id": "call_probe_01",
                                "type": "function"
                            }]
                        }}],
                        "usage": {"prompt_tokens": 599, "completion_tokens": 30}
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "choices": [{"message": {"content": "outlook routed correctly"}}]
                    }))
                }
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        // OneToolMcp.handles() must match the underscore form the proxy returns.
        let mut mcp = OneToolMcp {
            name: "acme_server__probe_tool",
            result: "ok",
        };
        let (reply, _, _, _) = chat_complete(c, &mut mcp)
            .await
            .expect("should route hyphenated server name through mcp");
        assert_eq!(reply, "outlook routed correctly");
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "must have completed both rounds"
        );
    }

    /// OpenAI mirror of the Ollama cap-exit fallback: tool calls until the cap,
    /// then a 400 on the tools-disabled summary → the named fallback.
    struct OpenAiErrOnFinal;
    impl Respond for OpenAiErrOnFinal {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if body_json(req).get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "definitely_not_a_real_tool", "arguments": "{}"}
                        }]
                    }}]
                }))
            } else {
                ResponseTemplate::new(400).set_body_string("bad request")
            }
        }
    }

    #[tokio::test]
    async fn openai_cap_exit_fallback_when_final_summary_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiErrOnFinal)
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.max_tool_rounds = 2;
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("must succeed even when the summary errors");
        assert!(reply.contains("tool-call limit of 2"), "got: {reply}");
        assert!(reply.contains("max_tool_rounds"));
    }
}

// ---------------------------------------------------------------------------
// save_note tool + memory nudge — loop integration (Step 19.3, #248)
// ---------------------------------------------------------------------------
//
// Wiremock-backed tests against both agentic loops, pinning:
//   (1) save_note is advertised iff a NoteSink is present, and a save_note
//       tool call routes through the sink with the result fed back;
//   (2) the in-band memory nudge is appended to the user message when due —
//       and ONLY when a sink exists;
//   (3) organic save_note use resets the nudge counter (the read-only-rounds
//       reset pattern, hermes's reset-on-memory-write).
#[cfg(test)]
mod save_note_loop_tests {
    use super::note_sink::tests::MockSink;
    use super::*;
    use crate::caveats::Caveats;
    use crate::{BackendKind, MemMessage};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn msgs() -> Vec<MemMessage> {
        vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ]
    }

    fn ctx<'a>(
        server_uri: &'a str,
        messages: &'a [MemMessage],
        caveats: &'a Caveats,
    ) -> ChatCtx<'a> {
        ChatCtx {
            url: server_uri,
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages,
            task: "do the thing",
            workspace: ".",
            color: false,
            caveats,
            max_tool_rounds: 6,
            tool_output_lines: 20,
            debug: false,
            trace: false,
            num_ctx: None,
            connect_timeout_secs: 5,
            inference_timeout_secs: 30,
            mid_loop_trim_threshold: 40,
            mid_loop_trim_tokens: None,
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: None,
            recover_cw_400: None,
            note_sink: None,
            note_nudge: None,
            recall_source: None,
            memory_source: None,
            summarizer: None,
            compress_state: None,
            tool_events: None,
            permission_gate: None,
            on_round_usage: None,
            estimate_ratio: None,
            exec_floor: None,
            write_ledger: None,
            cancel: None,
            git_tool: None,
            crew_runner: None,
        }
    }

    fn body_json(req: &Request) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap_or_default()
    }

    fn advertised_tool_names(body: &serde_json::Value) -> Vec<String> {
        body["tools"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|d| d["function"]["name"].as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn messages_contain(body: &serde_json::Value, needle: &str) -> bool {
        body["messages"]
            .as_array()
            .map(|msgs| {
                msgs.iter().any(|m| {
                    m["content"]
                        .as_str()
                        .map(|c| c.contains(needle))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }

    /// Ollama-shaped responder: issues one save_note tool call, then a final
    /// text answer once the "note saved:" tool result is visible in history.
    /// Also records whether save_note was advertised and whether the memory
    /// nudge line reached the model.
    struct SaveNoteResponder {
        save_note_advertised: Arc<AtomicBool>,
        nudge_seen: Arc<AtomicBool>,
        final_answer: String,
    }

    impl Respond for SaveNoteResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if advertised_tool_names(&body).contains(&"save_note".to_string()) {
                self.save_note_advertised.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "[system reminder:")
                && messages_contain(&body, "without a saved note")
            {
                self.nudge_seen.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "note saved:") {
                // The tool result round-tripped — answer for real now.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": self.final_answer }
                }))
            } else if body.get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{ "function": {
                            "name": "save_note",
                            "arguments": {
                                "action": "add",
                                "text": "user prefers vi keybindings"
                            }
                        }}]
                    }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "final summary" }
                }))
            }
        }
    }

    #[tokio::test]
    async fn ollama_save_note_routes_to_sink_and_result_feeds_back() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(SaveNoteResponder {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
                final_answer: "noted, moving on".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink::default();
        let mut c = ctx(&uri, &messages, &caveats);
        c.note_sink = Some(&mut sink);
        let (reply, _streamed, _usage, hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert!(
            advertised.load(Ordering::SeqCst),
            "save_note must be advertised when a sink is present"
        );
        assert_eq!(
            sink.calls,
            vec!["add:user prefers vi keybindings"],
            "the tool call must route through the sink"
        );
        assert_eq!(reply, "noted, moving on");
        assert_eq!(hallu, 0, "save_note is a real tool, not a hallucination");
        assert!(
            !nudge_seen.load(Ordering::SeqCst),
            "no nudge configured — none may be injected"
        );
    }

    /// Without a sink the tool must be absent from the advertised set, and a
    /// configured nudge must NOT be appended (absent-without-sink).
    struct NoSinkObserver {
        save_note_advertised: Arc<AtomicBool>,
        nudge_seen: Arc<AtomicBool>,
    }

    impl Respond for NoSinkObserver {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if advertised_tool_names(&body).contains(&"save_note".to_string()) {
                self.save_note_advertised.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "[system reminder:") {
                self.nudge_seen.store(true, Ordering::SeqCst);
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": { "content": "plain answer" }
            }))
        }
    }

    #[tokio::test]
    async fn without_sink_no_tool_and_no_nudge_even_when_due() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(NoSinkObserver {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        // A nudge that is overdue (interval 1, one quiet turn already counted)…
        let mut nudge = NoteNudge::new(1);
        let _ = nudge.begin_turn();
        let mut c = ctx(&uri, &messages, &caveats);
        // …but NO sink: the loop must neither advertise nor nudge.
        c.note_nudge = Some(&mut nudge);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert_eq!(reply, "plain answer");
        assert!(
            !advertised.load(Ordering::SeqCst),
            "save_note advertised without a sink"
        );
        assert!(
            !nudge_seen.load(Ordering::SeqCst),
            "nudge injected without a sink"
        );
    }

    #[tokio::test]
    async fn nudge_appended_to_user_message_when_due() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(NoSinkObserver {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink::default();
        // One quiet turn already elapsed → due on this (the next) turn.
        let mut nudge = NoteNudge::new(1);
        let _ = nudge.begin_turn();
        let mut c = ctx(&uri, &messages, &caveats);
        c.note_sink = Some(&mut sink);
        c.note_nudge = Some(&mut nudge);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert_eq!(reply, "plain answer");
        assert!(
            nudge_seen.load(Ordering::SeqCst),
            "the reminder line must reach the model on the due turn"
        );
    }

    #[tokio::test]
    async fn organic_save_resets_the_nudge_counter() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(SaveNoteResponder {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
                final_answer: "done".into(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink::default();
        let mut nudge = NoteNudge::new(1);
        let mut c = ctx(&uri, &messages, &caveats);
        c.note_sink = Some(&mut sink);
        c.note_nudge = Some(&mut nudge);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");
        assert_eq!(reply, "done");
        assert_eq!(sink.calls.len(), 1, "the model saved organically");

        // The turn included an organic save → the counter restarted, so the
        // next turn must NOT be nudged (without the save, interval=1 would
        // have made it due).
        assert!(
            nudge.begin_turn().is_none(),
            "organic save_note use must reset the nudge counter"
        );
    }

    /// OpenAI-shaped mirror: save_note advertised + routed, nudge appended.
    struct OpenAiSaveNoteResponder {
        save_note_advertised: Arc<AtomicBool>,
        nudge_seen: Arc<AtomicBool>,
    }

    impl Respond for OpenAiSaveNoteResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if advertised_tool_names(&body).contains(&"save_note".to_string()) {
                self.save_note_advertised.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "[system reminder:") {
                self.nudge_seen.store(true, Ordering::SeqCst);
            }
            if messages_contain(&body, "note saved:") {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": "openai noted" } }]
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "save_note",
                                "arguments": "{\"action\":\"add\",\"text\":\"CI gate is just check\"}"
                            }
                        }]
                    }}]
                }))
            }
        }
    }

    #[tokio::test]
    async fn openai_save_note_routes_and_nudge_appends() {
        let server = MockServer::start().await;
        let advertised = Arc::new(AtomicBool::new(false));
        let nudge_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiSaveNoteResponder {
                save_note_advertised: advertised.clone(),
                nudge_seen: nudge_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink::default();
        let mut nudge = NoteNudge::new(1);
        let _ = nudge.begin_turn(); // due on this turn
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.note_sink = Some(&mut sink);
        c.note_nudge = Some(&mut nudge);
        let (reply, _, _, hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("openai loop should succeed");

        assert_eq!(reply, "openai noted");
        assert_eq!(sink.calls, vec!["add:CI gate is just check"]);
        assert!(advertised.load(Ordering::SeqCst));
        assert!(nudge_seen.load(Ordering::SeqCst));
        assert_eq!(hallu, 0);
    }

    /// A sink error (here: the 19.1 over-budget curator error) must round-trip
    /// to the model verbatim as the tool result so it can replace/remove and
    /// retry — pinned end-to-end through the loop.
    struct ErrorEchoResponder {
        error_seen_by_model: Arc<AtomicBool>,
    }

    impl Respond for ErrorEchoResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if messages_contain(&body, "Replace or remove existing entries first")
                && messages_contain(&body, "1. an existing entry")
            {
                self.error_seen_by_model.store(true, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "I will curate first" }
                }))
            } else if body.get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "tool_calls": [{ "function": {
                            "name": "save_note",
                            "arguments": { "action": "add", "text": "too big" }
                        }}]
                    }
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "final summary" }
                }))
            }
        }
    }

    #[tokio::test]
    async fn over_budget_error_round_trips_to_the_model() {
        let server = MockServer::start().await;
        let error_seen = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ErrorEchoResponder {
                error_seen_by_model: error_seen.clone(),
            })
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut sink = MockSink {
            fail_with: Some(
                "NOTES.md is full: this write needs 99/50 chars. \
                 Replace or remove existing entries first.\nCurrent entries:\n  1. an existing entry"
                    .into(),
            ),
            ..Default::default()
        };
        let mut c = ctx(&uri, &messages, &caveats);
        c.note_sink = Some(&mut sink);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        assert_eq!(reply, "I will curate first");
        assert!(
            error_seen.load(Ordering::SeqCst),
            "the curator error (full entry list + instruction) must reach the model verbatim"
        );
    }
}

// ---------------------------------------------------------------------------
// Compression v2 — summarize, don't discard (Step 18.4, #247)
// ---------------------------------------------------------------------------
//
// End-to-end wiremock tests for the compression pipeline wired into both
// loops. The headline property is B5's acceptance criterion from the context
// baseline (docs/testing/results/context-baseline-f0f4f6e.md): a long
// tool-heavy conversation crosses the token budget, compression fires, and
// the ORIGINAL TASK still reaches the next request — where the baseline
// measured 9/10 silently wrong answers because truncation discarded it (B6).
#[cfg(test)]
mod compression_loop_tests {
    use super::*;
    use crate::caveats::Caveats;
    use crate::{BackendKind, MemMessage};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    const TASK: &str =
        "ACTIVE TASK GAUNTLET-7f3d9c: read big.txt until told to stop, then restate this marker";
    const CANNED_SUMMARY: &str =
        "## Active Task\nACTIVE TASK GAUNTLET-7f3d9c (canned summary)\n## Completed Actions\n1. read big.txt";

    fn msgs() -> Vec<MemMessage> {
        vec![MemMessage::system("you are a test"), MemMessage::user(TASK)]
    }

    fn ctx<'a>(
        server_uri: &'a str,
        messages: &'a [MemMessage],
        caveats: &'a Caveats,
        workspace: &'a str,
    ) -> ChatCtx<'a> {
        ChatCtx {
            url: server_uri,
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages,
            task: TASK,
            workspace,
            color: false,
            caveats,
            max_tool_rounds: 12,
            tool_output_lines: 2,
            debug: false,
            trace: false,
            num_ctx: None,
            connect_timeout_secs: 5,
            inference_timeout_secs: 30,
            mid_loop_trim_threshold: 40,
            // The token trigger under test: well below what a few 4 KB
            // tool results accumulate to.
            mid_loop_trim_tokens: Some(5_000),
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: None,
            recover_cw_400: None,
            note_sink: None,
            note_nudge: None,
            recall_source: None,
            memory_source: None,
            summarizer: None,
            compress_state: None,
            tool_events: None,
            permission_gate: None,
            on_round_usage: None,
            estimate_ratio: None,
            exec_floor: None,
            write_ledger: None,
            cancel: None,
            git_tool: None,
            crew_runner: None,
        }
    }

    /// Workspace with one ~4 KB file the mock model reads over and over.
    fn gauntlet_workspace() -> tempfile::TempDir {
        let ws = tempfile::TempDir::new().unwrap();
        let line = "the quick brown newt compresses context without discarding it\n";
        std::fs::write(ws.path().join("big.txt"), line.repeat(64)).unwrap();
        ws
    }

    fn body_json(req: &Request) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap_or_default()
    }

    fn messages_contain(body: &serde_json::Value, needle: &str) -> bool {
        body["messages"]
            .as_array()
            .map(|msgs| {
                msgs.iter().any(|m| {
                    m["content"]
                        .as_str()
                        .map(|c| c.contains(needle))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }

    /// chars/4 estimate of a request's message list (mirrors the loop's
    /// fallback estimator) — used to measure the reclaim across requests.
    fn body_message_tokens(body: &serde_json::Value) -> usize {
        body["messages"]
            .as_array()
            .map(|msgs| {
                msgs.iter()
                    .map(|m| m.to_string().chars().count().div_ceil(4))
                    .sum()
            })
            .unwrap_or(0)
    }

    /// A summarizer that records every request it receives and returns the
    /// canned summary.
    fn canned_summarizer(prompts: Arc<Mutex<Vec<String>>>) -> Summarizer {
        Box::new(move |prompt: String| {
            let prompts = prompts.clone();
            Box::pin(async move {
                prompts.lock().unwrap().push(prompt);
                Ok(CANNED_SUMMARY.to_string())
            })
        })
    }

    /// Ollama-shaped gauntlet responder: keeps demanding `read_file` of the
    /// big fixture until the compaction marker shows up in the request, then
    /// answers. Records per-request observations the assertions need.
    struct GauntletResponder {
        final_answer: String,
        /// `(had_marker, est_message_tokens)` per non-streaming request.
        log: Arc<Mutex<Vec<(bool, usize)>>>,
        task_in_marker_request: Arc<AtomicBool>,
        summary_in_marker_request: Arc<AtomicBool>,
        old_placeholder_seen: Arc<AtomicBool>,
        static_marker_instead: bool,
    }

    impl Respond for GauntletResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            let has_marker = messages_contain(&body, SUMMARY_PREFIX);
            if !body["stream"].as_bool().unwrap_or(false) {
                self.log
                    .lock()
                    .unwrap()
                    .push((has_marker, body_message_tokens(&body)));
            }
            if messages_contain(&body, "earlier tool-call messages omitted") {
                self.old_placeholder_seen.store(true, Ordering::SeqCst);
            }
            if has_marker {
                if messages_contain(&body, TASK) {
                    self.task_in_marker_request.store(true, Ordering::SeqCst);
                }
                let summary_needle = if self.static_marker_instead {
                    "Summary generation was unavailable."
                } else {
                    CANNED_SUMMARY
                };
                if messages_contain(&body, summary_needle) {
                    self.summary_in_marker_request.store(true, Ordering::SeqCst);
                }
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": self.final_answer }
                }));
            }
            if body.get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "", "tool_calls": [{
                        "function": { "name": "read_file", "arguments": { "path": "big.txt" } }
                    }]}
                }))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "message": { "content": "cap exit" } }))
            }
        }
    }

    /// THE B5 acceptance property: compression fires on a long tool-heavy
    /// conversation and the original task text still reaches the next
    /// request — summarized, not discarded.
    #[tokio::test]
    async fn active_task_survives_compression() {
        let server = MockServer::start().await;
        let log = Arc::new(Mutex::new(Vec::new()));
        let task_in_marker = Arc::new(AtomicBool::new(false));
        let summary_in_marker = Arc::new(AtomicBool::new(false));
        let old_placeholder = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(GauntletResponder {
                final_answer: "the marker is GAUNTLET-7f3d9c".into(),
                log: log.clone(),
                task_in_marker_request: task_in_marker.clone(),
                summary_in_marker_request: summary_in_marker.clone(),
                old_placeholder_seen: old_placeholder.clone(),
                static_marker_instead: false,
            })
            .mount(&server)
            .await;

        let ws = gauntlet_workspace();
        let workspace = ws.path().to_string_lossy().to_string();
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let summarizer = canned_summarizer(prompts.clone());
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        let (reply, _streamed, _usage, hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

        // The turn completed with a real answer, not a cap/diagnostic exit.
        assert_eq!(reply, "the marker is GAUNTLET-7f3d9c");
        assert_eq!(hallu, 0);

        // The summarizer ran exactly once and its request carried the
        // original task verbatim (the verbatim-Active-Task anchor).
        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1, "one compression, one summary request");
        assert!(
            prompts[0].contains(TASK),
            "summary request must quote the task verbatim"
        );

        // The post-compression request still carried the task AND the
        // summary, wrapped in the marker — summarize, don't discard.
        assert!(task_in_marker.load(Ordering::SeqCst), "B5 property");
        assert!(summary_in_marker.load(Ordering::SeqCst));
        assert!(
            !old_placeholder.load(Ordering::SeqCst),
            "the old amputation placeholder must never be dispatched"
        );

        // Reclaim numbers: the compressed request must be materially smaller
        // than the largest pre-compression request.
        let log = log.lock().unwrap();
        let before = log
            .iter()
            .filter(|(m, _)| !m)
            .map(|&(_, t)| t)
            .max()
            .expect("pre-compression requests were dispatched");
        let after = log
            .iter()
            .find(|(m, _)| *m)
            .map(|&(_, t)| t)
            .expect("a compressed request was dispatched");
        println!("e2e reclaim: ~{before} -> ~{after} est. message tokens");
        assert!(
            after < before * 6 / 10,
            "compression must reclaim >40% here (got {before} -> {after})"
        );
    }

    /// THE B6 regression (#282): a FIRST-turn request on a fresh capability
    /// cache (no `max_ok_input`, no `safe_context`) whose history exceeds the
    /// `num_ctx` ceiling must compress BEFORE dispatch and dispatch under the
    /// ceiling. Pre-fix, `send_budget` was `None` here — the after-benchmark
    /// measured all 10 B6 runs shipping ~41k-token requests into a forced
    /// 4,096 window with zero compression events (8/10 silently wrong),
    /// because the `num_ctx` newt itself sent fed into nothing.
    #[tokio::test]
    async fn first_turn_over_num_ctx_ceiling_compresses_before_dispatch() {
        let server = MockServer::start().await;
        let log = Arc::new(Mutex::new(Vec::new()));
        let task_in_marker = Arc::new(AtomicBool::new(false));
        let summary_in_marker = Arc::new(AtomicBool::new(false));
        let old_placeholder = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(GauntletResponder {
                final_answer: "the marker is GAUNTLET-7f3d9c".into(),
                log: log.clone(),
                task_in_marker_request: task_in_marker.clone(),
                summary_in_marker_request: summary_in_marker.clone(),
                old_placeholder_seen: old_placeholder.clone(),
                static_marker_instead: false,
            })
            .mount(&server)
            .await;

        // The B6 shape, condensed: turn 1 of a fresh process already carries
        // a history far over the forced window (a restored conversation whose
        // assistant replies dumped file contents) — ~34k chars ≈ 9k estimated
        // tokens against a 4,096 num_ctx. The task itself is small and sits
        // up front (the protected head), the bulk is summarizable middle, and
        // the recent tail is small — compression CAN reach the budget here,
        // so a still-over-budget dispatch would be a wiring failure, not an
        // incompressibility artifact.
        let filler = "the quick brown newt reads three fifty-kilobyte fixtures\n".repeat(50);
        let mut messages = vec![MemMessage::system("you are a test"), MemMessage::user(TASK)];
        for _ in 0..12 {
            messages.push(MemMessage::assistant(format!("file contents: {filler}")));
            messages.push(MemMessage::user("continue"));
        }

        let ws = gauntlet_workspace();
        let workspace = ws.path().to_string_lossy().to_string();
        let caveats = Caveats::top();
        let uri = server.uri();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let summarizer = canned_summarizer(prompts.clone());
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        // First turn of a fresh session: NO capability-cache numbers, NO
        // token threshold — pre-#282 nothing armed the trigger. The only
        // ceiling in play is the num_ctx the loop itself is about to send.
        c.max_ok_input = None;
        c.safe_context = None;
        c.mid_loop_trim_tokens = None;
        c.num_ctx = Some(4_096);
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        let (reply, _streamed, _usage, _hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("the first turn must complete");

        // The turn produced the real answer (visibly degraded, not silently
        // wrong: compression ran and the model answered from the summary).
        assert_eq!(reply, "the marker is GAUNTLET-7f3d9c");

        // Compression fired BEFORE the first dispatch: the summarizer ran,
        // and the VERY FIRST request the backend ever saw already carried
        // the compaction marker.
        assert_eq!(prompts.lock().unwrap().len(), 1, "one pre-send compression");
        let log = log.lock().unwrap();
        let (first_had_marker, first_tokens) =
            *log.first().expect("at least one request dispatched");
        assert!(
            first_had_marker,
            "B6: the first dispatched request must already be compressed — \
             pre-#282 it went out raw at ~9k tokens"
        );

        // And it dispatched UNDER the ceiling: 80% of 4,096 = 3,276 input
        // tokens (the same reply headroom the probe math reserves).
        assert!(
            first_tokens <= 3_276,
            "first dispatch must fit the num_ctx input ceiling \
             (got ~{first_tokens} est. message tokens > 3,276)"
        );

        // Summarize-don't-discard still holds on the turn-1 path.
        assert!(task_in_marker.load(Ordering::SeqCst), "task survives");
        assert!(summary_in_marker.load(Ordering::SeqCst), "summary present");
        assert!(!old_placeholder.load(Ordering::SeqCst));
    }

    /// Summarizer endpoint returns 500 → the static marker is dispatched
    /// instead and the turn still completes (never aborts).
    #[tokio::test]
    async fn summarizer_500_degrades_to_static_marker_and_turn_completes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/summarize"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let log = Arc::new(Mutex::new(Vec::new()));
        let task_in_marker = Arc::new(AtomicBool::new(false));
        let static_in_marker = Arc::new(AtomicBool::new(false));
        let old_placeholder = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(GauntletResponder {
                final_answer: "completed despite summarizer outage".into(),
                log: log.clone(),
                task_in_marker_request: task_in_marker.clone(),
                summary_in_marker_request: static_in_marker.clone(),
                old_placeholder_seen: old_placeholder.clone(),
                static_marker_instead: true,
            })
            .mount(&server)
            .await;

        // A summarizer that really performs the HTTP call — and gets a 500.
        let attempts = Arc::new(AtomicUsize::new(0));
        let summarize_url = format!("{}/summarize", server.uri());
        let attempts_in = attempts.clone();
        let summarizer: Summarizer = Box::new(move |prompt: String| {
            let url = summarize_url.clone();
            let attempts = attempts_in.clone();
            Box::pin(async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                let resp = reqwest::Client::new()
                    .post(&url)
                    .body(prompt)
                    .send()
                    .await?;
                if !resp.status().is_success() {
                    anyhow::bail!("summarizer endpoint {}", resp.status());
                }
                Ok(resp.text().await?)
            })
        });

        let ws = gauntlet_workspace();
        let workspace = ws.path().to_string_lossy().to_string();
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("a summarizer failure must never abort the turn");

        assert_eq!(reply, "completed despite summarizer outage");
        assert!(
            attempts.load(Ordering::SeqCst) >= 1,
            "the summarizer endpoint must have been attempted"
        );
        assert!(
            static_in_marker.load(Ordering::SeqCst),
            "the static fallback marker must reach the model"
        );
        assert!(task_in_marker.load(Ordering::SeqCst), "task still anchored");
        assert!(!old_placeholder.load(Ordering::SeqCst));
    }

    /// OpenAI-path mirror: the same pipeline serves the second loop — the
    /// marker + anchored task reach the post-compression request.
    struct OpenAiGauntletResponder {
        final_answer: String,
        task_in_marker_request: Arc<AtomicBool>,
        summary_in_marker_request: Arc<AtomicBool>,
    }

    impl Respond for OpenAiGauntletResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if messages_contain(&body, SUMMARY_PREFIX) {
                if messages_contain(&body, TASK) {
                    self.task_in_marker_request.store(true, Ordering::SeqCst);
                }
                if messages_contain(&body, CANNED_SUMMARY) {
                    self.summary_in_marker_request.store(true, Ordering::SeqCst);
                }
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": self.final_answer } }]
                }));
            }
            if body.get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "read_file", "arguments": "{\"path\":\"big.txt\"}" }
                        }]
                    }}]
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{ "message": { "content": "cap exit" } }]
                }))
            }
        }
    }

    #[tokio::test]
    async fn openai_loop_compresses_with_the_same_pipeline() {
        let server = MockServer::start().await;
        let task_in_marker = Arc::new(AtomicBool::new(false));
        let summary_in_marker = Arc::new(AtomicBool::new(false));
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiGauntletResponder {
                final_answer: "openai: marker is GAUNTLET-7f3d9c".into(),
                task_in_marker_request: task_in_marker.clone(),
                summary_in_marker_request: summary_in_marker.clone(),
            })
            .mount(&server)
            .await;

        let ws = gauntlet_workspace();
        let workspace = ws.path().to_string_lossy().to_string();
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let summarizer = canned_summarizer(prompts.clone());
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        c.kind = BackendKind::Openai;
        c.api_key = Some("sk-test");
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("openai loop should succeed");

        assert_eq!(reply, "openai: marker is GAUNTLET-7f3d9c");
        assert!(!prompts.lock().unwrap().is_empty(), "summarizer engaged");
        assert!(task_in_marker.load(Ordering::SeqCst));
        assert!(summary_in_marker.load(Ordering::SeqCst));
    }

    // -----------------------------------------------------------------------
    // Multi-compression long-haul regressions (review of PR #267, F1/F2/N3):
    // the original suite never exercised a SECOND compression — the gap that
    // let the self-poisoning boundary bug through.
    // -----------------------------------------------------------------------

    /// Per-request long-haul observations: `(dispatched message count,
    /// length of the last tool-role message — the freshest result)`.
    type HaulLog = Arc<Mutex<Vec<(usize, Option<usize>)>>>;

    /// Endless-work responder: each round calls a (hallucinated) write-ish
    /// tool and then `read_file` of `path` while tools are offered (the loop
    /// runs to its round cap), answering only the cap-exit tools-disabled
    /// completion. The non-read-only call keeps the loop's read-only nudge
    /// quiet — no intervening user messages, the regime the reviewer's F1
    /// traces locked up in (a periodic nudge would hand the boundary a fresh
    /// anchor and mask the bug). Logs, per tool-offering request, the
    /// dispatched message count and the length of the LAST tool-role message
    /// — the freshest result the model is about to read.
    struct LongHaulResponder {
        path: &'static str,
        log: HaulLog,
    }

    impl Respond for LongHaulResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if body.get("tools").is_some() {
                let empty = Vec::new();
                let msgs = body["messages"].as_array().unwrap_or(&empty);
                let last_tool_len = msgs
                    .iter()
                    .rev()
                    .find(|m| m["role"].as_str() == Some("tool"))
                    .and_then(|m| m["content"].as_str())
                    .map(|c| c.chars().count());
                self.log.lock().unwrap().push((msgs.len(), last_tool_len));
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "", "tool_calls": [
                        { "function": { "name": "apply_patch", "arguments": {} } },
                        { "function": { "name": "read_file", "arguments": { "path": self.path } } }
                    ]}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "message": { "content": "long haul done" } }),
                )
            }
        }
    }

    /// Three prior turns, then the active task — the reviewer's multi-turn
    /// shape (the last REAL user message sits deep before the tool rounds).
    fn multi_turn_msgs() -> Vec<MemMessage> {
        vec![
            MemMessage::system("you are a test"),
            MemMessage::user("prior turn: inspect the workspace"),
            MemMessage::assistant("inspected — looks healthy"),
            MemMessage::user("prior turn: run the linters"),
            MemMessage::assistant("linters are green"),
            MemMessage::user("prior turn: sketch a fix"),
            MemMessage::assistant("sketched in my head"),
            MemMessage::user(TASK),
        ]
    }

    /// Drive `rounds` tool rounds under count-only compression pressure.
    /// Returns `(per-request log, summarizer invocations, reply, latched)`.
    async fn run_long_haul(
        mem_messages: Vec<MemMessage>,
        rounds: usize,
        threshold: usize,
        file: &'static str,
        content: &str,
    ) -> (Vec<(usize, Option<usize>)>, usize, String, bool) {
        let server = MockServer::start().await;
        let log = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(LongHaulResponder {
                path: file,
                log: log.clone(),
            })
            .mount(&server)
            .await;
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join(file), content).unwrap();
        let workspace = ws.path().to_string_lossy().to_string();
        let caveats = Caveats::top();
        let uri = server.uri();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let summarizer = canned_summarizer(prompts.clone());
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &mem_messages, &caveats, &workspace);
        c.max_tool_rounds = rounds;
        c.mid_loop_trim_threshold = threshold;
        c.mid_loop_trim_tokens = None; // count-only: the F1/F2 regime
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("the long haul must complete");
        let log = log.lock().unwrap().clone();
        let calls = prompts.lock().unwrap().len();
        (log, calls, reply, compress_state.is_disabled())
    }

    /// F1 regression (i) — the reviewer's single-turn trace: 4 KB read_file
    /// results to round 40 under the count-only trigger. Pre-fix, the second
    /// compression anchored on its own summary: the count never shrank again
    /// (65 messages at round 40), the summarizer re-fired per round, and the
    /// fresh 4 KB result was one-lined before every dispatch from round ~20
    /// — the model could never read anything for the rest of the turn.
    #[tokio::test]
    async fn forty_rounds_single_turn_stay_bounded_with_fresh_results_intact() {
        let line = "the quick brown newt compresses context without discarding it\n";
        let threshold = 15usize;
        let (log, summarizer_calls, reply, latched) =
            run_long_haul(msgs(), 40, threshold, "big.txt", &line.repeat(64)).await;

        assert_eq!(reply, "long haul done");
        assert!(!latched, "count-only pressure must never latch anti-thrash");
        assert_eq!(log.len(), 40, "all 40 tool rounds dispatched");
        for (round, (len, last_tool)) in log.iter().enumerate() {
            assert!(
                *len <= threshold + 6,
                "round {round}: dispatched {len} messages — the count must stay \
                 bounded after every compression (threshold {threshold} + slack)"
            );
            if let Some(n) = last_tool {
                assert!(
                    *n > 1_000,
                    "round {round}: the fresh tool result was destroyed before \
                     dispatch ({n} chars — a one-liner)"
                );
            }
        }
        assert!(summarizer_calls >= 2, "the long haul compresses repeatedly");
        assert!(
            summarizer_calls <= 16,
            "summarizer invocations must be bounded, not per-round \
             (got {summarizer_calls} in 40 rounds)"
        );
        let max_len = log.iter().map(|(l, _)| *l).max().unwrap();
        println!(
            "forty-round trace: max dispatched len {max_len}, \
             summarizer calls {summarizer_calls}"
        );
    }

    /// F1 regression (ii) — the reviewer's multi-turn trace: 3 prior turns,
    /// then 30 tool rounds. Pre-fix the regime locked in at round ~9 (the
    /// anchor pinned the current task, the middle shrank to the previous
    /// summary alone, nothing could ever shrink) and the summarizer re-ran
    /// almost every round (71 invocations in 80 rounds).
    #[tokio::test]
    async fn thirty_rounds_multi_turn_stay_bounded_with_fresh_results_intact() {
        let line = "the quick brown newt compresses context without discarding it\n";
        let threshold = 15usize;
        let (log, summarizer_calls, reply, latched) = run_long_haul(
            multi_turn_msgs(),
            30,
            threshold,
            "big.txt",
            &line.repeat(64),
        )
        .await;

        assert_eq!(reply, "long haul done");
        assert!(!latched, "count-only pressure must never latch anti-thrash");
        assert_eq!(log.len(), 30, "all 30 tool rounds dispatched");
        for (round, (len, last_tool)) in log.iter().enumerate() {
            assert!(
                *len <= threshold + 6,
                "round {round}: dispatched {len} messages — bounded"
            );
            if let Some(n) = last_tool {
                assert!(
                    *n > 1_000,
                    "round {round}: fresh tool result destroyed pre-dispatch ({n} chars)"
                );
            }
        }
        assert!(summarizer_calls >= 2);
        assert!(
            summarizer_calls <= 14,
            "summarizer invocations must be bounded, not per-round \
             (got {summarizer_calls} in 30 rounds)"
        );
        let max_len = log.iter().map(|(l, _)| *l).max().unwrap();
        println!(
            "thirty-round multi-turn trace: max dispatched len {max_len}, \
             summarizer calls {summarizer_calls}"
        );
    }

    /// F2 regression — the reviewer's 600-char-results multi-turn shape:
    /// count-only compressions whose per-pass reclaim is small must neither
    /// latch anti-thrash (silently killing the VRAM guard) nor escalate to
    /// the Refused bail — pre-fix this errored the whole turn at round 25
    /// with "context exceeds the model's input budget" while the actual
    /// context was ~3-5k tokens and NO token threshold was configured.
    #[tokio::test]
    async fn count_only_low_reclaim_never_latches_or_bails() {
        let (log, _calls, reply, latched) =
            run_long_haul(multi_turn_msgs(), 30, 15, "small.txt", &"x".repeat(600)).await;
        assert_eq!(reply, "long haul done", "the turn must complete — no bail");
        assert!(
            !latched,
            "count-only passes must never latch the disable switch"
        );
        assert_eq!(log.len(), 30, "all 30 rounds ran (no Refused early-exit)");
    }

    // -----------------------------------------------------------------------
    // Trailing-group long-hauls (#270 / #285): the read-only-nudge regime the
    // #267 re-verifier flagged as untested, and the oversized-single-round
    // residual #284's gauntlet measured.
    // -----------------------------------------------------------------------

    /// Per-request observations for the nudged haul: `(message count, nudge
    /// text present, per-result content lengths of the trailing tool group)`.
    type NudgedLog = Arc<Mutex<Vec<(usize, bool, Vec<usize>)>>>;

    /// Read-only-only responder: every round reads big1 + big2 + small (three
    /// read-only calls, no writes), so the loop's read-only nudge fires every
    /// few rounds — the regime #270's probe ran in. The #267 long-haul
    /// responder deliberately added a write-ish call to keep that nudge quiet
    /// and hand the boundary no fresh anchors; this one deliberately does the
    /// opposite. Logs, per tool-offering request, the content length of every
    /// tool result in the trailing group (everything after the last
    /// assistant-with-`tool_calls`).
    struct NudgedHaulResponder {
        log: NudgedLog,
    }

    impl Respond for NudgedHaulResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            if body.get("tools").is_some() {
                let empty = Vec::new();
                let msgs = body["messages"].as_array().unwrap_or(&empty);
                let group_start = msgs
                    .iter()
                    .rposition(|m| m["tool_calls"].as_array().is_some_and(|t| !t.is_empty()))
                    .map(|i| i + 1)
                    .unwrap_or(msgs.len());
                let group_lens: Vec<usize> = msgs[group_start..]
                    .iter()
                    .filter(|m| m["role"].as_str() == Some("tool"))
                    .filter_map(|m| m["content"].as_str())
                    .map(|c| c.chars().count())
                    .collect();
                let nudged = messages_contain(&body, "read-only rounds so far");
                self.log
                    .lock()
                    .unwrap()
                    .push((msgs.len(), nudged, group_lens));
                // `role` present like a real Ollama reply — the loop appends
                // this object verbatim and the prune pairing reads the role.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "role": "assistant", "content": "", "tool_calls": [
                        { "function": { "name": "read_file", "arguments": { "path": "big1.txt" } } },
                        { "function": { "name": "read_file", "arguments": { "path": "big2.txt" } } },
                        { "function": { "name": "read_file", "arguments": { "path": "small.txt" } } }
                    ]}
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "message": { "content": "nudged haul done" } }),
                )
            }
        }
    }

    /// #270 e2e — the test the #267 re-verifier said was missing: a
    /// nudge-active long session (read-only rounds only, so the loop injects
    /// its stop-exploring user message right before the compression call
    /// site every few rounds) under a hard token trigger. Pre-fix, the
    /// nudge zeroed the trailing `role == "tool"` count, `keep_last`
    /// floored at 2, and BOTH big fresh results were one-lined pre-dispatch
    /// on every nudge round. Post-fix the group derives from the assistant
    /// turn that issued the calls: the newest member is always whole and
    /// the middle member survives every round (#285's within-group reclaim
    /// may one-line only the oldest, oldest-first, and only while over
    /// budget).
    #[tokio::test]
    async fn nudged_long_haul_keeps_fresh_group_results_intact() {
        let server = MockServer::start().await;
        let log: NudgedLog = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(NudgedHaulResponder { log: log.clone() })
            .mount(&server)
            .await;
        // Distinct contents per file: identical results would engage the
        // dedupe pass, which is not what this test pins.
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(
            ws.path().join("big1.txt"),
            "the first big fixture keeps unseen results intact\n".repeat(200),
        )
        .unwrap();
        std::fs::write(
            ws.path().join("big2.txt"),
            "the second big fixture must survive every nudge round\n".repeat(200),
        )
        .unwrap();
        std::fs::write(
            ws.path().join("small.txt"),
            "the small newest fixture stays whole\n".repeat(5),
        )
        .unwrap();
        let workspace = ws.path().to_string_lossy().to_string();
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let summarizer = canned_summarizer(prompts.clone());
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        c.max_tool_rounds = 12;
        c.mid_loop_trim_tokens = Some(4_000); // hard trigger, fires most rounds
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("the nudged haul must complete");

        assert_eq!(reply, "nudged haul done");
        assert!(
            !compress_state.is_disabled(),
            "real reclaims every round must never latch anti-thrash"
        );
        let log = log.lock().unwrap();
        for (round, entry) in log.iter().enumerate() {
            println!("nudged haul round {round}: {entry:?}");
        }
        assert_eq!(log.len(), 12, "all 12 tool rounds dispatched");
        let nudged_rounds = log.iter().filter(|(_, n, _)| *n).count();
        assert!(
            nudged_rounds >= 2,
            "the read-only nudge regime must actually be active \
             (got {nudged_rounds} nudged requests)"
        );
        for (round, (len, nudged, group_lens)) in log.iter().enumerate() {
            if group_lens.is_empty() {
                continue; // round 0: no tool group yet
            }
            assert_eq!(group_lens.len(), 3, "round {round}: pairing intact");
            // The newest member is ALWAYS whole.
            assert!(
                group_lens[2] > 150,
                "round {round} (nudged={nudged}): the newest fresh result \
                 was destroyed pre-dispatch ({} chars)",
                group_lens[2]
            );
            // The middle member survives too: within-group reclaim is
            // oldest-first and stops at the budget — pre-#270 every nudge
            // round one-lined it ({len} msgs dispatched).
            assert!(
                group_lens[1] > 1_000,
                "round {round} (nudged={nudged}, {len} msgs): the middle \
                 fresh result was destroyed pre-dispatch ({} chars)",
                group_lens[1]
            );
        }
        let max_tokens = log.iter().map(|(l, _, _)| *l).max().unwrap();
        println!(
            "nudged haul trace: {nudged_rounds} nudged rounds, \
             max dispatched len {max_tokens}, group lens e.g. {:?}",
            log.last().unwrap().2
        );
    }

    /// Per-request observations for the oversized-round haul: `(est message
    /// tokens, a one-lined, b one-lined, c payload intact, task present)`.
    type OversizedLog = Arc<Mutex<Vec<(usize, bool, bool, bool, bool)>>>;

    /// One round reads three files whose results TOGETHER exceed the model
    /// window; answers as soon as the dispatch shows a.txt one-lined.
    struct OversizedRoundResponder {
        log: OversizedLog,
    }

    impl Respond for OversizedRoundResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body = body_json(req);
            let a_onelined = messages_contain(&body, "[read_file] read 'a.txt'");
            if !body["stream"].as_bool().unwrap_or(false) {
                self.log.lock().unwrap().push((
                    body_message_tokens(&body),
                    a_onelined,
                    messages_contain(&body, "[read_file] read 'b.txt'"),
                    messages_contain(&body, "NEWEST-PAYLOAD-C-INTACT"),
                    messages_contain(&body, TASK),
                ));
            }
            if a_onelined {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "content": "the three files are summarized" }
                }));
            }
            if body.get("tools").is_some() {
                // `role` present like a real Ollama reply — the prune
                // pairing needs it to name the file in each one-liner.
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": { "role": "assistant", "content": "", "tool_calls": [
                        { "function": { "name": "read_file", "arguments": { "path": "a.txt" } } },
                        { "function": { "name": "read_file", "arguments": { "path": "b.txt" } } },
                        { "function": { "name": "read_file", "arguments": { "path": "c.txt" } } }
                    ]}
                }))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "message": { "content": "cap exit" } }))
            }
        }
    }

    /// #285 e2e — the B6 residual measured in #284's gauntlet: ONE round's
    /// tool group alone exceeds the `num_ctx` ceiling (the only budget in
    /// play on a fresh capability cache, #282/#284 wiring untouched).
    /// Pre-fix the F1c protection made the group unreclaimable: compression
    /// honestly reported "still over budget" and the dispatch shipped
    /// over-window into a silent backend truncation. Post-fix the dispatch
    /// fits the ceiling: a.txt / b.txt one-lined (each naming its file for
    /// re-read), c.txt — the newest — intact, the task still present, and
    /// the model returns the real answer.
    #[tokio::test]
    async fn oversized_single_round_dispatches_within_the_window() {
        let server = MockServer::start().await;
        let log: OversizedLog = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(OversizedRoundResponder { log: log.clone() })
            .mount(&server)
            .await;
        // Three ~7 KB results: together ~5.3k est tokens against a 4,096
        // num_ctx (3,276-token input ceiling) — the trailing group ALONE
        // exceeds the window; no boundary can split it (B6's shape).
        // Distinct contents per file: identical results would engage the
        // dedupe pass, which is not what this test pins.
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(
            ws.path().join("a.txt"),
            "fixture a arrives first in the one giant trailing group\n".repeat(125),
        )
        .unwrap();
        std::fs::write(
            ws.path().join("b.txt"),
            "fixture b arrives second and is older than the newest\n".repeat(130),
        )
        .unwrap();
        std::fs::write(
            ws.path().join("c.txt"),
            format!(
                "{}NEWEST-PAYLOAD-C-INTACT\n",
                "fixture c arrives last and must reach the model whole\n".repeat(130)
            ),
        )
        .unwrap();
        let workspace = ws.path().to_string_lossy().to_string();
        let messages = msgs();
        let caveats = Caveats::top();
        let uri = server.uri();
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let summarizer = canned_summarizer(prompts.clone());
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        // Fresh capability cache, no token threshold: the num_ctx the loop
        // itself sends is the only ceiling (#284's regime, untouched).
        c.max_ok_input = None;
        c.safe_context = None;
        c.mid_loop_trim_tokens = None;
        c.num_ctx = Some(4_096);
        c.summarizer = Some(&*summarizer);
        c.compress_state = Some(&mut compress_state);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("the oversized round must complete");

        let log = log.lock().unwrap();
        for (i, entry) in log.iter().enumerate() {
            println!("oversized round request {i}: {entry:?}");
        }
        // Never a silent wrong answer: the model answered from real data.
        assert_eq!(reply, "the three files are summarized");

        // THE #285 property: no dispatch ships over the window. The newest
        // result alone fits the ceiling here, so there is no truthful
        // still-over residual to excuse an oversized request — pre-fix the
        // round-1 dispatch went out at ~5.5k est tokens against 3,276.
        for (i, &(tokens, ..)) in log.iter().enumerate() {
            assert!(
                tokens <= 3_276,
                "request {i} dispatched over the window: ~{tokens} est. \
                 message tokens > 3,276 (the pre-#285 B6 residual)"
            );
        }

        let (tokens, _, b_onelined, c_intact, task_present) = *log
            .iter()
            .find(|(_, a, ..)| *a)
            .expect("a dispatch with a.txt one-lined must have happened");
        assert!(
            b_onelined,
            "older members one-lined in order: b.txt one-liner missing"
        );
        assert!(
            c_intact,
            "#285: the NEWEST result must reach the model whole"
        );
        assert!(task_present, "the task survives the within-group reclaim");
        // The dispatch fits the same input ceiling the #284 test pins:
        // 80% of 4,096 = 3,276 estimated message tokens.
        assert!(
            tokens <= 3_276,
            "#285: the reclaimed dispatch must fit the window \
             (got ~{tokens} est. message tokens > 3,276)"
        );
        println!(
            "#285 e2e trace: reclaimed dispatch ~{tokens} est. tokens \
             (ceiling 3,276), a/b one-lined, c intact"
        );
    }

    /// N3 — the loop-level refusal path end-to-end: a HARD token trigger
    /// (`mid_loop_trim_tokens`) over a genuinely incompressible context
    /// records two poor passes, latches anti-thrash, and the next
    /// over-budget round refuses the dispatch: `chat_complete` errors with
    /// the named message. Post-F2 only token-over-budget sessions can reach
    /// this — the bail text is now truthful.
    #[tokio::test]
    async fn hard_budget_thrash_latches_then_bails_with_named_error() {
        let server = MockServer::start().await;
        let log = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(LongHaulResponder {
                path: "tiny.txt",
                log: log.clone(),
            })
            .mount(&server)
            .await;
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("tiny.txt"), "ok").unwrap();
        let workspace = ws.path().to_string_lossy().to_string();
        // Incompressible: the system prompt dominates and no structural
        // pass or boundary can reduce it.
        let messages = vec![
            MemMessage::system(format!("you are a test. {}", "rule. ".repeat(700))),
            MemMessage::user(TASK),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        c.mid_loop_trim_tokens = Some(50); // hard trigger, unreachable budget
        c.compress_state = Some(&mut compress_state);
        let err = chat_complete(c, &mut NoMcp)
            .await
            .expect_err("the post-latch over-budget round must refuse the send");
        let msg = err.to_string();
        assert!(msg.contains("exceeds the model's input budget"), "{msg}");
        assert!(msg.contains("auto-compression is disabled"), "{msg}");
        assert!(compress_state.is_disabled(), "anti-thrash latched first");
        assert!(
            log.lock().unwrap().len() <= 3,
            "the refusal must stop the loop within a round or two"
        );
    }

    /// Step 20.3 — the loop-level fail-open path (the gpt-4.1 bug). Same
    /// incompressible over-budget shape as the bail test, but the budget rests
    /// on the proven-good high-water mark ALONE (`max_ok_input`, no
    /// `safe_context`, no `num_ctx`, no token threshold) — the cloud /
    /// no-`/api/show` case. Anti-thrash still latches, but the latched
    /// over-budget rounds must NOT refuse: refusing here is the death spiral
    /// the user hit. The loop keeps dispatching (fails open) and never bails
    /// with the named error.
    #[tokio::test]
    async fn lone_hwm_budget_fails_open_and_does_not_bail() {
        let server = MockServer::start().await;
        let log = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(LongHaulResponder {
                path: "tiny.txt",
                log: log.clone(),
            })
            .mount(&server)
            .await;
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::write(ws.path().join("tiny.txt"), "ok").unwrap();
        let workspace = ws.path().to_string_lossy().to_string();
        let messages = vec![
            MemMessage::system(format!("you are a test. {}", "rule. ".repeat(700))),
            MemMessage::user(TASK),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut compress_state = CompressState::new();
        let mut c = ctx(&uri, &messages, &caveats, &workspace);
        // The cloud shape: a starved proven-good HWM and NOTHING authoritative.
        c.mid_loop_trim_tokens = None;
        c.max_ok_input = Some(50); // largest prompt "seen" — a floor, not a cap
        c.safe_context = None; // no /api/show seed
        c.num_ctx = None; // no per-request window ceiling
        c.compress_state = Some(&mut compress_state);
        let result = chat_complete(c, &mut NoMcp).await;
        // The session must NOT die on the budget bail — it fails open.
        if let Err(e) = &result {
            let msg = e.to_string();
            assert!(
                !msg.contains("exceeds the model's input budget"),
                "a lone-HWM budget must never refuse the send: {msg}"
            );
        }
        assert!(
            compress_state.is_disabled(),
            "anti-thrash still latches on the poor passes"
        );
        assert!(
            log.lock().unwrap().len() > 3,
            "the loop kept dispatching past the latch instead of bailing"
        );
    }
}

// ---------------------------------------------------------------------------
// Per-round observation hook + mid-turn budget raise (Phase 20,
// docs/design/model-self-tuning.md §2.2) — wiremock e2e against both gates.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod observation_hook_tests {
    use super::*;
    use crate::caveats::Caveats;
    use crate::{BackendKind, MemMessage};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn ctx<'a>(
        server_uri: &'a str,
        messages: &'a [MemMessage],
        caveats: &'a Caveats,
    ) -> ChatCtx<'a> {
        ChatCtx {
            url: server_uri,
            model: "test-model",
            kind: BackendKind::Ollama,
            api_key: None,
            messages,
            task: "do the thing",
            workspace: ".",
            color: false,
            caveats,
            max_tool_rounds: 8,
            tool_output_lines: 20,
            debug: false,
            trace: false,
            num_ctx: None,
            connect_timeout_secs: 5,
            inference_timeout_secs: 30,
            mid_loop_trim_threshold: 40,
            mid_loop_trim_tokens: None,
            max_ok_input: None,
            build_check_cmd: None,
            safe_context: None,
            recover_cw_400: None,
            note_sink: None,
            note_nudge: None,
            recall_source: None,
            memory_source: None,
            summarizer: None,
            compress_state: None,
            tool_events: None,
            permission_gate: None,
            on_round_usage: None,
            estimate_ratio: None,
            // #307: test ChatCtx carries no preset exec floor (headless default).
            exec_floor: None,
            write_ledger: None,
            cancel: None,
            git_tool: None,
            crew_runner: None,
        }
    }

    fn body_json(req: &Request) -> serde_json::Value {
        serde_json::from_slice(&req.body).unwrap_or_default()
    }

    fn is_stream(req: &Request) -> bool {
        body_json(req)["stream"].as_bool().unwrap_or(false)
    }

    fn ndjson(lines: &[serde_json::Value]) -> ResponseTemplate {
        let body: String = lines
            .iter()
            .map(|l| format!("{l}\n"))
            .collect::<Vec<_>>()
            .join("");
        ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/x-ndjson")
    }

    /// Tool calls for the first two tools-offering requests (each reporting
    /// the backend ACCEPTED an 8,734-token prompt), then a final answer.
    struct AcceptsLargePrompts {
        tools_rounds: Arc<AtomicUsize>,
    }
    impl Respond for AcceptsLargePrompts {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                return ndjson(&[serde_json::json!({
                    "message": {"content": "budget raised, here is the answer"},
                    "done": true, "prompt_eval_count": 8_700, "eval_count": 12
                })]);
            }
            let n = self.tools_rounds.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "", "tool_calls": [{
                        "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                    }]},
                    "prompt_eval_count": 8_734, "eval_count": 10,
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "budget raised, here is the answer"},
                    "prompt_eval_count": 8_700, "eval_count": 12,
                }))
            }
        }
    }

    /// THE trace-class regression (the motivating failure): a poisoned-low
    /// `max_ok_input` (the largest prompt SEEN, not accepted) used to refuse
    /// sends the backend was happily evaluating. Now: the over-budget
    /// acceptance (a) reaches the caller as an `Accepted` observation with
    /// the backend's real prompt size, and (b) raises the in-turn send
    /// budget, so the turn completes instead of latching anti-thrash into
    /// the Refused bail across the following rounds.
    #[tokio::test]
    async fn poisoned_low_budget_recovers_via_accepted_observation_and_raise() {
        let server = MockServer::start().await;
        let tools_rounds = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(AcceptsLargePrompts {
                tools_rounds: tools_rounds.clone(),
            })
            .mount(&server)
            .await;

        // A task big enough (~12k chars ≈ 3k est. tokens) to sit over the
        // poisoned 2,000-token budget but far under what the backend accepts.
        let big_task = "study the workspace and report. ".repeat(380);
        let messages = vec![
            MemMessage::system("you are a test"),
            MemMessage::user(&big_task),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut observations: Vec<RoundObservation> = Vec::new();
        let mut hook = |obs: RoundObservation| observations.push(obs);
        let mut c = ctx(&uri, &messages, &caveats);
        c.max_ok_input = Some(2_000); // the poisoned ratchet
        c.on_round_usage = Some(&mut hook);
        let (reply, _streamed, _usage, _hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("the turn must complete — no Refused bail after the raise");

        assert_eq!(reply, "budget raised, here is the answer");
        assert!(
            observations.iter().any(|o| matches!(
                o,
                RoundObservation::Accepted {
                    prompt_tokens: 8_734,
                    ..
                }
            )),
            "the accepted 8,734-token prompt must reach the hook: {observations:?}"
        );
        // Every accepted round carried a non-zero chars/4 estimate for
        // calibration pairing.
        for o in &observations {
            if let RoundObservation::Accepted {
                estimated_tokens, ..
            } = o
            {
                assert!(*estimated_tokens > 0, "estimate rides along: {o:?}");
            }
        }
    }

    /// Always tool calls (with usage) — drives the anti-thrash latch under an
    /// unreachable hard token budget so the turn ends in the Refused Err.
    struct ToolCallsWithUsage;
    impl Respond for ToolCallsWithUsage {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if body_json(req).get("tools").is_some() {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "", "tool_calls": [{
                        "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                    }]},
                    "prompt_eval_count": 100, "eval_count": 5,
                }))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": "cap exit"}}))
            }
        }
    }

    /// A turn that ends `Err` (the Refused bail) STILL delivered the earlier
    /// rounds' `Accepted` observations first — evidence at the moment of
    /// observation, not in an epilogue the error skips (the spec's headline
    /// property). Also pins the bail's new tail: it names the model and the
    /// `newt tunings reset` escape hatch.
    #[tokio::test]
    async fn err_turn_still_delivered_accepted_observations_first() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ToolCallsWithUsage)
            .mount(&server)
            .await;

        // Incompressible context + unreachable hard token budget → two poor
        // passes latch anti-thrash, the next round refuses the send.
        let messages = vec![
            MemMessage::system(format!("you are a test. {}", "rule. ".repeat(700))),
            MemMessage::user("do the thing"),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut compress_state = CompressState::new();
        let mut observations: Vec<RoundObservation> = Vec::new();
        let mut hook = |obs: RoundObservation| observations.push(obs);
        let mut c = ctx(&uri, &messages, &caveats);
        c.mid_loop_trim_tokens = Some(50);
        c.compress_state = Some(&mut compress_state);
        c.on_round_usage = Some(&mut hook);
        let err = chat_complete(c, &mut NoMcp)
            .await
            .expect_err("the post-latch over-budget round must refuse the send");

        let msg = err.to_string();
        assert!(msg.contains("exceeds the model's input budget"), "{msg}");
        assert!(
            msg.contains("newt tunings reset test-model"),
            "the bail must name the model's reset command: {msg}"
        );
        assert!(
            observations.iter().any(|o| matches!(
                o,
                RoundObservation::Accepted {
                    prompt_tokens: 100,
                    ..
                }
            )),
            "accepted rounds before the bail must have been reported: {observations:?}"
        );
    }

    /// Probe 1: thinking-only (empty content, non-empty `thinking`, generated
    /// tokens); the corrective retry then recovers. The hook must see exactly
    /// one `ThinkingOnly` (once per turn) plus the recovery's `Accepted`.
    struct ThinkingOnlyThenRecover {
        probes: Arc<AtomicUsize>,
    }
    impl Respond for ThinkingOnlyThenRecover {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                if self.probes.load(Ordering::SeqCst) <= 1 {
                    ndjson(&[serde_json::json!({
                        "message": {"content": ""}, "done": true,
                        "prompt_eval_count": 9, "eval_count": 4
                    })])
                } else {
                    ndjson(&[serde_json::json!({
                        "message": {"content": "recovered after thinking-only"},
                        "done": true, "prompt_eval_count": 12, "eval_count": 3
                    })])
                }
            } else {
                let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": {
                            "content": "",
                            "thinking": "all reasoning, no final text"
                        },
                        "prompt_eval_count": 10, "eval_count": 2559,
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "message": {"content": "recovered after thinking-only"},
                        "prompt_eval_count": 12, "eval_count": 3,
                    }))
                }
            }
        }
    }

    #[tokio::test]
    async fn thinking_only_response_emits_one_thinking_only_observation() {
        let server = MockServer::start().await;
        let probes = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ThinkingOnlyThenRecover {
                probes: probes.clone(),
            })
            .mount(&server)
            .await;

        let messages = vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut observations: Vec<RoundObservation> = Vec::new();
        let mut hook = |obs: RoundObservation| observations.push(obs);
        let mut c = ctx(&uri, &messages, &caveats);
        c.on_round_usage = Some(&mut hook);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("the corrective retry recovers the turn");

        assert_eq!(reply, "recovered after thinking-only");
        let thinking = observations
            .iter()
            .filter(|o| matches!(o, RoundObservation::ThinkingOnly))
            .count();
        assert_eq!(thinking, 1, "exactly once per turn: {observations:?}");
        assert!(
            observations
                .iter()
                .any(|o| matches!(o, RoundObservation::Accepted { .. })),
            "the recovered round is usable output: {observations:?}"
        );
    }

    /// Tool round + final round both reporting a prompt at ≥95% of the
    /// request's `num_ctx` — Ollama may have silently dropped the head, so
    /// the rounds are window evidence of NOTHING: no `Accepted` observation,
    /// no budget raise.
    struct TruncationSuspectResponder {
        tools_rounds: Arc<AtomicUsize>,
    }
    impl Respond for TruncationSuspectResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if is_stream(req) {
                return ndjson(&[serde_json::json!({
                    "message": {"content": "suspect answer"}, "done": true,
                    "prompt_eval_count": 4_000, "eval_count": 5
                })]);
            }
            let n = self.tools_rounds.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "", "tool_calls": [{
                        "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                    }]},
                    // 4,000 ≥ 95% of 4,096 (3,891) — truncation suspect.
                    "prompt_eval_count": 4_000, "eval_count": 5,
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "suspect answer"},
                    "prompt_eval_count": 4_000, "eval_count": 5,
                }))
            }
        }
    }

    #[tokio::test]
    async fn truncation_suspect_rounds_emit_nothing() {
        let server = MockServer::start().await;
        let tools_rounds = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(TruncationSuspectResponder {
                tools_rounds: tools_rounds.clone(),
            })
            .mount(&server)
            .await;

        let messages = vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut observations: Vec<RoundObservation> = Vec::new();
        let mut hook = |obs: RoundObservation| observations.push(obs);
        let mut c = ctx(&uri, &messages, &caveats);
        c.num_ctx = Some(4_096);
        c.on_round_usage = Some(&mut hook);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("suspect rounds still complete the turn");

        assert_eq!(reply, "suspect answer");
        assert!(
            observations.is_empty(),
            "a possibly head-truncated prompt is evidence of nothing: \
             {observations:?}"
        );
    }

    /// OpenAI-path mirror: tool round then final content, both with usage —
    /// the hook receives `Accepted` for both (no `num_ctx` on this wire, so
    /// no truncation gate), and an absent hook stays a no-op.
    struct OpenAiAcceptsResponder;
    impl Respond for OpenAiAcceptsResponder {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            if body_json(req).get("tools").is_some()
                && !body_json(req)["messages"]
                    .as_array()
                    .map(|m| m.iter().any(|x| x["role"] == "tool"))
                    .unwrap_or(false)
            {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {
                        "content": null,
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "definitely_not_a_real_tool", "arguments": "{}"}
                        }]
                    }}],
                    "usage": {"prompt_tokens": 5_120, "completion_tokens": 9},
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": "openai accepted"}}],
                    "usage": {"prompt_tokens": 5_200, "completion_tokens": 11},
                }))
            }
        }
    }

    #[tokio::test]
    async fn openai_loop_reports_accepted_rounds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(OpenAiAcceptsResponder)
            .mount(&server)
            .await;

        let messages = vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut observations: Vec<RoundObservation> = Vec::new();
        let mut hook = |obs: RoundObservation| observations.push(obs);
        let mut c = ctx(&uri, &messages, &caveats);
        c.kind = BackendKind::Openai;
        c.api_key = Some("sk-test");
        c.on_round_usage = Some(&mut hook);
        let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
            .await
            .expect("openai loop should succeed");

        assert_eq!(reply, "openai accepted");
        let accepted: Vec<u32> = observations
            .iter()
            .filter_map(|o| match o {
                RoundObservation::Accepted { prompt_tokens, .. } => Some(*prompt_tokens),
                _ => None,
            })
            .collect();
        assert_eq!(
            accepted,
            vec![5_120, 5_200],
            "both usable rounds reported, in order: {observations:?}"
        );
    }

    /// Persistent empties (probe AND stream return empty content, no tool
    /// calls) at a prompt ≥85% of the configured `safe_context`, with no
    /// generated tokens — so the suspicious-empty corrective retry is NOT
    /// taken (that path needs `eval_count > 0`). The loop exhausts its two
    /// `overflow_retries`, then on the next persistent empty falls through to
    /// the silent-overflow exit and must emit exactly one
    /// `SuspectedOverflow { prompt_tokens }` carrying the merged (largest
    /// single) prompt size — the loop-emission seam that the dispatch-seam
    /// `record_overflow` tests at probe.rs cannot reach.
    struct PersistentEmptyOverflow;
    impl Respond for PersistentEmptyOverflow {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            if is_stream(_req) {
                // Stream re-issue: empty content, no tokens generated, but the
                // round still reports a large evaluated prompt.
                return ndjson(&[serde_json::json!({
                    "message": {"content": ""}, "done": true,
                    "prompt_eval_count": 8_734, "eval_count": 0
                })]);
            }
            // Probe (non-stream): empty content, no tool calls, no generated
            // tokens, large evaluated prompt.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": ""},
                "prompt_eval_count": 8_734, "eval_count": 0,
            }))
        }
    }

    #[tokio::test]
    async fn persistent_empty_over_safe_context_emits_suspected_overflow() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(PersistentEmptyOverflow)
            .mount(&server)
            .await;

        let messages = vec![
            MemMessage::system("you are a test"),
            MemMessage::user("do the thing"),
        ];
        let caveats = Caveats::top();
        let uri = server.uri();
        let mut observations: Vec<RoundObservation> = Vec::new();
        let mut hook = |obs: RoundObservation| observations.push(obs);
        let mut c = ctx(&uri, &messages, &caveats);
        // 8_734 ≥ 85% of 4_000 (3_400) → the silent-overflow gate fires.
        c.safe_context = Some(4_000);
        c.on_round_usage = Some(&mut hook);
        let (_reply, streamed, _usage, _hallu) = chat_complete(c, &mut NoMcp)
            .await
            .expect("persistent empties return the empty-response message, not Err");

        // Diagnostic exit returns non-streamed placeholder text.
        assert!(
            !streamed,
            "the silent-overflow exit is not a streamed reply"
        );
        // Exactly one SuspectedOverflow, carrying the merged (largest single)
        // prompt size — emitted once at the exit, never per retry.
        let overflow: Vec<u32> = observations
            .iter()
            .filter_map(|o| match o {
                RoundObservation::SuspectedOverflow { prompt_tokens } => Some(*prompt_tokens),
                _ => None,
            })
            .collect();
        assert_eq!(
            overflow,
            vec![8_734],
            "one SuspectedOverflow at the merged prompt size: {observations:?}"
        );
        // No Accepted: empty content is never usable output, so the window
        // evidence must not ratchet a success.
        assert!(
            !observations
                .iter()
                .any(|o| matches!(o, RoundObservation::Accepted { .. })),
            "empty rounds are not Accepted evidence: {observations:?}"
        );
    }
}
