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
#[path = "send_budget_tests/mod.rs"]
mod send_budget_tests;
