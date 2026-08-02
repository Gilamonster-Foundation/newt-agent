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

use newt_core::{BehaviorSignal, ErrorClass, ParseSignal};

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

/// Map the turn result to the contract `outcome` taxonomy. `clean` = the turn
/// completed with no error; a failed turn takes its TYPED class, and a
/// failure with no class at all (spawn/thread error before any dispatch)
/// files as `harness_error` — fail-closed, never a guess from message text.
pub fn outcome_label(clean: bool, class: Option<ErrorClass>) -> &'static str {
    if clean {
        return "completed";
    }
    match class {
        Some(ErrorClass::Model) => "model_error",
        Some(ErrorClass::Transport) => "transport_error",
        Some(ErrorClass::Timeout) => "timeout",
        Some(ErrorClass::Harness) | None => "harness_error",
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

    #[test]
    fn clean_turn_is_completed() {
        assert_eq!(outcome_label(true, None), "completed");
    }

    #[test]
    fn model_class_is_model_error() {
        assert_eq!(outcome_label(false, Some(ErrorClass::Model)), "model_error");
    }

    #[test]
    fn transport_class_is_transport_error() {
        assert_eq!(
            outcome_label(false, Some(ErrorClass::Transport)),
            "transport_error"
        );
    }

    #[test]
    fn timeout_class_is_timeout() {
        assert_eq!(outcome_label(false, Some(ErrorClass::Timeout)), "timeout");
    }

    #[test]
    fn harness_class_and_unclassified_failures_are_harness_error() {
        assert_eq!(
            outcome_label(false, Some(ErrorClass::Harness)),
            "harness_error"
        );
        // A spawn/thread failure never reached a dispatch — no class at all.
        // Fail-closed: it must not masquerade as a model result.
        assert_eq!(outcome_label(false, None), "harness_error");
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
