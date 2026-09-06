use super::*;

/// Probe (stream:false) answers with plain content; the streaming re-issue
/// (stream:true) returns NDJSON tokens with usage on the `done` chunk.
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
async fn ollama_streams_final_answer_and_merges_usage() {
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

    assert_eq!(reply, "Hello world", "tokens accumulated across chunks");
    assert!(streamed, "the streaming path printed the tokens");
    let u = usage.expect("probe + stream usage merged");
    // SEMANTICS CHANGED in Step 18.1: both requests carried the same
    // conversation, so input is max(5, 7) = 7 — the old sum (12) counted
    // the shared history twice. Output is still 2 + 3 (new generation).
    assert_eq!(u.input_tokens, 7, "max(5 probe, 7 stream), not the sum");
    assert_eq!(u.output_tokens, 5, "2 (probe) + 3 (stream)");
    assert_eq!(hallu, 0);
}

/// The streaming re-issue produces no tokens — the loop must fall back to
/// the probe round's content rather than returning silence.
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
    assert!(streamed);
    assert_eq!(probes.load(Ordering::SeqCst), 2);
    assert!(saw_nudge.load(Ordering::SeqCst));
    assert!(
        usage
            .expect("usage survives suspicious retry")
            .output_tokens
            >= 2566,
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
    assert!(streamed);
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
