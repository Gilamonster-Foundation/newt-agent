use super::*;

/// #2372: the probe's plain answer is ACCEPTED and returned. Any second
/// (`stream: true`) request would stream DIFFERENT text, so a display reissue
/// shows up as an extra request and as `Hello world` in the reply.
struct StreamHappyResponder;
impl Respond for StreamHappyResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            ndjson(&[
                serde_json::json!({"message": {"content": "Hello "}, "done": false}),
                serde_json::json!({
                    "message": {"content": "world"}, "done": true,
                    "prompt_eval_count": 7, "eval_count": 3
                }),
            ])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "probe answer"},
                "prompt_eval_count": 5, "eval_count": 2,
            }))
        }
    }
}

#[tokio::test]
async fn ollama_returns_the_accepted_probe_answer_without_a_reissue() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(StreamHappyResponder)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, usage, hallu) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

    assert_eq!(
        reply, "probe answer",
        "the gated answer, never a second generation"
    );
    assert!(!streamed, "nothing was streamed to a display");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "one generation request"
    );
    let u = usage.expect("the probe's usage");
    assert_eq!((u.input_tokens, u.output_tokens), (5, 2));
    assert_eq!(hallu, 0);
}

/// #2372: reasoning never reaches the accepted Ollama answer. The deleted
/// display stream's filter used to strip it; the probe path must now, for an
/// inline `<think>` block and #528's lone leading closer alike.
#[tokio::test]
async fn reasoning_does_not_leak_into_the_accepted_ollama_answer() {
    for content in ["<think>x</think>Done.", "x</think>Done."] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": content},
                "prompt_eval_count": 5, "eval_count": 2,
            })))
            .mount(&server)
            .await;

        let messages = msgs();
        let caveats = Caveats::top();
        let (reply, _streamed, _usage, _hallu) =
            chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
                .await
                .expect("dispatch");

        assert_eq!(reply, "Done.", "{content:?}");
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

struct EmptyStreamResponder;
impl Respond for EmptyStreamResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            ndjson(&[serde_json::json!({"message": {"content": ""}, "done": true})])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "probe says hi"},
                "prompt_eval_count": 5, "eval_count": 2,
            }))
        }
    }
}

#[tokio::test]
async fn empty_stream_falls_back_to_probe_content() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(EmptyStreamResponder)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, usage, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

    assert_eq!(reply, "probe says hi");
    assert!(!streamed, "fallback content was never streamed");
    assert_eq!(usage.unwrap().input_tokens, 5);
}

/// Regression for the DGX wedge: the non-streamed probe said "Let me verify
/// by looking...", then the streaming re-issue returned no tokens. The probe
/// fallback must still go through the no-tool nudge gate instead of ending
/// the turn and forcing the operator to type "continue".
struct EmptyStreamPendingActionResponder {
    probes: Arc<AtomicUsize>,
}
impl Respond for EmptyStreamPendingActionResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            return ndjson(&[serde_json::json!({
                "message": {"content": ""},
                "done": true,
                "prompt_eval_count": 6,
                "eval_count": 0
            })]);
        }
        let probe = self.probes.fetch_add(1, Ordering::SeqCst);
        let content = if probe == 0 {
            "Now I understand the issue. Let me verify by looking at format_rollup_detail."
        } else {
            "Verified after the automatic continue."
        };
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"content": content},
            "prompt_eval_count": 5 + probe as u32,
            "eval_count": 2,
        }))
    }
}

#[tokio::test]
async fn empty_stream_probe_fallback_pending_action_nudges_and_continues() {
    let server = MockServer::start().await;
    let probes = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(EmptyStreamPendingActionResponder {
            probes: probes.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _usage, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("chat_complete should auto-continue after pending probe fallback");

    assert_eq!(
        probes.load(Ordering::SeqCst),
        2,
        "the nudge ran a second probe"
    );
    assert_eq!(reply, "Verified after the automatic continue.");
    assert!(!streamed, "the second answer also came from probe fallback");
    assert!(
        !reply.contains("Let me verify"),
        "must not return the pending-action narration"
    );
}

/// Probe AND stream both empty, with no safe-context hint → the loop gives
/// the explicit empty-response diagnostic instead of silence.
struct AllEmptyResponder;
impl Respond for AllEmptyResponder {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            ndjson(&[serde_json::json!({"message": {"content": ""}, "done": true})])
        } else {
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"message": {"content": ""}}))
        }
    }
}

#[tokio::test]
async fn fully_empty_response_yields_diagnostic_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(AllEmptyResponder)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

    assert!(
        reply.contains("model returned an empty response"),
        "got: {reply}"
    );
    assert!(reply.contains("newt doctor"), "points at diagnostics");
    assert!(!streamed);
}

