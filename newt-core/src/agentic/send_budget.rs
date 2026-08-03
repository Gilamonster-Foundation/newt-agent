use super::RoundObservation;

/// Authoritative input-token ceiling implied by a declared context window.
///
/// A backend's context window contains both input and generated output. The
/// usable input is therefore the tighter of the configured percentage ceiling
/// and the space left after reserving the request's maximum output. `None`
/// means the endpoint's window is unknown. A known window that leaves zero
/// input capacity deliberately returns `Some(0)`: erasing it would turn an
/// impossible request into a fail-open dispatch.
pub(super) fn num_ctx_input_ceiling(
    num_ctx: Option<u32>,
    input_ceiling_pct: u32,
    max_output_tokens: Option<u32>,
) -> Option<usize> {
    num_ctx.map(|context_window| {
        let percentage_ceiling =
            crate::config::input_percentage_ceiling(context_window, input_ceiling_pct) as usize;
        let output_reserved = context_window.saturating_sub(max_output_tokens.unwrap_or(0));
        percentage_ceiling.min(output_reserved as usize)
    })
}

/// Resolve the initial input budget a caller should report for one backend
/// turn. This is the public reporting seam for the same percentage, cognition
/// output reserve, and cached-cap composition used by the dispatch loops.
///
/// Only explicitly capable Chat Completions endpoints receive Newt's local
/// generation policy (its output reserve). Responses reserves no local output
/// (it sends no `max_output_tokens` on this wire) so `max_output_tokens` stays
/// `None` for it — but the CONFIGURED context window is still a **local safety
/// limit** (#1526, invariant #4): a window the operator declared bounds the
/// input ceiling even though the Responses wire does not carry `num_ctx`.
/// Ceiling-from-an-unsent-value is deliberate here — the alternative is an
/// over-window request that only a reactive 400 (or a silent truncation) can
/// catch. Ollama and embedded backends keep the percentage-only local ceiling.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn initial_context_input_budget(
    kind: crate::BackendKind,
    api: crate::OpenAiApi,
    context_window: Option<u32>,
    input_ceiling_pct: u32,
    cognition: Option<crate::role_profile::Cognition>,
    chat_capability: crate::model_card::ChatCompletionsCapability,
    reasoning_replay_scope: crate::model_card::ReasoningReplayScope,
    max_ok_input: Option<u32>,
    safe_context: Option<u32>,
) -> Option<u32> {
    let max_output_tokens =
        if kind == crate::BackendKind::Openai && api == crate::OpenAiApi::ChatCompletions {
            super::generation_policy::GenerationPolicy::resolve(
                cognition,
                chat_capability,
                reasoning_replay_scope,
            )
            .max_output_tokens
        } else {
            None
        };
    // #1526 (invariant #4): the configured context window is a LOCAL safety
    // limit for every backend, including Responses. Responses reserves no local
    // output (`max_output_tokens` is `None` above) and never sends `num_ctx` on
    // the wire, but a declared window must still bound the input ceiling so an
    // over-window request is caught pre-dispatch, not only by a reactive 400.
    let ceiling = num_ctx_input_ceiling(context_window, input_ceiling_pct, max_output_tokens);
    initial_send_budget(max_ok_input, safe_context, ceiling)
        .map(|budget| u32::try_from(budget).expect("input budgets originate as u32 values"))
}

/// Convert a context-window 400's parsed full window into the next input
/// budget, reusing the request's output reserve and retaining any tighter
/// operator-declared ceiling.
pub(super) fn recovered_input_budget(
    context_window: u32,
    input_ceiling_pct: u32,
    max_output_tokens: Option<u32>,
    declared_ceiling: Option<usize>,
) -> usize {
    let recovered =
        num_ctx_input_ceiling(Some(context_window), input_ceiling_pct, max_output_tokens)
            .expect("a known context window always resolves an input ceiling");
    declared_ceiling.map_or(recovered, |declared| recovered.min(declared))
}

