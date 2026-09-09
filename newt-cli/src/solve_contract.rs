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

/// How many tool calls the run spent AFTER its last SUCCESSFUL workspace
/// write — the write-complete-then-grind measurement (#2214).
///
/// `RoundCap` alone cannot separate three runs that all hit the same wall:
/// thrash (rounds spent on failures), a genuinely-too-small cap (rounds spent
/// on real remaining work), and grind (rounds spent re-verifying work already
/// finished). This is the third one as a value a gate can assert, rather than
/// a substring of the advice prose the reply happens to carry.
///
/// `None` when the run never landed a successful write. That is the "never
/// acted" case — a distinct failure class, not a grind of length zero — and
/// collapsing the two would report every unproductive run as a grind.
///
/// Note it gates on `ok`, while the neighbouring `write_calls` on the same
/// line counts by NAME only. The two can legitimately disagree: a run whose
/// three writes were all DENIED reports `write_calls: 3` and `null` here.
pub fn calls_after_last_write(events: &[newt_core::ToolEvent]) -> Option<usize> {
    let last = events
        .iter()
        .rposition(|e| e.ok && newt_core::agentic::is_workspace_write_call(&e.tool))?;
    Some(events.len() - 1 - last)
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

/// The consumer's permitted `outcome` values, checked in beside this code.
///
/// See the file's own header for why a copy is the right shape here. In short:
/// the authority lives in another repository that deliberately shares no type
/// with this one, so nothing in this workspace can fail on a contract
/// violation — and #2218 recorded that gap after `round_cap` silently deleted
/// bench rows. This constant, and the test that drives every reachable
/// classification through it, is that gap closed at the one place newt chooses
/// a wire value.
#[cfg(test)]
const BENCH_OUTCOME_VALUES: &str = include_str!("../contract/bench_outcome_values_v1.txt");

/// Parse the checked-in permitted set: one value per line, `#` comments and
/// blank lines ignored.
#[cfg(test)]
fn permitted_outcomes() -> Vec<&'static str> {
    BENCH_OUTCOME_VALUES
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::{BehaviorSignal, ToolCallDialect};

    /// Every `TurnEndReason`, and a compile-time guard that this list stays
    /// complete.
    ///
    /// The guard is the load-bearing half. A hand-written list of variants is
    /// the classic vacuous negative (#2150): add an eighth variant and the
    /// coverage test still passes, having simply stopped looking at it. The
    /// index match below has no wildcard arm, so a new variant fails to
    /// COMPILE; and the density assertion in
    /// `every_reachable_outcome_is_a_value_the_bench_can_parse` fails unless
    /// the new arm's index is also added to this array. Neither direction can
    /// be satisfied by ignoring it.
    const ALL_END_REASONS: &[TurnEndReason] = &[
        TurnEndReason::Completed,
        TurnEndReason::NarrationCapExhausted,
        TurnEndReason::NarrationFinalRound,
        TurnEndReason::RoundCap,
        TurnEndReason::Empty,
        TurnEndReason::Cancelled,
        TurnEndReason::Failed,
    ];

    fn tool_event(tool: &str, ok: bool) -> newt_core::ToolEvent {
        newt_core::ToolEvent::from_call(tool, &serde_json::json!({"path": tool}), ok, None)
    }

    /// The "never acted" run is NOT a grind of length zero. `write_calls`
    /// already distinguishes a run that never wrote; this field must not
    /// re-report that run as "finished the work then ground", which is what
    /// any `unwrap_or(0)` or `unwrap_or(len)` fallback would do.
    #[test]
    fn a_run_that_never_landed_a_write_has_no_grind_measurement() {
        let events = [
            tool_event("read_file", true),
            // A write that FAILED is not a write. This is the ok-gate that
            // separates this field from `write_calls`, which counts by name.
            tool_event("write_file", false),
            tool_event("run_command", true),
        ];
        assert_eq!(calls_after_last_write(&events), None);
        assert_eq!(calls_after_last_write(&[]), None);
    }

    /// The other end: a run whose last act WAS the write ground for zero
    /// calls. Distinguishing this from `None` is the whole point of the
    /// `Option` — both are "no grind", for opposite reasons.
    #[test]
    fn a_write_as_the_final_call_is_a_grind_of_zero() {
        assert_eq!(
            calls_after_last_write(&[tool_event("read_file", true), tool_event("edit_file", true)]),
            Some(0)
        );
    }

    /// The measured shape from #2212, in miniature: writes early, then a tail
    /// of succeeding, redundant calls. Only the LAST successful write counts —
    /// an implementation keying on the FIRST would say 4 here.
    #[test]
    fn the_grind_is_measured_from_the_last_successful_write() {
        let events = [
            tool_event("write_file", true),
            tool_event("edit_file", true),
            tool_event("read_file", true),
            tool_event("text_search", true),
        ];
        assert_eq!(calls_after_last_write(&events), Some(2));
    }

    fn end_reason_index(r: TurnEndReason) -> usize {
        match r {
            TurnEndReason::Completed => 0,
            TurnEndReason::NarrationCapExhausted => 1,
            TurnEndReason::NarrationFinalRound => 2,
            TurnEndReason::RoundCap => 3,
            TurnEndReason::Empty => 4,
            TurnEndReason::Cancelled => 5,
            TurnEndReason::Failed => 6,
        }
    }

    /// Every `ErrorClass`, guarded the same way.
    const ALL_ERROR_CLASSES: &[ErrorClass] = &[
        ErrorClass::Model,
        ErrorClass::Transport,
        ErrorClass::Timeout,
        ErrorClass::Harness,
    ];

    fn error_class_index(c: ErrorClass) -> usize {
        match c {
            ErrorClass::Model => 0,
            ErrorClass::Transport => 1,
            ErrorClass::Timeout => 2,
            ErrorClass::Harness => 3,
        }
    }

    /// Every `Terminal` this binary can construct.
    fn all_terminals() -> Vec<Terminal> {
        let mut out = vec![Terminal::Completed, Terminal::Failed(None)];
        out.extend(ALL_END_REASONS.iter().copied().map(Terminal::StoppedShort));
        out.extend(
            ALL_ERROR_CLASSES
                .iter()
                .copied()
                .map(|c| Terminal::Failed(Some(c))),
        );
        out
    }

    /// **#2227 layer 2, and the specific gate that failed in #2215.**
    ///
    /// A green CI in this repo cannot mean "contract-conforming": the bench
    /// re-declares its own structs so the ruler stays independent of the thing
    /// it measures, which is correct and must not be traded away. The cost is
    /// that a wire value newt invents is caught by nobody — and because the
    /// consumer's enum is closed, an unparseable value does not score badly,
    /// it removes the row from the matrix.
    ///
    /// This drives EVERY reachable classification through `outcome_label` and
    /// requires the result to be a value the checked-in permitted set contains.
    /// It would have failed on `round_cap` before #2215 merged.
    #[test]
    fn every_reachable_outcome_is_a_value_the_bench_can_parse() {
        // The variant lists must be complete before anything derived from them
        // proves a thing. Indices are dense and unique iff every arm of the
        // (wildcard-free) index match appears exactly once in the array.
        let mut seen: Vec<usize> = ALL_END_REASONS
            .iter()
            .copied()
            .map(end_reason_index)
            .collect();
        seen.sort_unstable();
        assert_eq!(
            seen,
            (0..ALL_END_REASONS.len()).collect::<Vec<_>>(),
            "ALL_END_REASONS stopped covering TurnEndReason — a variant was \
             added to the index match but not to the array, so the coverage \
             below is measuring less than it appears to"
        );
        let mut seen: Vec<usize> = ALL_ERROR_CLASSES
            .iter()
            .copied()
            .map(error_class_index)
            .collect();
        seen.sort_unstable();
        assert_eq!(
            seen,
            (0..ALL_ERROR_CLASSES.len()).collect::<Vec<_>>(),
            "ALL_ERROR_CLASSES stopped covering ErrorClass"
        );

        let permitted = permitted_outcomes();
        assert!(
            !permitted.is_empty(),
            "the permitted set parsed empty — a checked-in file that reads as \
             'nothing is allowed' would fail every case below for the wrong \
             reason, and one that reads as 'everything' would pass them all"
        );
        for t in all_terminals() {
            let label = outcome_label(t);
            assert!(
                permitted.contains(&label),
                "outcome_label({t:?}) emitted {label:?}, which \
                 gilamonster-bench's closed `Outcome` enum cannot deserialize. \
                 The run will not score badly — its row will VANISH from the \
                 matrix. Permitted: {permitted:?}. If the contract genuinely \
                 moved, update contract/bench_outcome_values_v1.txt against the \
                 upstream enum, in its own commit."
            );
        }
    }

    /// The permitted set is a copy of a specific upstream shape, so it is
    /// pinned exactly rather than merely non-empty. A silent edit — adding a
    /// value to make a failing case pass — is the failure mode the file exists
    /// to prevent, and changing this list is how a reviewer is made to look.
    #[test]
    fn the_checked_in_permitted_set_matches_contract_version_one() {
        assert_eq!(
            permitted_outcomes(),
            vec![
                "completed",
                "model_error",
                "transport_error",
                "timeout",
                "harness_error"
            ],
            "the checked-in copy of gilamonster-bench's `Outcome` changed. That \
             is a deliberate act tracking an upstream contract change, not a \
             way to make a test pass — re-read `gilamonster-bench/src/\
             contract.rs` and confirm `contract_version` before editing this."
        );
    }

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