struct SuspiciousEmptyThenRecover {
    probes: Arc<AtomicUsize>,
    saw_nudge: Arc<AtomicBool>,
}
impl Respond for SuspiciousEmptyThenRecover {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            if self.probes.load(Ordering::SeqCst) <= 1 {
                ndjson(&[serde_json::json!({
                    "message": {"content": ""},
                    "done": true,
                    "prompt_eval_count": 9,
                    "eval_count": 4
                })])
            } else {
                ndjson(&[
                    serde_json::json!({"message": {"content": "recovered "}, "done": false}),
                    serde_json::json!({
                        "message": {"content": "after empty retry"},
                        "done": true,
                        "prompt_eval_count": 5,
                        "eval_count": 3
                    }),
                ])
            }
        } else {
            let body = body_json(req);
            if body["messages"].as_array().into_iter().flatten().any(|m| {
                m["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("no assistant-visible content")
            }) {
                self.saw_nudge.store(true, Ordering::SeqCst);
            }
            let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "thinking": "I know what to do but did not emit final text."
                    },
                    "prompt_eval_count": 10,
                    "eval_count": 2559,
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "recovered after empty retry"},
                    "prompt_eval_count": 5,
                    "eval_count": 3,
                }))
            }
        }
    }
}

#[tokio::test]
async fn suspicious_empty_generated_output_retries_with_nudge() {
    let server = MockServer::start().await;
    let probes = Arc::new(AtomicUsize::new(0));
    let saw_nudge = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(SuspiciousEmptyThenRecover {
            probes: probes.clone(),
            saw_nudge: saw_nudge.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, usage, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("chat_complete should succeed");

    assert_eq!(reply, "recovered after empty retry");
    assert!(!streamed, "the host renders the accepted answer (#2372)");
    assert_eq!(probes.load(Ordering::SeqCst), 2);
    assert!(saw_nudge.load(Ordering::SeqCst));
    assert_eq!(
        usage
            .expect("usage survives suspicious retry")
            .output_tokens,
        2_559 + 3,
        "usage from the suspicious empty round must be preserved"
    );
}

struct SuspiciousEmptyTwiceThenRecover {
    probes: Arc<AtomicUsize>,
    saw_strong_nudge: Arc<AtomicBool>,
}
impl Respond for SuspiciousEmptyTwiceThenRecover {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            if self.probes.load(Ordering::SeqCst) <= 2 {
                ndjson(&[serde_json::json!({
                    "message": {"content": ""},
                    "done": true,
                    "prompt_eval_count": 9,
                    "eval_count": 4
                })])
            } else {
                ndjson(&[serde_json::json!({
                    "message": {"content": "recovered after strong hidden-only nudge"},
                    "done": true,
                    "prompt_eval_count": 5,
                    "eval_count": 3
                })])
            }
        } else {
            let body = body_json(req);
            if body["messages"].as_array().into_iter().flatten().any(|m| {
                m["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Hidden thinking is not an action")
            }) {
                self.saw_strong_nudge.store(true, Ordering::SeqCst);
            }
            let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= 2 {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {
                        "content": "",
                        "thinking": "I know the next action but did not emit it."
                    },
                    "prompt_eval_count": 10,
                    "eval_count": 2559,
                }))
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "message": {"content": "recovered after strong hidden-only nudge"},
                    "prompt_eval_count": 5,
                    "eval_count": 3,
                }))
            }
        }
    }
}

#[tokio::test]
async fn repeated_thinking_only_gets_stronger_second_nudge() {
    let server = MockServer::start().await;
    let probes = Arc::new(AtomicUsize::new(0));
    let saw_strong_nudge = Arc::new(AtomicBool::new(false));
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(SuspiciousEmptyTwiceThenRecover {
            probes: probes.clone(),
            saw_strong_nudge: saw_strong_nudge.clone(),
        })
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let (reply, streamed, _, _) =
        chat_complete(ctx(&server.uri(), &messages, &caveats), &mut NoMcp)
            .await
            .expect("second hidden-only nudge should recover the turn");

    assert_eq!(reply, "recovered after strong hidden-only nudge");
    assert!(!streamed, "the host renders the accepted answer (#2372)");
    assert_eq!(probes.load(Ordering::SeqCst), 3);
    assert!(saw_strong_nudge.load(Ordering::SeqCst));
}

struct SuspiciousEmptyStaysEmpty;
impl Respond for SuspiciousEmptyStaysEmpty {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        if is_stream(req) {
            ndjson(&[serde_json::json!({
                "message": {"content": ""},
                "done": true,
                "prompt_eval_count": 9,
                "eval_count": 4
            })])
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {
                    "content": "",
                    "reasoning_content": "internal-only response"
                },
                "prompt_eval_count": 10,
                "eval_count": 12,
            }))
        }
    }
}

#[tokio::test]
async fn suspicious_empty_generated_output_reports_targeted_diagnostic() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat"))
        .respond_with(SuspiciousEmptyStaysEmpty)
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.trace = true;
    let (reply, streamed, _, _) = chat_complete(c, &mut NoMcp)
        .await
        .expect("chat_complete should succeed");

    assert!(reply.contains("generated output tokens"), "got: {reply}");
    assert!(
        reply.contains("reasoning_content"),
        "diagnostic should name the non-content field: {reply}"
    );
    assert!(reply.contains("--trace"), "points at trace diagnostics");
    assert!(!streamed);
}

#[tokio::test]
async fn openai_empty_content_yields_diagnostic_message() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": ""}}]
        })))
        .mount(&server)
        .await;

    let messages = msgs();
    let caveats = Caveats::top();
    let uri = server.uri();
    let mut c = ctx(&uri, &messages, &caveats);
    c.kind = BackendKind::Openai;
    let (reply, _, _, _) = chat_complete(c, &mut NoMcp).await.expect("should succeed");
    assert!(
        reply.contains("model returned an empty response"),
        "got: {reply}"
    );
}
