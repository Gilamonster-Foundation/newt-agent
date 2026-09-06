use super::*;

#[test]
fn ollama_auth_headers_builds_sensitive_bearer_or_nothing() {
    let h = ollama_auth_headers(Some("ol-cloud-key"));
    let v = h.get(reqwest::header::AUTHORIZATION).expect("header set");
    assert_eq!(v.to_str().unwrap(), "Bearer ol-cloud-key");
    assert!(v.is_sensitive(), "token must never reach debug logs");
    assert!(ollama_auth_headers(None).is_empty());
    assert!(ollama_auth_headers(Some("   ")).is_empty());
}

#[tokio::test]
async fn ollama_loop_sends_bearer_auth_on_every_request_when_key_configured() {
    // Field regression (Ollama Cloud 401): the wire spoke plain HTTP with
    // the key dropped on the floor. Every request the loop makes — tool
    // rounds AND the final summary — must now carry the bearer.
    let server = MockServer::start().await;
    let served = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(OllamaResponder {
            tool_rounds_served: served.clone(),
            final_answer: "authed answer".into(),
        })
        .mount(&server)
        .await;
    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut ctx = hard_budget_ctx(
        &uri,
        &messages,
        &caveats,
        "do the thing",
        BackendKind::Ollama,
    );
    ctx.api_key = Some("ol-cloud-key");
    ctx.safe_context = None;
    let (reply, _, _, _) = chat_complete(ctx, &mut NoMcp)
        .await
        .expect("turn completes");
    assert_eq!(reply, "authed answer");
    let reqs = server.received_requests().await.expect("journal");
    assert!(!reqs.is_empty());
    for r in &reqs {
        assert_eq!(
            r.headers.get("authorization").map(|v| v.to_str().unwrap()),
            Some("Bearer ol-cloud-key"),
            "unauthenticated request slipped through to {}",
            r.url
        );
    }
}