/// Initial pre-send budget for one turn (issue #282; Phase 20 semantics per
/// `docs/design/model-self-tuning.md` §2.1): the empirically-cached figure is
/// `max(max_ok_input, safe_context)` composed, via `min`, with the
/// authoritative input ceiling resolved for the request.
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
/// The declared-window ceiling composition is unchanged: before #282 the budget
/// was the cached numbers alone — unset on a fresh capability cache until
/// the turn ENDS, so the first turn of a session had no effective ceiling
/// and a 41k-token request sailed into a forced 4,096 window with zero
/// compression events (the measured B6 failure: 8/10 silently wrong). The
/// ceiling is a real token budget: when it fires the trigger, `hard_budget`
/// semantics apply (consults + feeds anti-thrash).
pub(super) fn initial_send_budget(
    max_ok_input: Option<u32>,
    safe_context: Option<u32>,
    input_ceiling: Option<usize>,
) -> Option<usize> {
    let cached = match (max_ok_input, safe_context) {
        (Some(m), Some(s)) => Some(m.max(s) as usize),
        (m, s) => m.or(s).map(|c| c as usize),
    };
    match (cached, input_ceiling) {
        (Some(budget), Some(ceiling)) => Some(budget.min(ceiling)),
        (budget, ceiling) => budget.or(ceiling),
    }
}

/// Convert a chars/4 estimate into real (backend-reported) token space using
/// the learned per-model `estimate_ratio` (Phase 20,
/// `docs/design/model-self-tuning.md` §2.3). Ceiling: estimates must err on
/// the side of counting, never undercounting — the 18.1 rule.
pub(super) fn calibrate_up(est: usize, ratio: f32) -> usize {
    (est as f32 * ratio).ceil() as usize
}

/// Convert a real-token budget into estimate (chars/4) space — the currency
/// the compression pipeline measures and reclaims in (Phase 20 §2.3).
/// Floor: a tighter target is safer than a looser one.
pub(super) fn calibrate_down(real: usize, ratio: f32) -> usize {
    (real as f32 / ratio).floor() as usize
}

/// Sanitize a per-model `estimate_ratio` for one turn (Phase 20 §2.3): only
/// finite values inside the learning clamp [0.5, 3.0] are trusted; anything
/// else (absent, NaN, a corrupted cache entry) degrades to 1.0 — the
/// identity, i.e. exactly the pre-calibration behavior.
pub(super) fn sanitize_estimate_ratio(estimate_ratio: Option<f32>) -> f32 {
    estimate_ratio
        .filter(|r| r.is_finite() && (0.5..=3.0).contains(r))
        .unwrap_or(1.0)
}

