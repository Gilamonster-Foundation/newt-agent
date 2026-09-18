use super::*;

/// #2268 deliberately extends the closed wire vocabulary: capacity
/// rejection must survive typed dispatch classification into the record.
#[test]
fn context_exceeded_is_an_explicit_bench_outcome() {
    let error = anyhow::Error::new(newt_core::agentic::DispatchError::http_status(
        "inference endpoint 500: Context size has been exceeded.".into(),
    ));
    let outcome = outcome_label(terminal(
        false,
        newt_core::agentic::error_class(&error),
        None,
        false,
    ));
    assert_eq!(outcome, "context_exceeded");
    assert!(permitted_outcomes().contains(&outcome));
}

/// #2313 (d): the `usage` stanza keeps a real zero cost, omits an unknown one,
/// and carries `ledger_head` only when the lines were emitted.
#[test]
fn usage_stanza_keeps_zero_cost_and_omits_the_unknown() {
    let totals = newt_core::attempts::UsageTotals {
        attempts: 2,
        usage_missing: 0,
        in_tokens: 64,
        out_tokens: 24,
        usage_complete: true,
    };
    let free = usage_stanza(&totals, Some(0.0), Some("bafy-head"));
    assert_eq!(free["cost_usd"], 0.0);
    assert_eq!(free["ledger_head"], "bafy-head");
    assert_eq!(free["attempts"], 2);
    assert_eq!(free["in_tokens"], 64);
    let unknown = usage_stanza(&totals, None, None);
    assert!(unknown.get("cost_usd").is_none(), "{unknown}");
    assert!(unknown.get("ledger_head").is_none(), "{unknown}");
}

/// #2313 (d): an attempt trace line is the journal line itself, tagged
/// `kind: "attempt"`, and still reads back as a verifiable chain line.
#[test]
fn an_attempt_trace_line_reads_back_as_a_chain_line() {
    use newt_core::attempts::{AttemptLedger, AttemptRecord, AttemptState};
    let mut ledger = AttemptLedger::default();
    let key = ledger.dispatch("turn-1", "primary", b"round 0");
    let record = AttemptRecord::new(&key, "model", "backend", None, AttemptState::Ok).unwrap();
    let line = ledger.observe(record).unwrap();
    let traced = attempt_line(&line);
    assert_eq!(traced["kind"], "attempt");
    let back: newt_core::event_journal::JournalLine<AttemptRecord> =
        serde_json::from_value(traced).unwrap();
    assert_eq!(back, line);
    let head = ledger.head().unwrap().to_string();
    assert_eq!(
        newt_core::event_journal::verify_chain(&[back], Some(&head)),
        vec![]
    );
}
