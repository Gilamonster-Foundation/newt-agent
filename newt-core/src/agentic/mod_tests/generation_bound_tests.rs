//! #2782: continuing transport progress must not buy unbounded generation.
use super::*;

#[tokio::test]
async fn bounds_2782_primary_requests_have_output_caps() {
    for responses in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(if responses {
                json!({"status":"completed","output":[{"type":"message","role":"assistant",
                    "content":[{"type":"output_text","text":"Finished."}]}],
                    "usage":{"input_tokens":20,"output_tokens":3}})
            } else {
                json!({"choices":[{"message":{"content":"Finished."},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":20,"completion_tokens":3}})
            }))
            .mount(&server)
            .await;
        let uri = server.uri();
        let messages = msgs();
        let caveats = Caveats::top();
        let mut context = ctx(&uri, &messages, &caveats);
        context.action_nudges = false;
        context.output_allowance = Some(1234);
        context.openai_api = if responses {
            crate::OpenAiApi::Responses
        } else {
            crate::OpenAiApi::ChatCompletions
        };
        chat_complete(context, &mut NoMcp).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let body = body_json(&requests[0]);
        let field = if responses {
            "max_output_tokens"
        } else {
            "max_tokens"
        };
        assert_eq!(
            body[field], 1234,
            "#2782: bounded output on both wires: {body}"
        );
    }
}

#[tokio::test]
async fn bounds_2782_steady_trickle_stops_at_total_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    let (tick, mut ticks) = tokio::sync::mpsc::unbounded_channel::<u32>();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: ready\n\n").await.unwrap();
        let mut closed = [0; 1];
        loop {
            tokio::select! {
                n = ticks.recv() => {
                    let Some(n) = n else { break; };
                    let frame = format!("data: {}\n\n", json!({"choices":[{"delta":{"content":format!("step {n} ")}}]}));
                    if socket.write_all(frame.as_bytes()).await.is_err() { break; }
                }
                result = socket.read(&mut closed) => {
                    assert!(matches!(result, Ok(0) | Err(_)), "no further client request expected");
                    break;
                }
            }
        }
    });
    let messages = msgs();
    let caveats = Caveats::top();
    let ledger = std::sync::Mutex::new(crate::attempts::AttemptLedger::default());
    let mut context = ctx(&uri, &messages, &caveats);
    context.attempt_ledger = Some(&ledger);
    context.action_nudges = false;
    context.inference_timeout_secs = 120;
    let consumed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut tools = NoMcp;
    let mut completion = Box::pin(
        crate::retry::RESPONSE_BYTES_READ
            .scope(consumed.clone(), chat_complete(context, &mut tools)),
    );
    while consumed.load(Ordering::SeqCst) == 0 {
        tokio::select! {
            result = &mut completion => panic!("ended before initial body: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
    }
    tokio::time::pause();
    let mut stopped = None;
    for n in 1..=16 {
        tokio::time::advance(Duration::from_secs(60)).await;
        let previous = consumed.load(Ordering::SeqCst);
        let _ = tick.send(n);
        for _ in 0..10_000 {
            tokio::select! {
                biased;
                result = &mut completion => { stopped = Some(result); break; }
                _ = tokio::task::yield_now() => {}
            }
            if consumed.load(Ordering::SeqCst) > previous {
                break;
            }
        }
        if stopped.is_some() {
            break;
        }
        assert!(
            consumed.load(Ordering::SeqCst) > previous,
            "fixture chunk consumed"
        );
    }
    drop(completion);
    for _ in 0..10_000 {
        if server.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        server.is_finished(),
        "#2782: stopped generation releases the HTTP connection"
    );
    server.await.unwrap();
    let error = stopped
        .expect("#2782: steady bytes must stop by the 900-second total bound")
        .unwrap_err();
    assert!(error.to_string().contains("total generation"), "{error:#}");
    assert_eq!(
        crate::retry::classify(&error),
        crate::retry::Retryability::Fatal
    );
    let ledger = ledger.lock().unwrap();
    let records: Vec<_> = ledger.records().collect();
    assert_eq!(records.len(), 1, "limit stops cannot purchase a retry");
    assert_eq!(records[0].state, crate::attempts::AttemptState::Failed);
}

/// #2782: repeated generated text, including hidden reasoning, stops one attempt;
/// repeated SSE scaffolding alone and an ordinary completed response are harmless.
#[tokio::test]
async fn bounds_2782_repetition_and_normal_completion() {
    let _env = crate::process_env::lock();
    let _enabled =
        crate::agentic::tools::disable_ocap_tests::EnvVar::set("NEWT_GENERATION_REPEAT_LIMIT", "8");
    for (field, repeated, cap) in [
        ("content", true, false),
        ("reasoning_content", true, false),
        ("content", true, true),
        ("content", false, false),
    ] {
        let server = MockServer::start().await;
        let mut stream =
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":7}}\n\n"
                .to_string();
        for n in 0..12 {
            let text = if repeated {
                "This is a long repeated model thought that never makes any further progress. "
                    .to_string()
            } else {
                format!("Unique step {n}: {}", "x".repeat(n))
            };
            stream.push_str(&format!(
                "data: {}\n\n",
                json!({"choices":[{"delta":{field:text}}]})
            ));
        }
        stream.push_str(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
            .mount(&server)
            .await;
        let uri = server.uri();
        let messages = msgs();
        let caveats = Caveats::top();
        let mut context = ctx(&uri, &messages, &caveats);
        context.action_nudges = false;
        if cap {
            context.max_tool_rounds = 0;
        }
        let result = chat_complete(context, &mut NoMcp).await;
        if cap {
            let (text, _, _, _) = result.unwrap();
            assert!(text.contains("repeated generation"), "{text}");
            assert!(text.contains("7 tokens"), "{text}");
        } else if repeated {
            let error = result.unwrap_err();
            assert!(error.to_string().contains("7 tokens"), "{error:#}");
            assert!(!error.to_string().contains("unavailable"), "{error:#}");
            assert!(
                error.to_string().contains("repeated generation"),
                "{error:#}"
            );
            assert_eq!(
                crate::retry::classify(&error),
                crate::retry::Retryability::Fatal
            );
        } else {
            assert!(result.is_ok(), "{result:?}");
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

/// #2782: the server cap never exceeds the remaining resolved context window.
#[test]
fn bounds_2782_cap_math_and_first_party_field() {
    let mut body = json!({"max_tokens":9999});
    let cap = generation_bounds::apply_chat(
        &mut body,
        "https://api.openai.com/v1/chat/completions",
        Some(1234),
        Some(1000),
        900,
    )
    .unwrap();
    assert_eq!(cap, 100);
    assert_eq!(body["max_completion_tokens"], 100);
    assert!(body.get("max_tokens").is_none());
    assert!(generation_bounds::output_cap(None, Some(1000), 1000).is_err());
    assert_eq!(
        generation_bounds::output_cap(Some(50_000), None, 0).unwrap(),
        16_384
    );
}
