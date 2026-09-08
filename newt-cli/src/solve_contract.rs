//! The observability-contract record `newt solve` emits (W0 #1511, epic
//! #1506) — the wire format the EXTERNAL evaluator consumes.
//!
//! The contract (`gilamonster-bench/CONTRACT.md`, `contract_version: "1"`) is
//! **data, deliberately re-declared per consumer**: this module is newt's
//! emitter-side declaration, and the bench keeps its own consumer-side
//! structs. Do NOT extract a shared `-contract` crate — that re-introduces
//! the circularity the versioned wire format exists to prevent.
//!
//! Everything here is pure (inputs → `serde_json::Value`) so the record
//! shape is unit-tested without a run; `solve::run` supplies the inputs and
//! appends the lines to `--events`. Exactly ONE contract record is emitted
//! per solve — the bench keys on the presence of `contract_version` and
//! rejects ambiguous traces.

use newt_core::{BehaviorSignal, ErrorClass, ParseSignal, TurnEndReason};

/// The contract version this emitter declares. Bumped only on a breaking
/// change to field names/semantics; adding an optional field is not breaking.
pub const CONTRACT_VERSION: &str = "1";

/// Provenance: which family member emitted the record.
pub const AGENT: &str = "newt-agent";

/// Everything the contract record serializes, already resolved by the caller.
/// `model_digest` is operator-supplied ONLY (flag / env twin) — when absent
/// it is omitted from the record, never fabricated: a made-up digest would
/// defeat the exact reason the field exists (silent same-name re-uploads).
pub struct ContractInputs<'a> {
    /// What the matrix asked the agent to run (the resolved request model).
    pub requested_model: &'a str,
    /// What the backend actually resolved/served: the response body's `model`
    /// field when the backend reported one, else the request model (the
    /// caller documents which it had).
    pub effective_model: &'a str,
    /// Operator-supplied sha256 of the served weights; `None` ⇒ omitted.
    pub model_digest: Option<&'a str>,
    /// The resolved backend driven for this solve.
    pub backend_name: &'a str,
    /// Wire kind label (`openai` / `ollama` / `embedded`).
    pub backend_kind: &'a str,
    /// One of the [`outcome_label`] strings.
    pub outcome: &'static str,
    /// The `--context-window` pin the agent ran with; `None` ⇒ omitted (the
    /// agent used its defaults — nothing authoritative to report).
    pub context_window: Option<u32>,
    /// The tenacity level the run resolved (family override / default).
    pub tenacity: &'a str,
    /// Effective cognition label (`default` when Newt sends no selection, or
    /// one of the explicit cognition levels).
    pub cognition: &'a str,
    /// `on` / `off` — whether a real crew runner was installed for the turn.
    pub crew: &'static str,
    /// `"on"` / `"off"` — whether OCAP enforcement was live for the run.
    pub ocap: &'static str,
    /// The max tool-rounds cap the driver actually used.
    pub max_rounds: u32,
    /// Wall-clock duration of the solve in milliseconds.
    pub wall_ms: u64,
    /// Generated (output) tokens, when the backend reported usage.
    pub gen_tokens: Option<u64>,
}

/// How a turn ended, decided ONCE (#2212, corrected by #2218).
///
/// Two fields render this, and they answer DIFFERENT questions — which is why
/// they legitimately differ rather than drifting:
///
/// * `outcome`, in the versioned contract record, answers **"was this a real
///   attempt?"**. `gilamonster-bench/CONTRACT.md` is explicit: *"`completed` —
///   the agent drove the model to a terminal state. Pass/fail is the suite
///   verifier's call, not this field's."* The taxonomy's whole purpose is a
///   *Real attempt?* column; `transport_error` / `timeout` / `harness_error`
///   are ❌ because the model was never meaningfully exercised.
/// * `status`, in newt's own trace line, answers **"did this run finish what
///   it set out to do?"**. That is the question #2212 found being answered
///   dishonestly, and it has no external consumer.
///
/// Deciding once and rendering twice is what keeps them from drifting the way
/// they did before #2215, while still letting them disagree where the two
/// questions genuinely have different answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    /// A genuine final answer.
    Completed,
    /// No error, but the loop stopped before finishing — a wall, not a
    /// failure. The model was reached and did real work, so this is still a
    /// real attempt in contract terms.
    StoppedShort(TurnEndReason),
    /// The turn failed; the TYPED class decides which failure.
    Failed(Option<ErrorClass>),
}

