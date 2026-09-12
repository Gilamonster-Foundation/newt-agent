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
