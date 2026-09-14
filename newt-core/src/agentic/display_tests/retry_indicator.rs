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

/// #2334: a changed response ID is named as observed, with its single retry; its twin, an
/// ordinary transport failure, keeps the class wording and the policy budget.
#[test]
fn retry_indicator_names_a_mixed_response_and_its_single_retry() {
    let mixed = crate::agentic::openai_sse::decode_response(
        b"data: {\"id\":\"a\",\"choices\":[]}\n\ndata: {\"id\":\"b\",\"choices\":[]}\n\ndata: [DONE]\n\n",
    )
    .unwrap_err();
    assert_eq!(
        retry_indicator_for(1, 6, std::time::Duration::from_secs(2), &mixed),
        "  ↻ stream response ID changed mid-response — retrying once in 2.0s…"
    );
    let transport = anyhow::anyhow!("vLLM request failed: fixture reset");
    assert!(
        retry_indicator_for(1, 6, std::time::Duration::from_secs(2), &transport)
            .ends_with("(retry 1/6)…")
    );
}