/// Classify the turn from every signal that can end it.
pub fn terminal(
    clean: bool,
    class: Option<ErrorClass>,
    end_reason: Option<TurnEndReason>,
) -> Terminal {
    if !clean {
        return Terminal::Failed(class);
    }
    // Matched explicitly, with no `_` arm: a NEW `TurnEndReason` variant must
    // fail to compile here and be classified deliberately rather than silently
    // inherit a terminal state. Silent inheritance is how #2212 arrived.
    match end_reason {
        Some(
            reason @ (TurnEndReason::RoundCap | TurnEndReason::Empty | TurnEndReason::Cancelled),
        ) => Terminal::StoppedShort(reason),
        Some(
            TurnEndReason::Completed
            | TurnEndReason::NarrationCapExhausted
            | TurnEndReason::NarrationFinalRound,
        )
        | None => Terminal::Completed,
        // `Failed` implies an error, so `clean` is false and this is
        // unreachable. Classified anyway rather than left to a wildcard.
        Some(TurnEndReason::Failed) => Terminal::Failed(Some(ErrorClass::Harness)),
    }
}

/// The contract record's `outcome` — **exactly the five values `CONTRACT.md`
/// defines**, and no others. The bench's `Outcome` is a CLOSED serde enum with
/// no `serde(other)` (`gilamonster-bench/src/contract.rs:20`), so an unknown
/// value does not deserialize: the run does not score badly, it DROPS OUT of
/// the matrix. #2215 emitted `round_cap` here and silently deleted rows.
///
/// **A cap exit maps to `timeout`, and that is defensible on the merits rather
/// than a compromise (#2218).** The contract needs a bucket for "exhausted its
/// budget without achieving the goal", and at v1 granularity both flavours of
/// budget belong in it: `is_real_attempt()` is `Completed | ModelError`, so
/// `timeout` correctly excludes a run that did not finish from capability
/// scoring. A cap exit without any continuation — and newt offers none today —
/// is a failure, not a terminal state worth scoring.
///
/// **What this conflates, recorded so the next reader knows it was decided:**
/// a wall-clock timeout and a round-cap grind become indistinguishable here.
/// They want different fixes (more time vs. stop the grinding). v2 should split
/// them, ideally distinguishing a cap exit that leaves resumable state from one
/// that leaves nothing, since only the second is unambiguously a failure. The
/// true reason is not lost meanwhile — `status` and `end_reason` both carry it
/// on newt's own trace line, where no external vocabulary is at stake.
pub fn outcome_label(t: Terminal) -> &'static str {
    match t {
        Terminal::Completed => "completed",
        // Budget exhausted without reaching the goal. `Cancelled` is
        // unreachable from this binary — it is set only in
        // `newt-tui/src/chat.rs` on an operator interrupt — but an abandoned
        // run did not finish either, so it files the same way rather than
        // falling through to a wildcard.
        Terminal::StoppedShort(TurnEndReason::RoundCap | TurnEndReason::Cancelled) => "timeout",
        // The model WAS reached and emitted unusable content, which is
        // `CONTRACT.md`'s `model_error` verbatim: "reached but errored
        // (refused, emitted invalid output)". `is_real_attempt()` includes it,
        // correctly — the model ran. This is a change from pre-#2215 behaviour
        // beyond the cap-exit case, made deliberately: `completed` for a
        // placeholder reply is the same falsehood #2212 identified.
        Terminal::StoppedShort(TurnEndReason::Empty) => "model_error",
        // The remaining variants cannot construct `StoppedShort`; listed so a
        // new one fails to compile rather than inheriting a bucket.
        Terminal::StoppedShort(
            TurnEndReason::Completed
            | TurnEndReason::NarrationCapExhausted
            | TurnEndReason::NarrationFinalRound
            | TurnEndReason::Failed,
        ) => "harness_error",
        Terminal::Failed(Some(ErrorClass::Model)) => "model_error",
        Terminal::Failed(Some(ErrorClass::Transport)) => "transport_error",
        Terminal::Failed(Some(ErrorClass::Timeout)) => "timeout",
        Terminal::Failed(Some(ErrorClass::Harness) | None) => "harness_error",
    }
}

