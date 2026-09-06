use super::*;

/// The cap-exit summary round returns 200 with EMPTY content: the loop
/// must surface the named fallback, not the empty string.
struct EmptyFinalSummary;
impl Respond for EmptyFinalSummary {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if body_json(req).get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                }]}
            }))
        } else {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"message": {"content": ""}}))
        }
    }
}

#[tokio::test]
async fn empty_final_summary_yields_cap_fallback() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(EmptyFinalSummary)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.max_tool_rounds = 2;
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert!(reply.contains("tool-round limit (2"), "got: {reply}");
    assert!(
        reply.contains("increase the tool-round limit"),
        "gives caller-neutral recovery advice"
    );
}

/// #867 regression: the cap-exit summary cites a file that does not
/// exist (the forensic transcript's exact shape — evidence trimmed, the
/// model reconstructs a plausible path). The claim check must append a
/// visible refutation naming the path, while leaving the model's prose
/// intact as a prefix.
struct HallucinatingFinalSummary;
impl Respond for HallucinatingFinalSummary {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if body_json(req).get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "", "tool_calls": [{
                    "function": {"name": "definitely_not_a_real_tool", "arguments": {}}
                }]}
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content":
                    "The /end command is defined in newt-tui/src/commands.rs \
                     (lines 38-40) as enum variants."}
            }))
        }
    }
}

#[tokio::test]
async fn cap_exit_hallucinated_path_gets_claim_check_refutation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(HallucinatingFinalSummary)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.max_tool_rounds = 2;
    // `ctx` sets a workspace that does not exist, so the cited
    // `newt-tui/src/commands.rs` provably does not resolve in it.
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert!(
        reply.contains("newt-tui/src/commands.rs (lines 38-40)"),
        "the model's prose is preserved verbatim: {reply}"
    );
    assert!(reply.contains("⚠ claim check (#867)"), "got: {reply}");
    assert!(
        reply.contains("`newt-tui/src/commands.rs`"),
        "the fabricated path is named in the refutation: {reply}"
    );
}

/// #1964 regression: before the fix, `finalize_cap_exit_text` (now
/// `finalize_final_text`) ran ONLY on the round-cap path — a normal finish
/// (the model answers with no tool calls, well inside the round budget)
/// returned straight through, unannotated even when it hallucinated the
/// same file path the cap-exit summary would have been checked for. Same
/// content as [`HallucinatingFinalSummary`] above, served on the FIRST
/// round with no cap in play, on both the non-streamed probe and the
/// streaming re-issue (the wire always re-issues once the probe returns no
/// tool calls).
struct HallucinatingFirstAnswer;
impl Respond for HallucinatingFirstAnswer {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let content = "The /end command is defined in newt-tui/src/commands.rs \
                       (lines 38-40) as enum variants.";
        if is_stream(req) {
            ndjson(&[serde_json::json!({
                "message": {"content": content}, "done": true,
                "prompt_eval_count": 5, "eval_count": 2
            })])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": content},
                "prompt_eval_count": 5, "eval_count": 2,
            }))
        }
    }
}

#[tokio::test]
async fn normal_finish_hallucinated_path_gets_claim_check_refutation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(HallucinatingFirstAnswer)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    // Default (uncapped) `max_tool_rounds` from `ctx` — this must end the
    // turn on round 1, never touching the round-cap path.
    let c = ctx(&uri, &messages, &caveats);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert!(
        reply.contains("newt-tui/src/commands.rs (lines 38-40)"),
        "the model's prose is preserved verbatim: {reply}"
    );
    assert!(
        reply.contains("⚠ claim check (#867)"),
        "a normal finish must get the same claim check as a cap-exit summary: {reply}"
    );
    assert!(
        reply.contains("`newt-tui/src/commands.rs`"),
        "the fabricated path is named in the refutation: {reply}"
    );
}

/// Twin of the above: an honest, claim-free normal finish must not grow a
/// trailing annotation — the check is append-only-on-refutation, never
/// noise on a clean answer.
struct CleanFirstAnswer;
impl Respond for CleanFirstAnswer {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let content = "The task is already complete; nothing further to do.";
        if is_stream(req) {
            ndjson(&[serde_json::json!({
                "message": {"content": content}, "done": true,
                "prompt_eval_count": 5, "eval_count": 2
            })])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": content},
                "prompt_eval_count": 5, "eval_count": 2,
            }))
        }
    }
}

#[tokio::test]
async fn clean_normal_finish_gets_no_annotation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(CleanFirstAnswer)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let c = ctx(&uri, &messages, &caveats);
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert_eq!(
        reply, "The task is already complete; nothing further to do.",
        "a clean normal finish must pass through byte-for-byte: {reply}"
    );
    assert!(!reply.contains("⚠ claim check"), "got: {reply}");
}

/// OpenAI mirror of the Ollama cap-exit fallback: tool calls until the cap,
/// then a 400 on the tools-disabled summary → the named fallback.
struct OpenAiErrOnFinal;
impl Respond for OpenAiErrOnFinal {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if body_json(req).get("tools").is_some() {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "definitely_not_a_real_tool", "arguments": "{}"}
                    }]
                }}]
            }))
        } else {
            ResponseTemplate::new(400).set_body_string("bad request")
        }
    }
}

#[tokio::test]
async fn openai_cap_exit_fallback_when_final_summary_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(OpenAiErrOnFinal)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    c.max_tool_rounds = 2;
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("must succeed even when the summary errors");
    assert!(reply.contains("tool-round limit (2"), "got: {reply}");
    assert!(reply.contains("increase the tool-round limit"));
}
