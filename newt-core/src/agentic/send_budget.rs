//! Request accounting, admission, and input-budget state for agentic turns.

use super::trim::{
    estimate_request_tokens, estimate_tokens, estimate_value_tokens, protected_prompt_head_len,
};
use super::{budget, prompt_read, RoundObservation};

/// Tightest whole-request ceiling that carries authoritative semantics for
/// this turn. A proven-good high-water mark by itself is deliberately not a
/// ceiling; configured token thresholds and believed/declared windows are.
/// The LIVE usable input budget (in estimated tokens) the tool-exposure
/// controller sizes the schema set against — the initial send budget when known
/// (derived from probed `max_ok_input` / `safe_context` / `num_ctx`), else the
/// declared `safe_context`. `None` means no live signal: the controller then
/// does NOT clip (no starvation without a measurement). Deliberately not a
/// function of the model name (#TEC): a bigger probed window widens exposure
/// automatically.
pub(super) fn exposure_budget_tokens(
    send_budget: Option<usize>,
    safe_context: Option<u32>,
) -> Option<usize> {
    send_budget.or_else(|| safe_context.map(|s| s as usize))
}

pub(super) fn authoritative_request_budget(
    send_budget: Option<usize>,
    send_budget_authoritative: bool,
    token_threshold: Option<usize>,
) -> Option<usize> {
    let send = send_budget_authoritative.then_some(send_budget).flatten();
    match (send, token_threshold.filter(|budget| *budget > 0)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Whether the message-count fallback may stand down. A real ceiling always
/// delegates to the token/send guards. With no ceiling, a model-specific
/// accepted-prompt high-water mark still proves that the current request fits
/// when it is at or below that mark; treating that proof as no headroom causes
/// needless count-only compaction on hosted large-context models.
pub(super) fn count_guard_has_headroom(
    current_tokens: usize,
    authoritative_budget: Option<usize>,
    max_ok_input: Option<u32>,
) -> bool {
    authoritative_budget.is_some()
        || max_ok_input.is_some_and(|accepted| current_tokens <= accepted as usize)
}

pub(super) fn capped_accepted_prompt_tokens(
    accepted_prompt_tokens: u32,
    declared_ceiling: Option<usize>,
) -> usize {
    (accepted_prompt_tokens as usize).min(declared_ceiling.unwrap_or(usize::MAX))
}

/// Refuse before inference when the compression-immune system/card/exact-user
/// head, newest live user presentation, and advertised schemas cannot fit an
/// authoritative model budget. The live presentation intentionally remains at
/// the transcript tail so normal multi-turn ordering is preserved; counting
/// only the protected recovery copy would under-price every prompt by one full
/// copy and permit an over-window dispatch. Exact prompt text is never
/// truncated to manufacture a dispatchable request.
pub(super) fn preflight_irreducible_request(
    messages: &[serde_json::Value],
    tools: Option<&serde_json::Value>,
    authoritative_budget: Option<usize>,
    calibration: f32,
    estimation: crate::tokens::TokenEstimation,
    model: &str,
) -> anyhow::Result<()> {
    let Some(budget) = authoritative_budget else {
        return Ok(());
    };
    let head = protected_prompt_head_len(messages, prompt_read::ACTIVE_PROMPT_PREFIX);
    let newest_live_user = messages[head..]
        .iter()
        .rev()
        .find(|message| message["role"].as_str() == Some("user"));
    let estimated = estimate_request_tokens(&messages[..head], tools, estimation)
        + newest_live_user
            .map(|message| estimate_value_tokens(message, estimation))
            .unwrap_or(0);
    let required = calibrate_up(estimated, calibration);
    if required > budget {
        anyhow::bail!(
            "the exact active prompt, live user presentation, and required request scaffolding \
             need ~{required} input \
             tokens (including advertised tool schemas), which cannot fit model `{model}`'s \
             authoritative {budget}-token input budget; refusing before inference dispatch — \
             the operator prompt was not truncated"
        );
    }
    Ok(())
}

/// Refuse any Chat-style dispatch when its complete dynamic message list plus
/// the schemas currently advertised on that request no longer fit an
/// authoritative budget. Count trimming alone is not a token bound: one fresh
/// tool or prompt-read result can be larger than the entire window.
fn full_message_request_real_tokens(
    messages: &[serde_json::Value],
    tools: Option<&serde_json::Value>,
    calibration: f32,
    estimation: crate::tokens::TokenEstimation,
) -> usize {
    calibrate_up(
        estimate_request_tokens(messages, tools, estimation),
        calibration,
    )
}

/// Compression must fire whenever either the backend-anchored observation or
/// the authoritative whole-request estimate crosses a budget. Otherwise the
/// trigger can say "fits" immediately before preflight refuses the same wire.
pub(super) fn full_message_request_pressure_tokens(
    tracked_tokens: usize,
    wire_messages: &[serde_json::Value],
    tools: Option<&serde_json::Value>,
    calibration: f32,
    estimation: crate::tokens::TokenEstimation,
) -> usize {
    tracked_tokens.max(full_message_request_real_tokens(
        wire_messages,
        tools,
        calibration,
        estimation,
    ))
}

pub(super) fn preflight_full_message_request(
    messages: &[serde_json::Value],
    tools: Option<&serde_json::Value>,
    authoritative_budget: Option<usize>,
    calibration: f32,
    estimation: crate::tokens::TokenEstimation,
    model: &str,
) -> anyhow::Result<()> {
    let Some(budget) = authoritative_budget else {
        return Ok(());
    };
    let required = full_message_request_real_tokens(messages, tools, calibration, estimation);
    if required > budget {
        anyhow::bail!(
            "the complete inference request needs ~{required} input tokens, which cannot fit \
             model `{model}`'s authoritative {budget}-token input budget; refusing before \
             inference dispatch — the exact operator prompt and tool results were not truncated"
        );
    }
    Ok(())
}

/// #1528: the ONE token-shape estimate of a Responses request — the
/// `instructions` (as a protected system head), the running `input`, and the
/// flattened Responses-WIRE tool schemas — that BOTH [`preflight_responses_request`]
/// (which refuses when it exceeds the budget) and the `get_context_remaining`
/// self-read (which reports it as `used`) call, so the self-read counts exactly
/// what dispatch counts. `tools` is `None` for a tools-disabled request; pass the
/// Responses-wire `tools` array actually sent, never the Chat-shaped catalog.
/// Uncalibrated (chars/4) — the caller applies [`calibrate_up`] when it needs
/// real-token currency.
pub(super) fn estimate_responses_request_tokens(
    instructions: Option<&str>,
    input: &[serde_json::Value],
    tools: Option<&[serde_json::Value]>,
    estimation: crate::tokens::TokenEstimation,
) -> usize {
    let instructions_tokens = instructions
        .map(|text| {
            estimate_value_tokens(
                &serde_json::json!({"role": "system", "content": text}),
                estimation,
            )
        })
        .unwrap_or(0);
    let input_tokens = estimate_tokens(input, estimation);
    let tool_tokens = tools
        .map(|tools| estimate_value_tokens(&serde_json::Value::Array(tools.to_vec()), estimation))
        .unwrap_or(0);
    instructions_tokens + input_tokens + tool_tokens
}

/// The CALIBRATED real-token estimate of a Responses request — the raw
/// [`estimate_responses_request_tokens`] shape (chars/4) converted to the
/// backend-token currency dispatch enforces in, via the model's `calibration`.
/// Budget ceilings and remaining-token reports are real-token currency, so BOTH
/// the dispatch preflight AND the `get_context_remaining` self-read subtract
/// THIS, never the raw estimate (BHV-BUDGET-001/002/003: one currency, calibrated
/// exactly once).
pub(super) fn estimate_responses_request_real_tokens(
    instructions: Option<&str>,
    input: &[serde_json::Value],
    tools: Option<&[serde_json::Value]>,
    estimation: crate::tokens::TokenEstimation,
    calibration: f32,
) -> usize {
    calibrate_up(
        estimate_responses_request_tokens(instructions, input, tools, estimation),
        calibration,
    )
}

/// The Responses `get_context_remaining` self-read report, extracted so it is
/// unit-testable and shares ONE calibrated estimate with dispatch: `used` is the
/// CALIBRATED estimate of the exact next request (the instructions, the running
/// `input`, and the enabled Responses-wire tool schemas), subtracted from the
/// SAME `actionable_input_budget` the preflight refuses against, in the SAME
/// real-token currency — so the self-read's remaining and low-budget
/// classification match what dispatch would accept or reject (BHV-BUDGET-002).
pub(super) fn responses_context_remaining_report(
    instructions: Option<&str>,
    input: &[serde_json::Value],
    tools: Option<&[serde_json::Value]>,
    budget_state: &ResponsesBudgetState,
    calibration: f32,
    estimation: crate::tokens::TokenEstimation,
    low_budget_pct: usize,
) -> String {
    let used_real =
        estimate_responses_request_real_tokens(instructions, input, tools, estimation, calibration);
    budget::render_context_budget(
        used_real,
        budget_state.actionable_input_budget(),
        budget_state.num_ctx(),
        budget_state.input_ceiling_pct(),
        low_budget_pct,
    )
}

pub(super) fn preflight_responses_request(
    instructions: Option<&str>,
    input: &[serde_json::Value],
    tools: Option<&[serde_json::Value]>,
    authoritative_budget: Option<usize>,
    calibration: f32,
    estimation: crate::tokens::TokenEstimation,
    model: &str,
) -> anyhow::Result<()> {
    let Some(budget) = authoritative_budget else {
        return Ok(());
    };
    let required =
        estimate_responses_request_real_tokens(instructions, input, tools, estimation, calibration);
    if required > budget {
        anyhow::bail!(
            "the Responses request needs ~{required} input tokens, which cannot fit model \
             `{model}`'s authoritative {budget}-token input budget; refusing before inference \
             dispatch — the exact operator prompt and function outputs were not truncated"
        );
    }
    Ok(())
}

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

/// #1528: the ONE resolver both the Responses dispatch loop
/// (`openai_responses_complete_with_prompt_and_artifacts`) and the public
/// reporting seam ([`initial_context_input_budget`]) build their budget from, so
/// the seam can no longer report a value the loop does not enforce. A thin,
/// argument-ordering wrapper over [`ResponsesBudgetState::new`] that names the
/// "resolve one Responses budget from raw config" seam explicitly; the reserve,
/// ceiling, and cached-cap composition all live in the state's constructor.
pub(super) fn resolve_responses_budget(
    num_ctx: Option<u32>,
    safe_context: Option<u32>,
    max_ok_input: Option<u32>,
    mid_loop_trim_tokens: Option<usize>,
    input_ceiling_pct: u32,
    cognition: Option<crate::role_profile::Cognition>,
) -> ResponsesBudgetState {
    ResponsesBudgetState::new(
        num_ctx,
        input_ceiling_pct,
        cognition,
        max_ok_input,
        safe_context,
        mid_loop_trim_tokens,
    )
}

/// Resolve the initial input budget a caller should report for one backend
/// turn. This is the public reporting seam for the same percentage, cognition
/// output reserve, and cached-cap composition used by the dispatch loops.
///
/// The **Responses** branch PROJECTS from the shared [`ResponsesBudgetState`]
/// (via [`resolve_responses_budget`]) so the reported budget cannot diverge from
/// what the Responses loop enforces: that state RESERVES local output via the
/// cognition dial even though this wire sends no `max_output_tokens`, because a
/// declared window (#1526, invariant #4) must still leave room to generate. The
/// projected value is the state's soft send budget — cached caps composed with
/// the reserved hard ceiling — exactly what this seam has always returned.
/// Explicitly capable **Chat Completions** endpoints receive Newt's local
/// generation policy (its output reserve). **Ollama and embedded** backends keep
/// the percentage-only local ceiling. Ceiling-from-an-unsent-value is deliberate
/// for Responses — the alternative is an over-window request that only a reactive
/// 400 (or a silent truncation) can catch.
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
    // #1528: PROJECT the Responses reporting budget from the same resolver the
    // dispatch loop uses, so the seam applies the cognition output reserve the
    // loop applies (the old branch re-derived the ceiling with NO reserve and
    // over-reported, e.g. 26,214 while the loop enforced 16,768).
    if kind == crate::BackendKind::Openai && api == crate::OpenAiApi::Responses {
        return resolve_responses_budget(
            context_window,
            safe_context,
            max_ok_input,
            None,
            input_ceiling_pct,
            cognition,
        )
        .soft_send_budget()
        .map(|budget| u32::try_from(budget).expect("input budgets originate as u32 values"));
    }
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
    // Chat Completions applies the resolved generation output reserve above;
    // Ollama and embedded backends keep the percentage-only local ceiling
    // (`max_output_tokens` is `None`). The declared window still bounds the input
    // ceiling so an over-window request is caught pre-dispatch, not only by a 400.
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

/// Sanitize a per-model `estimate_ratio` for DISPATCH use — preflight
/// estimate scaling and trigger budget pricing (Phase 20 §2.3). Never used
/// for reporting, which reads the raw stored EMA directly.
///
/// Two independent guards:
/// 1. Only a finite value inside the learning clamp `[0.5, 3.0]` is
///    trusted; anything else (absent, NaN, a corrupted cache entry)
///    degrades to 1.0 — the identity, i.e. exactly the pre-calibration
///    behavior.
/// 2. **Floored at 1.0 (#1968).** Calibration may only TIGHTEN the
///    authoritative send-budget gate, never loosen it. A stored EMA below
///    1.0 usually means an Ollama prompt-cache-hit sample partially escaped
///    `newt-tui`'s `CapabilityEntry::record_estimate_sample` exclusion (a
///    partial hit reports only the newly-evaluated suffix, undercounting
///    the true prompt) — dispatching with it verbatim would scale chars/4
///    estimates DOWN below the model's real token cost. #1968's incident:
///    a poisoned EMA of ~0.999 let the authoritative 167,772-token gate
///    admit an estimate that resolved to a 205,189-token real request
///    (23.7% over budget). The floor is a backstop, not the fix on its
///    own — sample exclusion is what keeps the raw EMA honest in the
///    first place, and a floor of exactly 1.0 would not by itself have
///    caught this incident's true under-count. It still protects every
///    OTHER model whose EMA has not (yet) been re-learned clean.
pub(super) fn sanitize_estimate_ratio(estimate_ratio: Option<f32>) -> f32 {
    estimate_ratio
        .filter(|r| r.is_finite() && (0.5..=3.0).contains(r))
        .unwrap_or(1.0)
        .max(1.0)
}

/// Whether a round's real prompt-token count is truncation-suspect (Phase 20
/// §2.2): within 5% of the request's `num_ctx`, where Ollama may have
/// silently head-truncated the prompt. Such a round is window evidence of
/// NOTHING and must not raise any budget ratchet or promote tuning
/// confidence.
///
/// The ONE predicate every writer that ratchets from observed usage must
/// gate on (#1967): the per-round writer via [`emit_accepted`] below, the
/// turn-level writer in `newt-tui`'s chat loop
/// (`CapabilityEntry::record_success`), and retroactive pin validation at
/// cache load (`CapabilityEntry`'s suspect-pin invalidation) all call this
/// SAME function rather than re-deriving the 95% threshold — #1967's defect
/// was exactly a second, ungated copy of this check.
pub fn is_truncation_suspect(input_tokens: u32, num_ctx: Option<u32>) -> bool {
    num_ctx.is_some_and(|c| input_tokens >= c.saturating_mul(95) / 100)
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
/// [`authoritative_request_budget`], [`exposure_budget_tokens`],
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
        let preflight_budget =
            authoritative_request_budget(soft_send_budget, authoritative, mid_loop_trim_tokens);
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
        exposure_budget_tokens(self.soft_send_budget, self.safe_context)
    }

    /// The constraint governing the next attempted dispatch: the hard ceiling and
    /// soft send budget composed with the mid-loop trim threshold (the authoritative
    /// value each preflight refuses against — per round and for the tools-disabled
    /// final summary). Every ENFORCEMENT and REPORTING surface reads THIS value —
    /// preflight AND `get_context_remaining` — so the self-read can never advertise
    /// a budget the loop does not enforce. `None` leaves the preflight a no-op.
    /// (Tool exposure sizes the advertised catalog against the softer
    /// [`Self::exposure_budget`] instead, matching the Chat/Ollama sibling loops.)
    pub(super) fn actionable_input_budget(&self) -> Option<usize> {
        self.preflight_budget
    }

    /// The soft send budget — the numberless cw-400 fallback recovers against it.
    pub(super) fn soft_send_budget(&self) -> Option<usize> {
        self.soft_send_budget
    }

    /// The monotone learned HARD ceiling — the seed for cw-400 recovery and the
    /// hard leg of the hard-vs-soft distinction. Distinct from
    /// [`Self::actionable_input_budget`] (the value dispatch and
    /// `get_context_remaining` read), which may be tighter when a soft send /
    /// mid-loop-trim budget binds. Test-only observability of the hard leg: the
    /// non-test consumers ([`Self::recover_from_cw400`],
    /// [`Self::recovered_budget_for_window`], [`Self::compaction_budget`]) read the
    /// field directly.
    #[cfg(test)]
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
        self.preflight_budget =
            authoritative_request_budget(self.soft_send_budget, true, self.mid_loop_trim_tokens);
    }

    /// The compaction target for the cw-400 recovery's `compress` call: the
    /// authoritative next-dispatch budget minus real-token schema overhead,
    /// converted back into the pipeline's chars/4 currency. Called only after
    /// [`Self::recover_from_cw400`] has recomputed the budget.
    ///
    /// BHV-BUDGET-007: this targets [`Self::actionable_input_budget`]
    /// (`min(hard ceiling, mid-loop trim)`) — the value the immediately-following
    /// preflight refuses against — NOT the hard ceiling alone. A binding mid-loop
    /// trim can leave `actionable` tighter than the recovered hard ceiling;
    /// targeting the ceiling would let the compactor report success on a request
    /// the next preflight then rejects. `recover_from_cw400` guarantees
    /// `preflight_budget` is `Some` before this is called.
    ///
    /// #1528 B3: `with_tool_schemas` controls the schema-overhead subtraction. A
    /// tool-capable request carries the exposed tool schemas, so their real-token
    /// overhead is reserved (`true`). The tools-DISABLED final summary sends no
    /// schemas, so subtracting them would make the target needlessly tight and
    /// OVER-compact (`false`) — the estimate side already drops them by passing
    /// `tools = None` to the estimator.
    pub(super) fn compaction_budget(&self, calibration: f32, with_tool_schemas: bool) -> usize {
        let ceiling = self.preflight_budget.unwrap_or(0);
        let overhead = if with_tool_schemas {
            self.tool_schema_tokens
        } else {
            0
        };
        calibrate_down(ceiling.saturating_sub(overhead), calibration)
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
        // limit for Responses even though the wire sends no `num_ctx`. #1528: the
        // seam PROJECTS from `ResponsesBudgetState`, so it reserves the cognition
        // output allowance the loop reserves — the budget is the RESERVED ceiling
        // (16,768), NOT the un-reserved percentage bound (26,214) the seam used to
        // over-report, and NOT `None` (the old, now-reversed contract).
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
        // 32_768 − 16_000 Contemplating output reserve = 16_768, tighter than the
        // 80% percentage bound (26,214) — the SAME value the Responses loop enforces.
        assert_eq!(
            ceiling,
            Some(16_768),
            "the Responses seam projects the RESERVED ceiling the loop enforces (#1528)"
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
        assert_eq!(sanitize_estimate_ratio(Some(3.0)), 3.0, "clamp inclusive");
    }

    /// #1968: dispatch calibration may only tighten the authoritative
    /// send-budget gate, never loosen it — a stored EMA below 1.0 (a
    /// cache-hit-poisoned sample that escaped exclusion) must not scale
    /// preflight estimates DOWN below the model's real token cost.
    #[test]
    fn sanitize_estimate_ratio_floors_dispatch_at_1_0() {
        use super::sanitize_estimate_ratio;
        // Below 1.0 but inside the learning clamp: floored, not passed
        // through — this is the exact shape of the #1968 incident's final
        // stored ratio (0.9994337).
        assert_eq!(
            sanitize_estimate_ratio(Some(0.5)),
            1.0,
            "the clamp's own lower bound must not loosen the gate"
        );
        assert_eq!(sanitize_estimate_ratio(Some(0.9994337)), 1.0);
        // At or above 1.0: unaffected — the floor only ever raises, never
        // lowers, a value already safe to dispatch with.
        assert_eq!(sanitize_estimate_ratio(Some(1.0)), 1.0);
        assert_eq!(sanitize_estimate_ratio(Some(1.3)), 1.3);
    }

    /// #1967: the ONE truncation-suspect predicate — a suspect round's
    /// prompt is window evidence of nothing (Ollama may have silently
    /// head-truncated it), so nothing may treat it as proof of a safe
    /// ceiling. Replays the incident's exact numbers: `num_ctx` 209,715
    /// (the session's `safe_context`, absent an explicit `[backends]
    /// num_ctx`), threshold 199,229 (95% of it, integer floor), and the
    /// poisoned round's real 205,189 input tokens.
    #[test]
    fn is_truncation_suspect_replays_the_1967_incident_numbers() {
        use super::is_truncation_suspect;
        assert!(
            is_truncation_suspect(205_189, Some(209_715)),
            "205,189 is 97.8% of 209,715 — inside the suspect zone"
        );
        // The threshold itself: exactly 95% (integer floor) is suspect;
        // one token under is not.
        assert!(is_truncation_suspect(199_229, Some(209_715)));
        assert!(!is_truncation_suspect(199_228, Some(209_715)));
        // No known `num_ctx` — nothing to compare against, never suspect.
        assert!(!is_truncation_suspect(205_189, None));
        // A genuinely small prompt, nowhere near the window.
        assert!(!is_truncation_suspect(4_136, Some(209_715)));
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

        // Every surface reports the enforced 16,768, not the un-reserved 26,214:
        // 1. dispatch preflight AND get_context_remaining both read
        // `actionable_input_budget`, 2. tool exposure reads the soft budget,
        // 3. the hard ceiling seeds cw-400 recovery. Here they coincide.
        assert_eq!(
            state.actionable_input_budget(),
            Some(16_768),
            "dispatch preflight + get_context_remaining ceiling"
        );
        assert_eq!(
            state.exposure_budget(),
            Some(16_768),
            "tool exposure clip budget"
        );
        assert_eq!(
            state.learned_hard_ceiling(),
            Some(16_768),
            "hard ceiling (seeds cw-400 recovery)"
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
        assert_eq!(
            state.compaction_budget(1.0, true),
            16_768,
            "compaction target"
        );
    }

    #[test]
    fn compaction_budget_drops_schema_overhead_for_the_tools_disabled_summary() {
        // #1528 B3 (req 7): a tool-capable request reserves the exposed schemas'
        // real-token overhead in its compaction target; the tools-DISABLED final
        // summary must NOT — subtracting schemas that are never sent makes the target
        // needlessly tight and over-compacts. At identity calibration the difference
        // between the two targets is EXACTLY the schema overhead.
        use super::ResponsesBudgetState;
        let mut state = ResponsesBudgetState::new(Some(32_768), 80, None, None, None, None);
        let recovered = state.recovered_budget_for_window(32_768);
        state.recover_from_cw400(recovered);
        state.set_tool_schema_tokens(1_000);
        let with_schemas = state.compaction_budget(1.0, true);
        let without_schemas = state.compaction_budget(1.0, false);
        assert!(
            without_schemas > with_schemas,
            "dropping the un-sent schemas RAISES the target: {without_schemas} !> {with_schemas}"
        );
        assert_eq!(
            without_schemas - with_schemas,
            1_000,
            "the tools-disabled target reclaims exactly the un-sent schema overhead"
        );
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
        // The value get_context_remaining now feeds to render_context_budget is
        // the actionable input budget (here == the enforced hard ceiling).
        assert_eq!(state.actionable_input_budget(), Some(16_768));
        // The pre-fix recompute (reserve = None) over-advertised — the divergence.
        assert_eq!(
            num_ctx_input_ceiling(state.num_ctx(), 80, None),
            Some(26_214)
        );
        assert_ne!(state.actionable_input_budget(), Some(26_214));
        let report = crate::agentic::budget::render_context_budget(
            0,
            state.actionable_input_budget(),
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
        assert_eq!(state.actionable_input_budget(), Some(8_000));
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
        assert_eq!(state.actionable_input_budget(), Some(26_214));
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
        assert_eq!(
            state.actionable_input_budget(),
            None,
            "nothing to refuse against"
        );
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
        assert_eq!(state.actionable_input_budget(), Some(0));
    }

    // --- #1534 "finish the single source of truth" — the two equalities ---

    /// P1.1 (#1534): the public reporting seam PROJECTS from the shared
    /// `ResponsesBudgetState`, so for EVERY cognition level and representative
    /// configured/learned combo the seam equals the state's authoritative input
    /// budget (its soft send budget). Pre-fix the Responses branch re-derived the
    /// ceiling with NO output reserve and diverged (26,214 vs 16,768).
    #[test]
    fn seam_projects_the_responses_budget_state_for_every_cognition() {
        use super::ResponsesBudgetState;
        let capability = ChatCompletionsCapability {
            cognition: Some(true),
            ..Default::default()
        };
        let cognitions = [
            None,
            Some(Cognition::Glancing),
            Some(Cognition::Pondering),
            Some(Cognition::Deliberating),
            Some(Cognition::Contemplating),
        ];
        // (num_ctx, max_ok_input, safe_context): configured-window-only, cached
        // caps present, a learned/cached cap tighter than the window, and the
        // cloud default (no window).
        let combos = [
            (Some(32_768u32), None, None),
            (Some(65_536), Some(40_000), None),
            (Some(32_768), None, Some(8_000)),
            (Some(32_768), Some(6_068), Some(26_214)),
            (None, Some(12_000), Some(20_000)),
            (None, None, None),
        ];
        for cognition in cognitions {
            for (num_ctx, max_ok_input, safe_context) in combos {
                let seam = super::initial_context_input_budget(
                    BackendKind::Openai,
                    OpenAiApi::Responses,
                    num_ctx,
                    80,
                    cognition,
                    capability,
                    ReasoningReplayScope::CurrentUserTurn,
                    max_ok_input,
                    safe_context,
                );
                let state = ResponsesBudgetState::new(
                    num_ctx,
                    80,
                    cognition,
                    max_ok_input,
                    safe_context,
                    None,
                );
                assert_eq!(
                    seam,
                    state
                        .soft_send_budget()
                        .map(|budget| u32::try_from(budget).unwrap()),
                    "the seam must project the state's authoritative input budget \
                     (num_ctx={num_ctx:?}, max_ok={max_ok_input:?}, safe={safe_context:?}, \
                     cognition={cognition:?})",
                );
            }
        }
        // Lock the exact reserve divergence the fix closes: Contemplating at 32K
        // reports the RESERVED 16,768, never the un-reserved 26,214.
        let contemplating = super::initial_context_input_budget(
            BackendKind::Openai,
            OpenAiApi::Responses,
            Some(32_768),
            80,
            Some(Cognition::Contemplating),
            capability,
            ReasoningReplayScope::CurrentUserTurn,
            None,
            None,
        );
        assert_eq!(contemplating, Some(16_768));
        assert_ne!(contemplating, Some(26_214), "no un-reserved over-report");
    }

    /// P1.2 (#1534): `get_context_remaining` describes the NEXT dispatch — it
    /// reads the SAME `actionable_input_budget` the preflight refuses against and
    /// the SAME `estimate_responses_request_tokens` (instructions, the running
    /// input, and the real Responses-wire tools) the preflight counts. For every
    /// budget shape the rendered remaining equals
    /// `actionable_input_budget − actual_responses_wire_estimate`.
    #[test]
    fn get_context_remaining_agrees_with_dispatch_across_budget_shapes() {
        use super::ResponsesBudgetState;
        let est = crate::tokens::TokenEstimation::default();
        let input = [serde_json::json!({"role": "user", "content": "do the thing"})];
        let big_instructions = "SYSTEM POLICY. ".repeat(200); // ~3,000 chars
        let wire_tools = [
            serde_json::json!({
                "type": "function",
                "name": "read_file",
                "description": "read a file in pages",
                "parameters": {"type": "object", "properties": {
                    "path": {"type": "string"}, "offset": {"type": "integer"}
                }, "required": ["path"]}
            }),
            serde_json::json!({
                "type": "function",
                "name": "run_command",
                "description": "run a shell command in the workspace",
                "parameters": {"type": "object", "properties": {
                    "command": {"type": "string"}
                }, "required": ["command"]}
            }),
        ];

        // The reconstruction the loop performs at the `get_context_remaining`
        // intercept: used = the shared wire estimator, ceiling = actionable.
        let agrees = |label: &str,
                      state: &ResponsesBudgetState,
                      instructions: Option<&str>,
                      tools: Option<&[serde_json::Value]>|
         -> usize {
            let used =
                crate::agentic::estimate_responses_request_tokens(instructions, &input, tools, est);
            let ceiling = state
                .actionable_input_budget()
                .unwrap_or_else(|| panic!("{label}: the actionable budget must bind"));
            let expected = ceiling.saturating_sub(used);
            let report = crate::agentic::budget::render_context_budget(
                used,
                state.actionable_input_budget(),
                state.num_ctx(),
                state.input_ceiling_pct(),
                15,
            );
            assert!(
                report.contains(&format!("{expected} tokens remaining")),
                "{label}: self-read remaining must equal actionable({ceiling}) − \
                 wire_estimate({used}) = {expected}; got: {report}",
            );
            used
        };

        // 1. Only num_ctx constrains (no reserve, no cached caps).
        let s = ResponsesBudgetState::new(Some(32_768), 80, None, None, None, None);
        assert_eq!(s.actionable_input_budget(), Some(26_214));
        agrees("only num_ctx", &s, None, None);

        // 2. Only safe_context constrains (no window at all).
        let s = ResponsesBudgetState::new(None, 80, None, None, Some(8_000), None);
        assert_eq!(s.actionable_input_budget(), Some(8_000));
        // Pre-fix divergence: the old ceiling (learned_hard_ceiling) was None here,
        // so the self-read said "no ceiling" while dispatch refused at 8,000.
        assert_eq!(
            s.learned_hard_ceiling(),
            None,
            "no window → no hard ceiling"
        );
        agrees("only safe_context", &s, None, None);

        // 3. max_ok_input smaller than safe_context (a floor, never a cap).
        let s = ResponsesBudgetState::new(Some(32_768), 80, None, Some(12_000), Some(20_000), None);
        assert_eq!(s.actionable_input_budget(), Some(20_000));
        agrees("max_ok smaller", &s, None, None);

        // 4. mid_loop_trim_tokens smaller than the ceiling — it binds the dispatch.
        let s = ResponsesBudgetState::new(
            Some(32_768),
            80,
            Some(Cognition::Contemplating),
            None,
            None,
            Some(6_000),
        );
        assert_eq!(s.actionable_input_budget(), Some(6_000));
        // Divergence guard: the OLD ceiling over-advertised 16,768 while dispatch
        // refuses at 6,000.
        assert_eq!(s.learned_hard_ceiling(), Some(16_768));
        assert_ne!(s.learned_hard_ceiling(), s.actionable_input_budget());
        agrees("mid_loop_trim smaller", &s, None, None);

        // 5. Cognition reserve active (16,768 tighter than the 26,214 pct bound).
        let s = ResponsesBudgetState::new(
            Some(32_768),
            80,
            Some(Cognition::Contemplating),
            None,
            None,
            None,
        );
        assert_eq!(s.actionable_input_budget(), Some(16_768));
        agrees("cognition reserve", &s, None, None);

        // 6. Substantial instructions — the used side MUST count them (the old
        // Chat-shaped estimate omitted instructions entirely).
        let s = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, None);
        let with_instr = agrees(
            "instructions substantial",
            &s,
            Some(&big_instructions),
            None,
        );
        let without_instr =
            crate::agentic::estimate_responses_request_tokens(None, &input, None, est);
        assert!(
            with_instr > without_instr,
            "instructions must raise the wire estimate (was omitted pre-fix)",
        );

        // 7. Responses tool schemas enabled — the used side counts the real
        //    Responses-wire tools.
        let with_tools = agrees("tools enabled", &s, None, Some(&wire_tools));
        // 8. Tools disabled — the used side drops the tool schemas.
        let without_tools = agrees("tools disabled", &s, None, None);
        assert!(
            with_tools > without_tools,
            "enabled wire tools must raise the estimate over tools-disabled",
        );

        // 9. A learned hard ceiling tighter than EVERY configured value (a cw-400
        //    recovery), so the self-read tracks the recovered constraint.
        let mut s = ResponsesBudgetState::new(
            Some(65_536),
            80,
            Some(Cognition::Deliberating),
            Some(40_000),
            Some(40_000),
            Some(30_000),
        );
        s.recover_from_cw400(9_000);
        assert_eq!(s.actionable_input_budget(), Some(9_000));
        // Tighter than the window ceiling, the cached caps, and the mid-loop trim.
        assert!(s.actionable_input_budget().unwrap() < 30_000);
        agrees(
            "learned ceiling tighter than all",
            &s,
            Some(&big_instructions),
            Some(&wire_tools),
        );

        // 10. max_ok_input is the SOLE cached cap and below the ceiling, so it
        //     binds as the smallest operative limit (safe_context absent — with
        //     safe_context present the two are max'd and safe_context wins).
        let s = ResponsesBudgetState::new(Some(32_768), 80, None, Some(12_000), None, None);
        assert_eq!(
            s.actionable_input_budget(),
            Some(12_000),
            "max_ok binds alone"
        );
        agrees("max_ok binds", &s, None, None);

        // 11. No authoritative ceiling (unknown cloud window, no caches): the
        //     self-read must STATE that no ceiling is known, never fabricate a
        //     remaining figure. `agrees` requires a bound, so assert directly.
        let s =
            ResponsesBudgetState::new(None, 80, Some(Cognition::Contemplating), None, None, None);
        assert_eq!(
            s.actionable_input_budget(),
            None,
            "an unknown window stays unknown — no fabricated ceiling"
        );
        let unknown = crate::agentic::budget::render_context_budget(
            crate::agentic::estimate_responses_request_tokens(None, &input, None, est),
            s.actionable_input_budget(),
            s.num_ctx(),
            s.input_ceiling_pct(),
            15,
        );
        assert!(
            unknown.contains("No input-token ceiling is configured"),
            "ceiling-less self-read must say so, not report remaining: {unknown}",
        );

        // 12. Authoritative zero (window minus the cognition reserve leaves no
        //     input room): a real fail-closed budget, never erased to None. The
        //     self-read reports 0 remaining, NOT "no ceiling".
        let s = ResponsesBudgetState::new(
            Some(16_000),
            80,
            Some(Cognition::Contemplating),
            None,
            None,
            None,
        );
        assert_eq!(
            s.actionable_input_budget(),
            Some(0),
            "window (16,000) − reserve (16,000) = 0 is an authoritative zero, not None",
        );
        let zero = crate::agentic::budget::render_context_budget(
            crate::agentic::estimate_responses_request_tokens(None, &input, None, est),
            s.actionable_input_budget(),
            s.num_ctx(),
            s.input_ceiling_pct(),
            15,
        );
        assert!(
            zero.contains("0 tokens remaining") && !zero.contains("No input-token ceiling"),
            "an authoritative zero renders 0 remaining, not ceiling-less: {zero}",
        );

        // --- fail-on-old: the SELF-READ SOURCE matters. The pre-fix intercept
        // read `learned_hard_ceiling` (which diverges from the enforced budget
        // when a soft / mid-loop limit binds, and is `None` when only
        // safe_context binds) and the CHAT-shaped estimator (which omits
        // instructions). Rendering / estimating each shape both ways proves the
        // OLD pair yields the WRONG answer — so a regression of the mod.rs
        // intercept back to the old source/estimator is caught here (the intercept
        // reads the NEW pair — `actionable_input_budget` +
        // `estimate_responses_request_tokens` — per the diff at the
        // `is_context_remaining_call` site).
        let render0 = |ceiling: Option<usize>, num_ctx: Option<u32>| {
            crate::agentic::budget::render_context_budget(0, ceiling, num_ctx, 80, 15)
        };
        // Item 2: safe_context=8,000 with no window. OLD (learned=None) → "no
        // ceiling"; NEW (actionable=8,000) → an 8,000 remaining budget.
        let s = ResponsesBudgetState::new(None, 80, None, None, Some(8_000), None);
        let old2 = render0(s.learned_hard_ceiling(), s.num_ctx());
        let new2 = render0(s.actionable_input_budget(), s.num_ctx());
        assert!(
            old2.contains("No input-token ceiling is configured"),
            "item 2 pre-fix source wrongly reports no ceiling: {old2}"
        );
        assert!(
            new2.contains("8000 tokens remaining"),
            "item 2 fixed source reports the enforced 8,000: {new2}"
        );
        assert_ne!(old2, new2, "item 2: the ceiling source changes the answer");
        // Item 3: a 6,000 mid-loop trim under a 16,768 hard ceiling. OLD
        // (learned=16,768) over-advertises; NEW (actionable=6,000) matches dispatch.
        let s = ResponsesBudgetState::new(
            Some(32_768),
            80,
            Some(Cognition::Contemplating),
            None,
            None,
            Some(6_000),
        );
        let old3 = render0(s.learned_hard_ceiling(), s.num_ctx());
        let new3 = render0(s.actionable_input_budget(), s.num_ctx());
        assert!(
            old3.contains("16768 tokens remaining"),
            "item 3 pre-fix over-advertises the hard ceiling: {old3}"
        );
        assert!(
            new3.contains("6000 tokens remaining"),
            "item 3 fixed source reports the 6,000 dispatch refuses at: {new3}"
        );
        assert_ne!(
            old3, new3,
            "item 3: the mid-loop trim is invisible to the pre-fix source"
        );
        // Item 5: the pre-fix self-read estimated the CHAT-shaped catalog and
        // OMITTED instructions. The Responses-wire estimate (instructions + flat
        // Responses tools) is the larger, honest count dispatch enforces.
        let responses_used = crate::agentic::estimate_responses_request_tokens(
            Some(&big_instructions),
            &input,
            Some(&wire_tools),
            est,
        );
        // The pre-fix Chat estimator takes the tool catalog as a single array
        // Value (Chat's nested `{function:{…}}` shape), not the flat Responses tools.
        let chat_tools = serde_json::json!([{
            "type": "function",
            "function": {"name": "run_command", "description": "run a shell command",
                "parameters": {"type": "object",
                    "properties": {"command": {"type": "string"}}, "required": ["command"]}}
        }]);
        let chat_used =
            crate::agentic::trim::estimate_request_tokens(&input, Some(&chat_tools), est);
        assert!(
            responses_used > chat_used,
            "item 5: the Responses-wire estimate (instructions + flat tools, {responses_used}) must \
             exceed the pre-fix Chat-shaped estimate that omitted instructions ({chat_used})",
        );
    }

    /// #1534 monotonicity (CG-6 and the used-side): the actionable budget and the
    /// reported remaining only ever move in the safe direction — tighter inputs
    /// never buy more room, and a recovered hard ceiling never loosens.
    #[test]
    fn responses_budget_moves_only_in_the_safe_direction() {
        use super::ResponsesBudgetState;
        let est = crate::tokens::TokenEstimation::default();
        let input = [serde_json::json!({"role": "user", "content": "hi"})];

        // (a) Lowering an authoritative limit never RAISES the actionable budget.
        let loose = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, None);
        let tight = ResponsesBudgetState::new(Some(32_768), 80, None, None, None, None);
        assert!(
            tight.actionable_input_budget() <= loose.actionable_input_budget(),
            "a smaller window cannot raise the actionable budget",
        );
        let capped =
            ResponsesBudgetState::new(Some(65_536), 80, None, None, Some(8_000), Some(6_000));
        assert!(
            capped.actionable_input_budget() <= loose.actionable_input_budget(),
            "a tighter safe_context + mid-loop trim cannot raise it",
        );

        // (b)/(c) Adding instructions or tool schemas never RAISES remaining.
        let s = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, None);
        let ceiling = s.actionable_input_budget().expect("window binds");
        let rem = |instr: Option<&str>, tools: Option<&[serde_json::Value]>| {
            ceiling.saturating_sub(crate::agentic::estimate_responses_request_tokens(
                instr, &input, tools, est,
            ))
        };
        let big = "POLICY. ".repeat(200);
        assert!(
            rem(Some(&big), None) <= rem(None, None),
            "instructions never increase remaining",
        );
        let wire_tools = [serde_json::json!({
            "type": "function", "name": "run_command",
            "description": "run a shell command",
            "parameters": {"type": "object",
                "properties": {"command": {"type": "string"}}, "required": ["command"]}
        })];
        assert!(
            rem(None, Some(&wire_tools)) <= rem(None, None),
            "tool schemas never increase remaining",
        );

        // (d) A cw-400 only TIGHTENS the hard ceiling; a later larger recovered
        //     window cannot raise it back.
        let mut s = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, None);
        let before = s.actionable_input_budget();
        s.recover_from_cw400(10_000);
        let after = s.actionable_input_budget();
        assert!(
            after <= before && after == Some(10_000),
            "a cw-400 tightens the actionable budget to the recovered window",
        );
        s.recover_from_cw400(50_000);
        assert_eq!(
            s.actionable_input_budget(),
            Some(10_000),
            "a later larger recovered window cannot raise the hard ceiling",
        );
    }

    /// #1534 BHV-BUDGET-001/002/003: the Responses self-read and the dispatch
    /// preflight share ONE calibrated estimate in ONE real-token currency. For
    /// every calibration ratio the reported remaining equals
    /// `actionable − calibrate_up(raw, ratio)`, and the preflight refuses against
    /// that same real value — never the raw chars/4 estimate.
    #[test]
    fn responses_self_read_and_dispatch_agree_across_calibration() {
        use super::ResponsesBudgetState;
        let est = crate::tokens::TokenEstimation::default();
        let instr = "SYSTEM POLICY. ".repeat(64);
        let input = [serde_json::json!({"role": "user", "content": "please do the task"})];
        let tools = [serde_json::json!({
            "type": "function", "name": "run_command",
            "description": "run a shell command in the workspace",
            "parameters": {"type": "object",
                "properties": {"command": {"type": "string"}}, "required": ["command"]}
        })];
        // A generous window so the request fits with positive remaining.
        let state = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, None);
        let actionable = state.actionable_input_budget().expect("window binds");
        let raw = crate::agentic::estimate_responses_request_tokens(
            Some(&instr),
            &input,
            Some(&tools),
            est,
        );
        for ratio in [0.5f32, 1.0, 1.3, 2.0, 3.0] {
            let real = super::calibrate_up(raw, ratio);
            // The shared calibrated estimator IS calibrate_up(raw).
            assert_eq!(
                crate::agentic::estimate_responses_request_real_tokens(
                    Some(&instr),
                    &input,
                    Some(&tools),
                    est,
                    ratio,
                ),
                real,
                "ratio {ratio}: calibrated estimator == calibrate_up(raw)",
            );
            // Self-read: remaining == actionable − real.
            let report = crate::agentic::responses_context_remaining_report(
                Some(&instr),
                &input,
                Some(&tools),
                &state,
                ratio,
                est,
                15,
            );
            let expected = actionable.saturating_sub(real);
            assert!(
                report.contains(&format!("{expected} tokens remaining")),
                "ratio {ratio}: remaining must be actionable({actionable}) − real({real}) \
                 = {expected}: {report}",
            );
            // Dispatch refuses against the SAME real value: Ok exactly at `real`,
            // Err one token below it.
            assert!(
                crate::agentic::preflight_responses_request(
                    Some(&instr),
                    &input,
                    Some(&tools),
                    Some(real),
                    ratio,
                    est,
                    "m",
                )
                .is_ok(),
                "ratio {ratio}: preflight accepts a budget equal to the real estimate",
            );
            assert!(
                crate::agentic::preflight_responses_request(
                    Some(&instr),
                    &input,
                    Some(&tools),
                    Some(real.saturating_sub(1)),
                    ratio,
                    est,
                    "m",
                )
                .is_err(),
                "ratio {ratio}: preflight refuses one token below the real estimate",
            );
        }
    }

    /// #1534 BHV-BUDGET-002 fail-on-old: calibration decides the low-budget
    /// warning. A request whose CALIBRATED size nearly fills the budget is LOW;
    /// the pre-fix self-read subtracted the smaller RAW estimate and looked
    /// healthy. This FAILS on 8f0111c (which rendered the raw estimate). Mirrors
    /// the 10,000 / 6,000 / ×1.5 / 9,000 / 1,000 shape.
    #[test]
    fn responses_self_read_low_budget_tracks_calibration() {
        use super::ResponsesBudgetState;
        let est = crate::tokens::TokenEstimation::default();
        let input = [serde_json::json!({"role": "user", "content": "x ".repeat(4_000)})];
        let raw = crate::agentic::estimate_responses_request_tokens(None, &input, None, est);
        let ratio = 1.5f32;
        let real = super::calibrate_up(raw, ratio);
        // Budget just above the CALIBRATED size: real leaves < 15% (LOW); the raw
        // estimate leaves ~36% (not LOW). Guarantees the flip for any raw.
        let actionable = (real + real / 20) as u32;
        let state = ResponsesBudgetState::new(None, 80, None, None, Some(actionable), None);
        assert_eq!(state.actionable_input_budget(), Some(actionable as usize));
        let calibrated = crate::agentic::responses_context_remaining_report(
            None, &input, None, &state, ratio, est, 15,
        );
        // The pre-fix behaviour: subtract the RAW estimate from the same ceiling.
        let raw_report = crate::agentic::budget::render_context_budget(
            raw,
            state.actionable_input_budget(),
            state.num_ctx(),
            state.input_ceiling_pct(),
            15,
        );
        assert!(
            calibrated.contains("Budget is LOW"),
            "the calibrated self-read must warn LOW ({real} real of {actionable}): {calibrated}",
        );
        assert!(
            !raw_report.contains("Budget is LOW"),
            "the pre-fix raw self-read did NOT warn — the bug ({raw} raw of {actionable}): {raw_report}",
        );
    }

    /// #1534 BHV-BUDGET-007: reactive recovery compacts to a target the
    /// immediately-following preflight accepts — the compaction budget tracks
    /// `actionable_input_budget` (min(hard ceiling, mid-loop trim)), never the
    /// looser hard ceiling. Also proves the target round-trips through calibration
    /// without exceeding that budget, and preserves an authoritative zero.
    #[test]
    fn recovery_compaction_target_cannot_exceed_the_next_preflight() {
        use super::ResponsesBudgetState;
        let est = crate::tokens::TokenEstimation::default();

        // Case A: the hard ceiling is the smallest — actionable == hard == 8,000.
        let mut a = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, Some(12_000));
        a.recover_from_cw400(8_000);
        assert_eq!(a.actionable_input_budget(), Some(8_000));
        assert_eq!(
            a.compaction_budget(1.0, true),
            8_000,
            "targets the 8,000 preflight enforces"
        );

        // Case B: the mid-loop trim is the smallest — actionable == 6,000 while the
        // hard ceiling is 16,000. The compactor MUST target 6,000, not 16,000, or
        // preflight would reject what it accepted (pre-fix targeted the ceiling).
        let mut b = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, Some(6_000));
        b.recover_from_cw400(16_000);
        assert_eq!(b.actionable_input_budget(), Some(6_000));
        assert_eq!(
            b.compaction_budget(1.0, true),
            6_000,
            "targets the 6,000 preflight enforces"
        );
        assert_ne!(
            b.compaction_budget(1.0, true),
            16_000,
            "must not target the looser hard ceiling"
        );

        // Case C: the target round-trips through calibration without exceeding the
        // budget the preflight enforces — a request compacted to the target passes.
        let mut c = ResponsesBudgetState::new(Some(65_536), 80, None, None, None, None);
        c.set_tool_schema_tokens(1_000);
        c.recover_from_cw400(20_000);
        let actionable_c = c.actionable_input_budget().unwrap();
        for ratio in [1.3f32, 2.0, 3.0] {
            let target_raw = c.compaction_budget(ratio, true); // chars/4 input budget
            let calibrated_input = super::calibrate_up(target_raw, ratio);
            assert!(
                calibrated_input + 1_000 <= actionable_c,
                "ratio {ratio}: calibrated compacted input ({calibrated_input}) + tools (1000) \
                 must fit actionable ({actionable_c})",
            );
        }

        // Case D: an authoritative zero actionable budget stays zero (fail-closed)
        // — a zero compaction target and a 0-remaining self-read, NOT an unknown
        // ceiling.
        let d = ResponsesBudgetState::new(
            Some(16_000),
            80,
            Some(Cognition::Contemplating),
            None,
            None,
            None,
        );
        assert_eq!(d.actionable_input_budget(), Some(0));
        assert_eq!(
            d.compaction_budget(1.5, true),
            0,
            "zero budget → zero compaction target"
        );
        let report =
            crate::agentic::responses_context_remaining_report(None, &[], None, &d, 1.5, est, 15);
        assert!(
            report.contains("0 tokens remaining") && !report.contains("No input-token ceiling"),
            "an authoritative zero renders 0 remaining, not unknown: {report}",
        );
    }
}