/// The `solve_result` trace line's `status` — newt's own field, free to carry
/// a vocabulary no external contract constrains.
///
/// This is the half of #2212 that was a real defect: a run that stopped at a
/// wall reported itself `completed`, and every downstream signal — eval scores,
/// CI gates, an operator reading a summary — rested on that.
pub fn status_label(t: Terminal) -> &'static str {
    match t {
        Terminal::Completed => "completed",
        Terminal::StoppedShort(_) => "incomplete",
        Terminal::Failed(_) => "failed",
    }
}

/// One JSONL trace line per parse signal (the ADR §5 events
/// `recovered_tool_call{dialect}` / `no_parseable_tool_call`). These lines
/// carry no `contract_version`, so the bench's contract scan skips them.
pub fn parse_signal_line(signal: &ParseSignal) -> serde_json::Value {
    serde_json::to_value(signal).expect("ParseSignal serializes infallibly")
}

/// One JSONL trace line per output-behavior signal. Like parse signals, these
/// carry no `contract_version`, so they cannot be mistaken for contract rows.
pub fn behavior_signal_line(signal: &BehaviorSignal) -> serde_json::Value {
    serde_json::to_value(signal).expect("BehaviorSignal serializes infallibly")
}

/// Build THE contract record — exactly the `contract_version: "1"` fields.
/// Optional fields (`model_digest`, `effective_config.context_window`,
/// `timing.gen_tokens`/`tok_s`) are OMITTED when unknown, never nulled-in
/// with invented values.
pub fn contract_record(i: &ContractInputs<'_>) -> serde_json::Value {
    let mut timing = serde_json::json!({ "wall_ms": i.wall_ms });
    if let Some(gen) = i.gen_tokens {
        timing["gen_tokens"] = gen.into();
        // tok_s only when derivable: tokens AND a non-zero wall clock.
        if i.wall_ms > 0 {
            timing["tok_s"] = serde_json::json!(gen as f64 * 1000.0 / i.wall_ms as f64);
        }
    }
    let mut effective_config = serde_json::json!({
        "tenacity": i.tenacity,
        "cognition": i.cognition,
        "crew": i.crew,
        "ocap": i.ocap,
        "max_rounds": i.max_rounds,
    });
    if let Some(cw) = i.context_window {
        effective_config["context_window"] = cw.into();
    }
    let mut record = serde_json::json!({
        "contract_version": CONTRACT_VERSION,
        "requested_model": i.requested_model,
        "effective_model": i.effective_model,
        "outcome": i.outcome,
        "backend": { "name": i.backend_name, "kind": i.backend_kind },
        "agent": AGENT,
        "agent_version": env!("CARGO_PKG_VERSION"),
        "effective_config": effective_config,
        "timing": timing,
    });
    if let Some(digest) = i.model_digest {
        record["model_digest"] = digest.into();
    }
    record
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::{BehaviorSignal, ToolCallDialect};

    fn inputs() -> ContractInputs<'static> {
        ContractInputs {
            requested_model: "qwen3.6_35b",
            effective_model: "qwen3.6_35b",
            model_digest: None,
            backend_name: "dgx",
            backend_kind: "openai",
            outcome: "completed",
            context_window: Some(32768),
            tenacity: "standard",
            cognition: "default",
            crew: "off",
            ocap: "off",
            max_rounds: 40,
            wall_ms: 10_000,
            gen_tokens: Some(500),
        }
    }

    // --- one test per outcome class ---

    /// **The #2212 defect, as a test.** Tonight's dogfood run produced a
    /// correct implementation, ran out of rounds, never ran the test the task
    /// required, and reported `outcome: completed` with `end_reason: RoundCap`
    /// in the same JSON line. A cap-exit is not an error, and `outcome` was
    /// derived only from the absence of one.
    #[test]
    fn a_round_cap_exit_files_as_timeout_not_completed() {
        assert_eq!(
            outcome_label(terminal(true, None, Some(TurnEndReason::RoundCap))),
            "timeout",
            "a run that exhausted its round budget without reaching the goal \
             must be excluded from capability scoring — is_real_attempt() is \
             Completed|ModelError. #2215 emitted `round_cap`, which the bench's \
             closed enum cannot parse, so the row vanished instead."
        );
    }

    #[test]
    fn a_round_cap_exit_reports_an_incomplete_run() {
        assert_eq!(
            status_label(terminal(true, None, Some(TurnEndReason::RoundCap))),
            "incomplete",
            "a run that ended at the tool-round cap reported itself finished"
        );
    }

    /// The two fields answer different questions, so the SAME turn renders
    /// differently — deliberately, from one classification.
    #[test]
    fn one_classification_renders_two_honest_answers() {
        let t = terminal(true, None, Some(TurnEndReason::RoundCap));
        assert_eq!(outcome_label(t), "timeout", "excluded from scoring");
        assert_eq!(
            status_label(t),
            "incomplete",
            "and honestly named on our own line"
        );
    }

    #[test]
    fn clean_turn_is_completed() {
        assert_eq!(outcome_label(terminal(true, None, None)), "completed");
    }

    #[test]
    fn model_class_is_model_error() {
        assert_eq!(
            outcome_label(terminal(false, Some(ErrorClass::Model), None)),
            "model_error"
        );
    }

    #[test]
    fn transport_class_is_transport_error() {
        assert_eq!(
            outcome_label(terminal(false, Some(ErrorClass::Transport), None)),
            "transport_error"
        );
    }

    #[test]
    fn timeout_class_is_timeout() {
        assert_eq!(
            outcome_label(terminal(false, Some(ErrorClass::Timeout), None)),
            "timeout"
        );
    }

    #[test]
    fn harness_class_and_unclassified_failures_are_harness_error() {
        assert_eq!(
            outcome_label(terminal(false, Some(ErrorClass::Harness), None)),
            "harness_error"
        );
        // A spawn/thread failure never reached a dispatch — no class at all.
        // Fail-closed: it must not masquerade as a model result.
        assert_eq!(outcome_label(terminal(false, None, None)), "harness_error");
    }

    // --- one test per parse-status signal line ---

    #[test]
    fn no_parseable_tool_call_line_shape() {
        assert_eq!(
            parse_signal_line(&ParseSignal::NoParseableToolCall { round: 3 }),
            serde_json::json!({"kind": "no_parseable_tool_call", "round": 3})
        );
    }

    #[test]
    fn recovered_tool_call_line_names_the_dialect() {
        assert_eq!(
            parse_signal_line(&ParseSignal::RecoveredToolCall {
                round: 1,
                dialect: ToolCallDialect::FunctionTag,
            }),
            serde_json::json!({
                "kind": "recovered_tool_call", "round": 1, "dialect": "function_tag"
            })
        );
    }

    #[test]
    fn reasoning_overflow_line_carries_the_bounded_recovery_result() {
        let signal = BehaviorSignal::ReasoningOverflow {
            round: 0,
            reasoning_overflow_detected: true,
            continuation_attempted: true,
            continuation_succeeded: true,
            finish_reason: "length".into(),
            reasoning_tokens_estimate: 2500,
        };
        assert_eq!(
            behavior_signal_line(&signal),
            serde_json::json!({
                "kind": "reasoning_overflow",
                "round": 0,
                "reasoning_overflow_detected": true,
                "continuation_attempted": true,
                "continuation_succeeded": true,
                "finish_reason": "length",
                "reasoning_tokens_estimate": 2500,
            })
        );
    }

    #[test]
    fn chat_completion_finish_line_records_backend_reason() {
        let signal = BehaviorSignal::ChatCompletionFinish {
            round: 3,
            finish_reason: Some("length".into()),
        };
        assert_eq!(
            behavior_signal_line(&signal),
            serde_json::json!({
                "kind": "chat_completion_finish",
                "round": 3,
                "finish_reason": "length",
            })
        );
    }

    // --- the record itself ---

    /// The record round-trips as valid JSON with EXACTLY the contract-v1
    /// fields — no extras for the bench to trip on, none of ours missing.
    #[test]
    fn record_round_trips_with_exactly_the_contract_fields() {
        let record = contract_record(&inputs());
        // Round-trip through the wire form.
        let wire = record.to_string();
        let parsed: serde_json::Value = serde_json::from_str(&wire).expect("valid JSON");
        let keys: Vec<&str> = {
            let mut k: Vec<&str> = parsed.as_object().unwrap().keys().map(|s| &**s).collect();
            k.sort_unstable();
            k
        };
        assert_eq!(
            keys,
            vec![
                "agent",
                "agent_version",
                "backend",
                "contract_version",
                "effective_config",
                "effective_model",
                "outcome",
                "requested_model",
                "timing",
            ],
            "exactly the contract fields (model_digest absent: not supplied)"
        );
        assert_eq!(parsed["contract_version"], "1");
        assert_eq!(parsed["agent"], "newt-agent");
        assert_eq!(parsed["agent_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            parsed["backend"],
            serde_json::json!({"name": "dgx", "kind": "openai"})
        );
        assert_eq!(
            parsed["effective_config"],
            serde_json::json!({
                "context_window": 32768, "tenacity": "standard",
                "cognition": "default", "crew": "off", "ocap": "off",
                "max_rounds": 40
            })
        );
        // 500 tokens over 10s ⇒ 50 tok/s, derived — never measured twice.
        assert_eq!(
            parsed["timing"],
            serde_json::json!({"wall_ms": 10_000, "gen_tokens": 500, "tok_s": 50.0})
        );
    }

    /// `model_digest` appears ONLY when operator-supplied — never fabricated.
    #[test]
    fn model_digest_only_when_supplied() {
        let mut i = inputs();
        assert!(contract_record(&i).get("model_digest").is_none());
        i.model_digest = Some("a3f6…deadbeef");
        assert_eq!(contract_record(&i)["model_digest"], "a3f6…deadbeef");
    }

    /// Unknown optionals are OMITTED, not nulled: no usage ⇒ no `gen_tokens`
    /// / `tok_s`; no `--context-window` pin ⇒ no `context_window`.
    #[test]
    fn unknown_optionals_are_omitted_not_invented() {
        let mut i = inputs();
        i.gen_tokens = None;
        i.context_window = None;
        let record = contract_record(&i);
        assert_eq!(record["timing"], serde_json::json!({"wall_ms": 10_000}));
        assert!(record["effective_config"].get("context_window").is_none());
        // A zero wall clock cannot derive a rate.
        i.gen_tokens = Some(500);
        i.wall_ms = 0;
        assert!(contract_record(&i)["timing"].get("tok_s").is_none());
    }
}