/// Report one quality-gated [`RoundObservation::Accepted`] (Phase 20 §2.2).
/// Called only from usable-output control paths (tool calls or non-empty
/// content — the quality gate); skips when the prompt was truncation-suspect
/// (≥95% of the request's `num_ctx`, where Ollama may have silently dropped
/// the head) or when the backend reported no usage for the round.
pub(super) fn emit_accepted(
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

/// Report a numbered hard context-window rejection through the same
/// capability-observation owner that receives later accepted-round evidence.
pub(super) fn emit_context_window_400(
    hook: &mut Option<&mut dyn FnMut(RoundObservation)>,
    context_window: u32,
) {
    if let Some(hook) = hook.as_deref_mut() {
        hook(RoundObservation::ContextWindow400 { context_window });
    }
}

/// #1528: the single source of truth for the **Responses** loop's context
/// budget. It composes the existing pure helpers ([`num_ctx_input_ceiling`],
/// [`initial_send_budget`], [`recovered_input_budget`],
/// [`super::authoritative_request_budget`], [`super::exposure_budget_tokens`],
/// [`super::generation_policy::cognition_output_reserve`]) into ONE owner so the
/// Responses dispatch preflight, tool exposure, compaction target, cw-400
/// recovery, and `get_context_remaining` all read one derivation instead of the
/// seven scattered locals (`output_reserve`, `responses_input_ceiling`,
/// `send_budget`, `send_budget_authoritative`, `effective_input_ceiling`,
/// `authoritative_budget`, `tool_tokens_real`) they replaced — and so
/// `get_context_remaining` can no longer diverge from what the loop enforces.
///
/// SCOPE: the Responses loop only (`openai_responses_complete_with_prompt_and_
/// artifacts`). The Ollama (`mod.rs` ~1363) and Chat Completions (`mod.rs`
/// ~4887) loops rebuild the identical trio inline and differ ONLY in the
/// output-reserve argument to [`num_ctx_input_ceiling`] (`None` /
/// `generation_policy.max_output_tokens` / `cognition_output_reserve`); folding
/// those two into this struct is the sibling duplication this type is designed
/// to absorb next (one-issue-one-PR).
///
/// DISTINCT concepts, one lifetime each:
/// * the **seed hard ceiling** ([`num_ctx_input_ceiling`]) seeds
///   `learned_hard_ceiling`; the seed is a construction input, the monotone
///   learned ceiling is the retained state.
/// * `soft_send_budget` (the cached cap composed with the ceiling) and
///   `learned_hard_ceiling` are kept as SEPARATE fields — they diverge at
///   construction and only collapse after a cw-400 (fail-closed; see
///   [`Self::recover_from_cw400`]).
/// * `max_ok_input` and the authoritative flag are construction inputs consumed
///   into `soft_send_budget` / `preflight_budget`; the Responses path has no
///   accepted-side ceiling raise (deferred, `mod.rs` ~6253), so neither is
///   retained as a field.
pub(super) struct ResponsesBudgetState {
    /// Configured context window, echoed by `get_context_remaining`.
    num_ctx: Option<u32>,
    /// Configured percentage bound (`[context] input_ceiling_pct`).
    input_ceiling_pct: u32,
    /// Cognition output reserve (this wire sends no `max_output_tokens`, but the
    /// declared window must still leave room to generate). Reused when a cw-400
    /// recovers the full window into the next input cap.
    output_reserve: Option<u32>,
    /// Declared/believed-safe window; the tool-exposure fallback clip budget.
    safe_context: Option<u32>,
    /// Real-token schema overhead of the EXPOSED tool set (known only after
    /// exposure has run; see [`Self::set_tool_schema_tokens`]).
    tool_schema_tokens: usize,
    /// Monotone learned input ceiling: seeded from the declared-window hard
    /// ceiling and only ever `.min`-tightened by a recovered 400, never raised.
    learned_hard_ceiling: Option<usize>,
    /// Soft pre-send budget: `max(max_ok_input, safe_context)` composed with the
    /// hard ceiling via `min`. Distinct from `learned_hard_ceiling` until a
    /// cw-400 collapses the two.
    soft_send_budget: Option<usize>,
    /// The preflight refusal budget (the hard budget composed with the mid-loop
    /// trim threshold); `None` leaves the preflight a no-op.
    preflight_budget: Option<usize>,
    /// Proactive mid-loop trim threshold — kept as a DISTINCT field even though
    /// today it is consumed only by `preflight_budget`; a future proactive-
    /// compaction consumer (deferred, `mod.rs` ~6253) reads it directly.
    mid_loop_trim_tokens: Option<usize>,
}

impl ResponsesBudgetState {
    /// Compose the Responses budget from the declared window, the configured
    /// percentage bound, the cognition output reserve, and the cached-capability
    /// numbers. `num_ctx == None` (cloud Responses) yields NO ceiling — the
    /// budget stays ceiling-less exactly as before (invariant #1). An
    /// authoritative `Some(0)` ceiling (a window with no input room) is never
    /// erased (invariant #2): it flows through [`initial_send_budget`] unchanged.
    pub(super) fn new(
        num_ctx: Option<u32>,
        input_ceiling_pct: u32,
        cognition: Option<crate::role_profile::Cognition>,
        max_ok_input: Option<u32>,
        safe_context: Option<u32>,
        mid_loop_trim_tokens: Option<usize>,
    ) -> Self {
        let output_reserve = super::generation_policy::cognition_output_reserve(cognition);
        // Seed hard ceiling: min(pct% window, window − output reserve). `None`
        // when the window is unknown; `Some(0)` when no input fits (both
        // authoritative — never erased to fail open).
        let seed_ceiling = num_ctx_input_ceiling(num_ctx, input_ceiling_pct, output_reserve);
        let soft_send_budget = initial_send_budget(max_ok_input, safe_context, seed_ceiling);
        // A declared window is authoritative just like a cached `safe_context`.
        let authoritative = safe_context.is_some() || seed_ceiling.is_some();
        let preflight_budget = super::authoritative_request_budget(
            soft_send_budget,
            authoritative,
            mid_loop_trim_tokens,
        );
        Self {
            num_ctx,
            input_ceiling_pct,
            output_reserve,
            safe_context,
            tool_schema_tokens: 0,
            learned_hard_ceiling: seed_ceiling,
            soft_send_budget,
            preflight_budget,
            mid_loop_trim_tokens,
        }
    }

    /// Record the real-token schema overhead of the exposed tool set. Split from
    /// [`Self::new`] because it is known only after tool exposure runs, and it is
    /// consumed only by the (much later) cw-400 [`Self::compaction_budget`].
    pub(super) fn set_tool_schema_tokens(&mut self, tokens: usize) {
        self.tool_schema_tokens = tokens;
    }

    /// The configured window, echoed by `get_context_remaining`.
    pub(super) fn num_ctx(&self) -> Option<u32> {
        self.num_ctx
    }

    /// The configured percentage bound, echoed by `get_context_remaining`.
    pub(super) fn input_ceiling_pct(&self) -> u32 {
        self.input_ceiling_pct
    }

    /// The live tool-exposure clip budget: the soft send budget when known, else
    /// the declared `safe_context`. `None` means don't clip (no starvation
    /// without a measurement).
    pub(super) fn exposure_budget(&self) -> Option<usize> {
        super::exposure_budget_tokens(self.soft_send_budget, self.safe_context)
    }

    /// The authoritative preflight refusal budget every dispatch is checked
    /// against (per round, plus the tools-disabled final summary).
    pub(super) fn preflight_budget(&self) -> Option<usize> {
        self.preflight_budget
    }

    /// The soft send budget — the numberless cw-400 fallback recovers against it.
    pub(super) fn soft_send_budget(&self) -> Option<usize> {
        self.soft_send_budget
    }

    /// The ENFORCED learned input ceiling — the SAME value the loop enforces,
    /// which `get_context_remaining` now reports. This replaces the divergent
    /// `num_ctx_input_ceiling(num_ctx, pct, None)` recompute that advertised the
    /// looser percentage-only ceiling (26,214) while the loop enforced 16,768.
    pub(super) fn learned_hard_ceiling(&self) -> Option<usize> {
        self.learned_hard_ceiling
    }

    /// The input cap implied by a context-window 400's parsed full window,
    /// reusing this turn's output reserve and retaining any tighter learned
    /// ceiling. Pre-tighten — feed the result to [`Self::recover_from_cw400`].
    pub(super) fn recovered_budget_for_window(&self, context_window: u32) -> usize {
        recovered_input_budget(
            context_window,
            self.input_ceiling_pct,
            self.output_reserve,
            self.learned_hard_ceiling,
        )
    }

    /// Tighten the budget after a hard context-window 400. `recovered_budget` is
    /// the input cap implied by the endpoint's real limit (from
    /// [`Self::recovered_budget_for_window`] or a numberless fallback).
    ///
    /// MONOTONE (invariant #3): the learned ceiling only ever tightens — the new
    /// value is `min`ed against the current one, never re-derived upward from
    /// `num_ctx`. COLLAPSE (kept from the pre-refactor `mod.rs:6660`): the soft
    /// send budget is set equal to the new hard ceiling — the recovered window IS
    /// the new hard bound, so fail-closed they are one number here. Making them
    /// distinct-after-recovery is a SEPARATE change gated on the deferred
    /// proactive-threshold / accepted-raise consumers (not done here). Preflight
    /// is recomputed as authoritative.
    pub(super) fn recover_from_cw400(&mut self, recovered_budget: usize) {
        let new_budget = self
            .learned_hard_ceiling
            .map_or(recovered_budget, |ceiling| recovered_budget.min(ceiling));
        self.soft_send_budget = Some(new_budget);
        self.learned_hard_ceiling = Some(new_budget);
        self.preflight_budget = super::authoritative_request_budget(
            self.soft_send_budget,
            true,
            self.mid_loop_trim_tokens,
        );
    }

    /// The compaction target for the cw-400 recovery's `compress` call: the
    /// (now-collapsed) hard ceiling minus real-token schema overhead, converted
    /// back into the pipeline's chars/4 currency. Called only after
    /// [`Self::recover_from_cw400`] has set the ceiling.
    pub(super) fn compaction_budget(&self, calibration: f32) -> usize {
        let ceiling = self.learned_hard_ceiling.unwrap_or(0);
        calibrate_down(ceiling.saturating_sub(self.tool_schema_tokens), calibration)
    }
}

// Unit tests for the declared-window budget wiring: the effective ceiling
// composes with cached capability numbers via `min`, vanishes only when the
// window is unknown, and retains an authoritative zero when no input fits.
#[cfg(test)]
mod send_budget_tests {
    use super::super::compress::{compression_trigger, CompressionTriggerLimits};
    use super::{initial_send_budget, num_ctx_input_ceiling, recovered_input_budget};
    use crate::agentic::generation_policy::GenerationPolicy;
    use crate::model_card::{ChatCompletionsCapability, ReasoningReplayScope};
    use crate::role_profile::Cognition;
    use crate::{BackendKind, CompactionTriggerPolicy, OpenAiApi};

    #[test]
    fn responses_honors_the_configured_window_as_a_local_safety_limit() {
        // #1526 (invariant #4): a CONFIGURED context window is a local safety
        // limit for Responses even though the wire sends no `num_ctx`. Responses
        // reserves no local output, so the budget is the percentage ceiling of
        // the declared window — NOT `None` (the old, now-reversed contract).
        let ceiling = super::initial_context_input_budget(
            BackendKind::Openai,
            OpenAiApi::Responses,
            Some(32_768),
            80,
            Some(Cognition::Contemplating),
            ChatCompletionsCapability {
                cognition: Some(true),
                ..Default::default()
            },
            ReasoningReplayScope::CurrentUserTurn,
            None,
            None,
        );
        // 80% of 32_768, with no output reserve on this wire.
        assert_eq!(
            ceiling,
            Some(26_214),
            "a declared Responses window is a local refusal budget (invariant #4)"
        );
        // The cloud default (no configured window) still yields no local ceiling —
        // the change is opt-in via configuration and does not affect hosted OpenAI.
        assert_eq!(
            super::initial_context_input_budget(
                BackendKind::Openai,
                OpenAiApi::Responses,
                None,
                80,
                Some(Cognition::Contemplating),
                ChatCompletionsCapability {
                    cognition: Some(true),
                    ..Default::default()
                },
                ReasoningReplayScope::CurrentUserTurn,
                None,
                None,
            ),
            None,
            "an UNset num_ctx (cloud Responses) still has no local ceiling",
        );
    }

    /// The Chat Completions generation policy and input budget share one model
    /// window. These fixtures cover every cognition output allowance at the
    /// two local qualification windows from the Nemotron review.
    #[test]
    fn cognition_output_is_reserved_from_32k_and_65k_context_windows() {
        let capability = ChatCompletionsCapability {
            cognition: Some(true),
            ..Default::default()
        };
        let cases = [
            (Cognition::Glancing, 26_214, 52_428),
            (Cognition::Pondering, 26_214, 52_428),
            (Cognition::Deliberating, 22_768, 52_428),
            (Cognition::Contemplating, 16_768, 49_536),
        ];

        for (cognition, expected_32k, expected_65k) in cases {
            let policy =
                GenerationPolicy::resolve(Some(cognition), capability, ReasoningReplayScope::Never);
            assert_eq!(
                num_ctx_input_ceiling(Some(32_768), 80, policy.max_output_tokens),
                Some(expected_32k),
                "32K {cognition}"
            );
            assert_eq!(
                num_ctx_input_ceiling(Some(65_536), 80, policy.max_output_tokens),
                Some(expected_65k),
                "65K {cognition}"
            );
        }
    }

    #[test]
    fn recovered_full_window_reapplies_output_reserve_without_failing_open() {
        assert_eq!(
            recovered_input_budget(32_768, 80, Some(16_000), Some(49_536)),
            16_768,
            "a 32K recovered window reserves contemplating's 16K output"
        );
        assert_eq!(
            recovered_input_budget(8_000, 80, Some(10_000), Some(22_768)),
            0,
            "output consuming the recovered window remains an authoritative zero"
        );
    }

    /// THE B6 first-turn hole: a fresh capability cache (no `max_ok_input`,
    /// no `safe_context`) used to mean NO budget at all even though the
    /// request itself carried `options.num_ctx = 4096`. The ceiling must now
    /// arm the trigger on turn 1 — as a HARD budget (anti-thrash semantics).
    #[test]
    fn first_turn_fresh_cache_trigger_sees_the_num_ctx_ceiling() {
        let budget = initial_send_budget(None, None, num_ctx_input_ceiling(Some(4096), 80, None));
        assert_eq!(budget, Some(3276), "80% of 4096 — reply headroom reserved");
        // The measured B6 shape: ~41k estimated tokens, 3 messages, no
        // count/token thresholds in reach — pre-fix this returned None and
        // the request sailed into the 4k window with zero events.
        let trigger = compression_trigger(
            3,
            41_355,
            39_900,
            CompressionTriggerLimits {
                count_threshold: 40,
                token_threshold: None,
                send_budget: budget,
                tool_tokens: 1_432,
                policy: CompactionTriggerPolicy::HeadroomAware,
                has_authoritative_headroom: true,
            },
        )
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
            compression_trigger(
                3,
                41_355,
                39_900,
                CompressionTriggerLimits {
                    count_threshold: 40,
                    token_threshold: None,
                    send_budget: None,
                    tool_tokens: 1_432,
                    policy: CompactionTriggerPolicy::HeadroomAware,
                    has_authoritative_headroom: false,
                },
            ),
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
            CompressionTriggerLimits {
                count_threshold: 40,
                token_threshold: None,
                send_budget: Some(send_budget),
                tool_tokens: tool_tokens_real,
                policy: CompactionTriggerPolicy::HeadroomAware,
                has_authoritative_headroom: true,
            },
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
            initial_send_budget(
                Some(2_135),
                None,
                num_ctx_input_ceiling(Some(4_096), 80, None),
            ),
            Some(2_135)
        );
        // Ceiling tighter than the cached cap: the B6 shape — bootstrap
        // safe_context 104,857 vs forced num_ctx 4,096.
        assert_eq!(
            initial_send_budget(
                None,
                Some(104_857),
                num_ctx_input_ceiling(Some(4_096), 80, None),
            ),
            Some(3_276)
        );
        assert_eq!(
            initial_send_budget(
                Some(104_857),
                Some(104_857),
                num_ctx_input_ceiling(Some(4_096), 80, None),
            ),
            Some(3_276)
        );
    }

    /// A declared window with no room for input remains authoritative. `None`
    /// alone means unknown; zero must not erase the ceiling and fail open.
    #[test]
    fn zero_remaining_input_budget_does_not_fail_open() {
        assert_eq!(num_ctx_input_ceiling(None, 80, Some(16_000)), None);
        assert_eq!(num_ctx_input_ceiling(Some(0), 80, None), Some(0));
        assert_eq!(num_ctx_input_ceiling(Some(1), 80, None), Some(0));
        assert_eq!(
            num_ctx_input_ceiling(Some(4_096), 80, Some(4_096)),
            Some(0),
            "the output reserve consumes the whole known window"
        );
        assert_eq!(initial_send_budget(None, None, Some(0)), Some(0));
        assert_eq!(
            initial_send_budget(Some(2_000), None, Some(0)),
            Some(0),
            "an authoritative zero ceiling must shadow cached evidence"
        );
    }

    #[test]
    fn programmatic_percentage_values_use_config_normalization() {
        assert_eq!(num_ctx_input_ceiling(Some(10_000), 0, None), Some(8_000));
        assert_eq!(num_ctx_input_ceiling(Some(10_000), 100, None), Some(8_000));
        assert_eq!(
            num_ctx_input_ceiling(Some(10_000), u32::MAX, None),
            Some(8_000)
        );
    }

    // --- #1528 `ResponsesBudgetState`: one enforced budget, every reader ---

    /// THE numerical regression: with a 32K window at 80% and Contemplating
    /// (16K output reserve), the percentage ceiling is 26,214 but the ENFORCED
    /// hard ceiling is the tighter 16,768 — and EVERY Responses budget surface
    /// reports that same 16,768, closing the old `get_context_remaining`
    /// divergence by construction.
    #[test]
    fn responses_budget_state_reports_one_enforced_ceiling() {
        use super::ResponsesBudgetState;
        // The two legs of the ceiling: percentage-only (no reserve) vs
        // window-after-reserve. The authoritative ceiling is the tighter one.
        assert_eq!(num_ctx_input_ceiling(Some(32_768), 80, None), Some(26_214));
        assert_eq!(32_768 - 16_000, 16_768);

        let mut state = ResponsesBudgetState::new(
            Some(32_768),
            80,
            Some(Cognition::Contemplating),
            None,
            None,
            None,
        );
        state.set_tool_schema_tokens(0);

        // 1. dispatch preflight, 2. tool exposure, 3. get_context_remaining
        // ceiling, 4. cw-400 recovery seed — all the enforced 16,768, not 26,214.
        assert_eq!(state.preflight_budget(), Some(16_768), "dispatch preflight");
        assert_eq!(
            state.exposure_budget(),
            Some(16_768),
            "tool exposure clip budget"
        );
        assert_eq!(
            state.learned_hard_ceiling(),
            Some(16_768),
            "get_context_remaining ceiling"
        );
        assert_eq!(
            state.recovered_budget_for_window(32_768),
            16_768,
            "cw-400 recovery seed"
        );
        // 5. compaction target: recover into the collapsed ceiling, then the
        // chars/4 target (identity calibration, no schema overhead) is 16,768.
        let recovered = state.recovered_budget_for_window(32_768);
        state.recover_from_cw400(recovered);
        assert_eq!(state.compaction_budget(1.0), 16_768, "compaction target");
    }

    /// The `get_context_remaining` REGRESSION (#1528): the reported ceiling is
    /// the ENFORCED 16,768, never the old `num_ctx_input_ceiling(num_ctx, pct,
    /// None)` recompute (26,214). Rendered end to end, the report the model sees
    /// quotes the enforced headroom, not the un-reserved percentage ceiling.
    #[test]
    fn get_context_remaining_reports_enforced_not_percentage_ceiling() {
        use super::ResponsesBudgetState;
        let state = ResponsesBudgetState::new(
            Some(32_768),
            80,
            Some(Cognition::Contemplating),
            None,
            None,
            None,
        );
        // The value get_context_remaining now feeds to render_context_budget.
        assert_eq!(state.learned_hard_ceiling(), Some(16_768));
        // The pre-fix recompute (reserve = None) over-advertised — the divergence.
        assert_eq!(
            num_ctx_input_ceiling(state.num_ctx(), 80, None),
            Some(26_214)
        );
        assert_ne!(state.learned_hard_ceiling(), Some(26_214));
        let report = crate::agentic::budget::render_context_budget(
            0,
            state.learned_hard_ceiling(),
            state.num_ctx(),
            state.input_ceiling_pct(),
            15,
        );
        assert!(report.contains("ceiling of ~16768"), "{report}");
        assert!(
            !report.contains("26214"),
            "must not advertise the un-reserved ceiling: {report}"
        );
    }

    /// Monotone tighten-only (invariant #3): a recovered 400 can only lower the
    /// learned ceiling; a LATER larger recovery never raises it, and a huge
    /// recovered window is still clamped to the retained ceiling. The soft send
    /// budget collapses to the new hard ceiling (fail-closed) and preflight
    /// follows.
    #[test]
    fn learned_hard_ceiling_only_ever_tightens() {
        use super::ResponsesBudgetState;
        let mut state = ResponsesBudgetState::new(
            Some(32_768),
            80,
            Some(Cognition::Contemplating),
            None,
            None,
            None,
        );
        assert_eq!(state.learned_hard_ceiling(), Some(16_768));
        // A tighter recovery wins (a smaller learned ceiling always wins).
        state.recover_from_cw400(8_000);
        assert_eq!(state.learned_hard_ceiling(), Some(8_000));
        assert_eq!(
            state.soft_send_budget(),
            Some(8_000),
            "soft collapses to hard"
        );
        assert_eq!(state.preflight_budget(), Some(8_000));
        // A LATER larger recovery cannot raise it.
        state.recover_from_cw400(20_000);
        assert_eq!(
            state.learned_hard_ceiling(),
            Some(8_000),
            "a later value cannot raise the learned ceiling"
        );
        // Even a huge recovered window is clamped to the retained ceiling.
        assert_eq!(state.recovered_budget_for_window(1_000_000), 8_000);
    }

    /// Opt-in reserve: no cognition dial reserves nothing, so the ceiling is the
    /// plain percentage bound (26,214) and every reader agrees.
    #[test]
    fn no_cognition_reserve_reports_the_percentage_ceiling() {
        use super::ResponsesBudgetState;
        let state = ResponsesBudgetState::new(Some(32_768), 80, None, None, None, None);
        assert_eq!(state.learned_hard_ceiling(), Some(26_214));
        assert_eq!(state.preflight_budget(), Some(26_214));
        assert_eq!(state.exposure_budget(), Some(26_214));
    }

    /// Tightening the ceiling can only DECREASE reported remaining, and more
    /// instructions + tool-schema overhead (a larger `used`) further lowers it —
    /// the two directions that keep the reported budget honest.
    #[test]
    fn tightening_and_overhead_only_shrink_reported_remaining() {
        use super::ResponsesBudgetState;
        use crate::agentic::budget::render_context_budget;
        let mut state = ResponsesBudgetState::new(
            Some(32_768),
            80,
            Some(Cognition::Contemplating),
            None,
            None,
            None,
        );
        let render = |s: &ResponsesBudgetState, used: usize| {
            render_context_budget(
                used,
                s.learned_hard_ceiling(),
                s.num_ctx(),
                s.input_ceiling_pct(),
                15,
            )
        };
        // 16,768 ceiling, 4,000 used → 12,768 remaining.
        assert!(render(&state, 4_000).contains("12768 tokens remaining"));
        // More instructions + schema overhead (a higher `used`) lowers remaining.
        assert!(render(&state, 8_000).contains("8768 tokens remaining"));
        // Tightening the ceiling to 8,000 cannot report MORE remaining.
        state.recover_from_cw400(8_000);
        assert!(render(&state, 4_000).contains("4000 tokens remaining"));
    }

    /// Invariant #1 at the state level: `None` num_ctx (cloud Responses) yields
    /// NO ceiling — the state stays ceiling-less and every reader reports no
    /// bound, leaving hosted OpenAI unchanged.
    #[test]
    fn cloud_responses_none_num_ctx_stays_ceiling_less() {
        use super::ResponsesBudgetState;
        let state =
            ResponsesBudgetState::new(None, 80, Some(Cognition::Contemplating), None, None, None);
        assert_eq!(state.learned_hard_ceiling(), None, "no window → no ceiling");
        assert_eq!(state.preflight_budget(), None, "nothing to refuse against");
        assert_eq!(state.exposure_budget(), None, "no live budget → don't clip");
    }

    /// Invariant #2 at the state level: a window with no input room resolves to
    /// an authoritative `Some(0)` ceiling that is NEVER erased and shadows cached
    /// evidence — fail-closed, not fail-open.
    #[test]
    fn zero_input_room_window_stays_authoritative_zero() {
        use super::ResponsesBudgetState;
        // 80% of 16_000 = 12_800; window − 16_000 Contemplating reserve = 0 →
        // min = Some(0). A cached max_ok_input=2_000 must be shadowed, not win.
        let state = ResponsesBudgetState::new(
            Some(16_000),
            80,
            Some(Cognition::Contemplating),
            Some(2_000),
            None,
            None,
        );
        assert_eq!(
            state.learned_hard_ceiling(),
            Some(0),
            "no input room is an authoritative zero"
        );
        assert_eq!(
            state.soft_send_budget(),
            Some(0),
            "authoritative zero shadows cached evidence"
        );
        assert_eq!(state.preflight_budget(), Some(0));
    }
}
