use super::*;

#[test]
fn retry_indicator_names_timeout_and_retry_budget() {
    assert_eq!(
        retry_indicator_text(
            1,
            1,
            std::time::Duration::from_secs(2),
            Some(ErrorClass::Timeout),
        ),
        "  ↻ request timed out — retrying in 2.0s (retry 1/1)…"
    );
}

#[test]
fn retry_indicator_distinguishes_transport_from_model_errors() {
    assert!(retry_indicator_text(
        2,
        4,
        std::time::Duration::from_millis(750),
        Some(ErrorClass::Transport),
    )
    .contains("connection lost"));
    assert!(retry_indicator_text(
        2,
        4,
        std::time::Duration::from_millis(750),
        Some(ErrorClass::Model),
    )
    .contains("backend returned a retryable error"));
}
